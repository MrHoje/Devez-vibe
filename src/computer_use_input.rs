//! Physical Escape is a safety signal, not a replacement for terminal input.
//!
//! Only an explicitly armed Computer Use turn receives it. The hook never consumes
//! input, records keys, or treats an agent's injected Escape as a user interruption.

use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::sync::mpsc::UnboundedReceiver;

pub(crate) struct PhysicalEscape {
    arm: EscapeArmHandle,
    #[cfg(all(windows, not(test)))]
    _inner: platform::HookThread,
}

/// A cheap synchronous arm/disarm operation for the protocol reader.
/// Clones share only the generation, never ownership of the hook or its thread.
#[derive(Clone, Default)]
pub(crate) struct EscapeArmHandle {
    generation: Arc<AtomicU64>,
}

impl EscapeArmHandle {
    /// Zero disarms the hook. Receivers must reject generations no longer active.
    pub(crate) fn arm(&self, generation: u64) {
        self.generation.store(generation, Ordering::Release);
    }
}

impl PhysicalEscape {
    /// Start disarmed. Tests deliberately cannot install a desktop-wide hook.
    pub(crate) fn start() -> io::Result<(Self, UnboundedReceiver<u64>)> {
        #[cfg(all(windows, not(test)))]
        {
            let arm = EscapeArmHandle::default();
            let (inner, receiver) = platform::HookThread::start(Arc::clone(&arm.generation))?;
            Ok((Self { arm, _inner: inner }, receiver))
        }
        #[cfg(any(not(windows), test))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "physical Escape monitoring is only enabled on Windows outside tests",
            ))
        }
    }

    pub(crate) fn arm_handle(&self) -> EscapeArmHandle {
        self.arm.clone()
    }
}

#[cfg(any(windows, test))]
#[derive(Default)]
struct EscapeDebounce {
    pressed: bool,
}

#[cfg(any(windows, test))]
impl EscapeDebounce {
    fn observe(
        &mut self,
        is_escape: bool,
        injected: bool,
        lower_integrity_injected: bool,
        pressed: bool,
        generation: u64,
    ) -> Option<u64> {
        if !is_escape || injected || lower_integrity_injected {
            return None;
        }
        let was_pressed = self.pressed;
        self.pressed = pressed;
        (pressed && !was_pressed && generation != 0).then_some(generation)
    }
}

#[cfg(all(windows, not(test)))]
mod platform {
    use super::EscapeDebounce;
    use std::{
        cell::RefCell,
        io, ptr,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicU64, Ordering},
            mpsc,
        },
        thread::{self, JoinHandle},
    };
    use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
    use windows_sys::Win32::{
        Foundation::{LPARAM, LRESULT, WAIT_FAILED, WPARAM},
        System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
        UI::{
            Input::KeyboardAndMouse::VK_ESCAPE,
            WindowsAndMessaging::{
                CallNextHookEx, DispatchMessageW, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT,
                LLKHF_INJECTED, LLKHF_LOWER_IL_INJECTED, MSG, MWMO_INPUTAVAILABLE,
                MsgWaitForMultipleObjectsEx, PM_NOREMOVE, PM_REMOVE, PeekMessageW,
                PostThreadMessageW, QS_ALLINPUT, SetWindowsHookExW, TranslateMessage,
                UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN,
                WM_SYSKEYUP,
            },
        },
    };

    struct HookContext {
        generation: Arc<AtomicU64>,
        sender: UnboundedSender<u64>,
        escape: EscapeDebounce,
    }

    thread_local! {
        // WH_KEYBOARD_LL runs on the installing thread. Each server owns its own
        // thread and context; there is no process-global "current server" slot.
        static CONTEXT: RefCell<Option<HookContext>> = const { RefCell::new(None) };
    }

    pub(super) struct HookThread {
        generation: Arc<AtomicU64>,
        stopping: Arc<AtomicBool>,
        thread_id: Arc<Mutex<Option<u32>>>,
        worker: Option<JoinHandle<()>>,
    }

    impl HookThread {
        pub(super) fn start(
            generation: Arc<AtomicU64>,
        ) -> io::Result<(Self, UnboundedReceiver<u64>)> {
            let stopping = Arc::new(AtomicBool::new(false));
            let thread_id = Arc::new(Mutex::new(None));
            let (sender, receiver) = unbounded_channel();
            let (ready_tx, ready_rx) = mpsc::sync_channel(1);
            let worker = {
                let generation = Arc::clone(&generation);
                let stopping = Arc::clone(&stopping);
                let thread_id = Arc::clone(&thread_id);
                thread::Builder::new()
                    .name("computer-use-escape".into())
                    .spawn(move || {
                        run_hook(generation, stopping, thread_id, sender, ready_tx);
                    })?
            };
            match ready_rx.recv() {
                Ok(Ok(())) => Ok((
                    Self {
                        generation,
                        stopping,
                        thread_id,
                        worker: Some(worker),
                    },
                    receiver,
                )),
                result => {
                    // Setup errors never leave a detached hook thread behind.
                    let _ = worker.join();
                    Err(match result {
                        Ok(Err(error)) => error,
                        _ => io::Error::other("physical Escape hook stopped during setup"),
                    })
                }
            }
        }

        pub(super) fn arm(&self, generation: u64) {
            self.generation.store(generation, Ordering::Release);
        }
    }

    impl Drop for HookThread {
        fn drop(&mut self) {
            self.arm(0);
            self.stopping.store(true, Ordering::Release);
            {
                let id = self.thread_id.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(id) = *id {
                    // Setup acknowledged only after PeekMessage created the queue.
                    // The registration lock prevents posting to a recycled ID.
                    unsafe { PostThreadMessageW(id, WM_QUIT, 0, 0) };
                }
            }
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    struct ThreadRegistration(Arc<Mutex<Option<u32>>>);

    impl Drop for ThreadRegistration {
        fn drop(&mut self) {
            *self.0.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }

    struct InstalledHook(HHOOK);

    impl Drop for InstalledHook {
        fn drop(&mut self) {
            unsafe { UnhookWindowsHookEx(self.0) };
        }
    }

    fn run_hook(
        generation: Arc<AtomicU64>,
        stopping: Arc<AtomicBool>,
        thread_id: Arc<Mutex<Option<u32>>>,
        sender: UnboundedSender<u64>,
        ready: mpsc::SyncSender<io::Result<()>>,
    ) {
        let registration = ThreadRegistration(thread_id);
        *registration.0.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(unsafe { GetCurrentThreadId() });
        let mut message: MSG = unsafe { std::mem::zeroed() };
        // PostThreadMessage fails before a target thread has a message queue.
        unsafe { PeekMessageW(&mut message, ptr::null_mut(), 0, 0, PM_NOREMOVE) };
        CONTEXT.with(|context| {
            *context.borrow_mut() = Some(HookContext {
                generation,
                sender,
                escape: EscapeDebounce::default(),
            });
        });
        let module = unsafe { GetModuleHandleW(ptr::null()) };
        if module.is_null() {
            let _ = ready.send(Err(io::Error::last_os_error()));
            return;
        }
        let hook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(on_keyboard), module, 0) };
        if hook.is_null() {
            let _ = ready.send(Err(io::Error::last_os_error()));
            return;
        }
        let _hook = InstalledHook(hook);
        if ready.send(Ok(())).is_err() {
            return;
        }
        'messages: while !stopping.load(Ordering::Acquire) {
            while unsafe { PeekMessageW(&mut message, ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
                if message.message == WM_QUIT || stopping.load(Ordering::Acquire) {
                    break 'messages;
                }
                unsafe {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
            // Normally WM_QUIT wakes this immediately. A bounded wait also makes
            // Drop reliable if PostThreadMessage fails (for example, a full queue).
            let result = unsafe {
                MsgWaitForMultipleObjectsEx(0, ptr::null(), 250, QS_ALLINPUT, MWMO_INPUTAVAILABLE)
            };
            if result == WAIT_FAILED {
                break;
            }
        }
        // _hook is uninstalled before registration is removed and the thread's
        // context/sender are destroyed. Joining guarantees cleanup has completed.
    }

    unsafe extern "system" fn on_keyboard(code: i32, message: WPARAM, data: LPARAM) -> LRESULT {
        if code == HC_ACTION as i32 && data != 0 {
            let pressed = match message as u32 {
                WM_KEYDOWN | WM_SYSKEYDOWN => Some(true),
                WM_KEYUP | WM_SYSKEYUP => Some(false),
                _ => None,
            };
            if let Some(pressed) = pressed {
                let event = unsafe { &*(data as *const KBDLLHOOKSTRUCT) };
                // Do not retain or inspect other keys beyond this Escape filter.
                if event.vkCode == u32::from(VK_ESCAPE) {
                    let _ = CONTEXT.try_with(|context| {
                        let Ok(mut context) = context.try_borrow_mut() else {
                            return;
                        };
                        if let Some(context) = context.as_mut() {
                            let generation = context.generation.load(Ordering::Acquire);
                            if let Some(generation) = context.escape.observe(
                                true,
                                event.flags & LLKHF_INJECTED != 0,
                                event.flags & LLKHF_LOWER_IL_INJECTED != 0,
                                pressed,
                                generation,
                            ) {
                                let _ = context.sender.send(generation);
                            }
                        }
                    });
                }
            }
        }
        // Always forward, including physical Escape and negative hook codes.
        unsafe { CallNextHookEx(ptr::null_mut(), code, message, data) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builds_never_install_desktop_hooks() {
        assert!(matches!(
            PhysicalEscape::start(),
            Err(error) if error.kind() == io::ErrorKind::Unsupported
        ));
        PhysicalEscape {
            arm: EscapeArmHandle::default(),
        }
        .arm_handle()
        .arm(7);
    }

    #[test]
    fn cloned_arm_handles_update_only_their_own_guard_synchronously() {
        let first = PhysicalEscape {
            arm: EscapeArmHandle::default(),
        };
        let second = PhysicalEscape {
            arm: EscapeArmHandle::default(),
        };
        let arm = first.arm_handle();
        arm.arm(7);
        assert_eq!(first.arm.generation.load(Ordering::Acquire), 7);
        assert_eq!(second.arm.generation.load(Ordering::Acquire), 0);
        let clone = arm.clone();
        clone.arm(0);
        assert_eq!(first.arm.generation.load(Ordering::Acquire), 0);
        first.arm_handle().arm(8);
        assert_eq!(arm.generation.load(Ordering::Acquire), 8);
        // A remaining handle owns only this atomic, not the guard or its worker.
        drop(first);
        arm.arm(9);
        assert_eq!(clone.generation.load(Ordering::Acquire), 9);
    }

    #[test]
    fn only_physical_escape_presses_signal_the_armed_generation() {
        let mut escape = EscapeDebounce::default();
        assert_eq!(escape.observe(false, false, false, true, 7), None);
        assert_eq!(escape.observe(true, true, false, true, 7), None);
        assert_eq!(escape.observe(true, false, true, true, 7), None);
        assert_eq!(escape.observe(true, true, true, true, 7), None);
        assert_eq!(escape.observe(true, false, false, true, 7), Some(7));
    }

    #[test]
    fn repeats_require_a_physical_keyup_to_rearm() {
        let mut escape = EscapeDebounce::default();
        assert_eq!(escape.observe(true, false, false, false, 7), None);
        assert_eq!(escape.observe(true, false, false, true, 7), Some(7));
        assert_eq!(escape.observe(true, false, false, true, 7), None);
        assert_eq!(escape.observe(true, true, false, false, 7), None);
        assert_eq!(escape.observe(true, false, true, false, 7), None);
        assert_eq!(escape.observe(false, false, false, false, 7), None);
        assert_eq!(escape.observe(true, false, false, true, 7), None);
        assert_eq!(escape.observe(true, false, false, false, 7), None);
        assert_eq!(escape.observe(true, false, false, true, 7), Some(7));
    }

    #[test]
    fn holding_escape_across_disarm_and_new_turns_does_not_cancel_the_new_turn() {
        let mut escape = EscapeDebounce::default();
        assert_eq!(escape.observe(true, false, false, true, 7), Some(7));
        assert_eq!(escape.observe(true, false, false, true, 0), None);
        assert_eq!(escape.observe(true, false, false, true, 8), None);
        assert_eq!(escape.observe(true, false, false, false, 8), None);
        assert_eq!(escape.observe(true, false, false, true, 8), Some(8));
    }

    #[test]
    fn an_escape_held_while_disarmed_needs_a_new_press_after_arming() {
        let mut escape = EscapeDebounce::default();
        assert_eq!(escape.observe(true, false, false, true, 0), None);
        assert_eq!(escape.observe(true, false, false, true, 7), None);
        assert_eq!(escape.observe(true, false, false, false, 7), None);
        assert_eq!(escape.observe(true, false, false, true, 7), Some(7));
    }

    #[test]
    fn queued_signals_keep_the_generation_from_the_original_press() {
        let mut escape = EscapeDebounce::default();
        let old_signal = escape.observe(true, false, false, true, 7);
        assert_eq!(escape.observe(true, false, false, false, 8), None);
        let new_signal = escape.observe(true, false, false, true, 8);
        assert_eq!(old_signal, Some(7));
        assert_eq!(new_signal, Some(8));
    }
}
