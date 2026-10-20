#[cfg(any(windows, test))]
use std::time::Duration;
use std::{
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

// AppServer declares this after its job handle, so recovery also runs after
// owned helpers are terminated during provider changes, shutdown, or unwinding.
pub struct RestoreOnDrop {
    enabled: Arc<AtomicBool>,
}

impl RestoreOnDrop {
    pub fn new(enabled: Arc<AtomicBool>) -> Self {
        Self { enabled }
    }
}

impl Drop for RestoreOnDrop {
    fn drop(&mut self) {
        // Configuration may acquire Computer Use after this guard is created.
        // Consult the live capability flag, not the initial config snapshot.
        if self.enabled.load(Ordering::Acquire) {
            // Closing a Windows job starts asynchronous process termination.
            // Give its overlay a short chance to disappear, even without a live
            // Tokio runtime. Do not leave a detached thread resetting cursors.
            #[cfg(all(windows, not(test)))]
            restore_on_drop_with(windows::restore_if_idle, std::thread::sleep);
        }
    }
}

#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Observation {
    Idle,
    Deferred,
    Restored,
    Cancelled,
}

// Turn completion and native helper teardown are different events. In
// particular, an initially normal cursor is not proof that teardown is done.
#[cfg(any(windows, test))]
const TURN_PROBE_MILLIS: [u64; 8] = [0, 100, 250, 500, 1_000, 2_000, 3_000, 5_000];
#[cfg(any(windows, test))]
const MIN_SETTLE_MILLIS: u64 = 1_000;

pub async fn restore_after_turn() -> io::Result<()> {
    #[cfg(all(windows, not(test)))]
    return restore_after_turn_with(windows::restore_if_idle).await;
    #[cfg(any(not(windows), test))]
    Ok(())
}

/// Serialize each complete native probe and cursor reload with the host's next
/// Computer Use start. Aborting an async task alone cannot stop a synchronous
/// probe already running between EnumWindows and SystemParametersInfoW.
pub async fn restore_after_turn_guarded(gate: Arc<Mutex<bool>>) -> io::Result<()> {
    #[cfg(all(windows, not(test)))]
    return restore_after_turn_guarded_with(&gate, windows::restore_if_idle).await;
    #[cfg(any(not(windows), test))]
    {
        let _ = gate;
        Ok(())
    }
}

#[cfg(any(windows, test))]
async fn restore_after_turn_guarded_with(
    gate: &Mutex<bool>,
    mut probe: impl FnMut() -> io::Result<Observation>,
) -> io::Result<()> {
    restore_after_turn_with(|| {
        // A poisoned gate cannot prove the desktop is still ours to repair.
        let Ok(enabled) = gate.lock() else {
            return Ok(Observation::Cancelled);
        };
        if !*enabled {
            return Ok(Observation::Cancelled);
        }
        // Keep the lock through the entire probe, including the native reload.
        // The host must close this gate before starting a new CU operation.
        let result = probe();
        drop(enabled);
        result
    })
    .await
}

#[cfg(any(windows, test))]
async fn restore_after_turn_with(
    mut probe: impl FnMut() -> io::Result<Observation>,
) -> io::Result<()> {
    let started = tokio::time::Instant::now();
    let mut last_error = None;
    for millis in TURN_PROBE_MILLIS {
        tokio::time::sleep_until(started + Duration::from_millis(millis)).await;
        match probe() {
            Ok(Observation::Restored | Observation::Cancelled) => return Ok(()),
            Ok(Observation::Idle) if millis >= MIN_SETTLE_MILLIS => return Ok(()),
            Ok(_) => last_error = None,
            // A transient desktop/enumeration failure must not discard the
            // remaining cleanup attempts. Only report a still-unresolved error.
            Err(error) => last_error = Some(error),
        }
    }
    // An active helper may belong to the next turn or another client. Deferring
    // to it is intentional and is not a restoration failure or timeout.
    last_error.map_or(Ok(()), Err)
}

#[cfg(any(windows, test))]
fn restore_on_drop_with(
    mut probe: impl FnMut() -> io::Result<Observation>,
    mut sleep: impl FnMut(Duration),
) {
    let mut previous = 0;
    for millis in [0, 25, 75, 150, 250] {
        if millis != 0 {
            sleep(Duration::from_millis(millis - previous));
        }
        if matches!(probe(), Ok(Observation::Restored)) {
            break;
        }
        previous = millis;
    }
}

/// Returns true only when a blank system cursor is still protected by a
/// visible helper. Never changes ShowCursor counters or closes helper windows.
pub fn restore_if_idle() -> io::Result<bool> {
    // Unit tests may inspect private, off-screen cursor bitmaps, but must never
    // reload the desktop cursor scheme as a side effect of app-server tests.
    #[cfg(all(windows, not(test)))]
    return windows::restore_if_idle().map(|state| state == Observation::Deferred);
    #[cfg(any(not(windows), test))]
    Ok(false)
}

#[cfg(windows)]
mod windows {
    use super::Observation;
    use std::{io, mem::size_of, ptr};

    use windows_sys::Win32::{
        Foundation::{CloseHandle, HWND, LPARAM},
        Graphics::Gdi::{
            BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection,
            DIB_RGB_COLORS, DeleteDC, DeleteObject, GdiFlush, GetObjectW, SelectObject,
        },
        System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
        },
        UI::WindowsAndMessaging::{
            CURSORINFO, DI_NORMAL, DrawIconEx, EnumWindows, GetClassNameW, GetCursorInfo,
            GetIconInfo, GetWindowThreadProcessId, HCURSOR, ICONINFO, IDC_APPSTARTING, IDC_ARROW,
            IDC_CROSS, IDC_HAND, IDC_HELP, IDC_IBEAM, IDC_NO, IDC_SIZEALL, IDC_SIZENESW,
            IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE, IDC_UPARROW, IDC_WAIT, IsWindowVisible,
            LoadCursorW, SPI_SETCURSORS, SystemParametersInfoW,
        },
    };

    pub(super) fn restore_if_idle() -> io::Result<Observation> {
        unsafe {
            if !has_blank_system_cursor() {
                return Ok(Observation::Idle);
            }
            let mut active = false;
            if EnumWindows(
                Some(find_helper_overlay),
                ptr::from_mut(&mut active) as LPARAM,
            ) == 0
                && !active
            {
                return Err(io::Error::last_os_error());
            }
            if active {
                return Ok(Observation::Deferred);
            }
            // Reload the user's current scheme; do not replace it with a default
            // arrow or change ShowCursor's per-thread display counter.
            if SystemParametersInfoW(SPI_SETCURSORS, 0, ptr::null_mut(), 0) == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Observation::Restored)
    }

    fn has_blank_system_cursor() -> bool {
        unsafe {
            // Helpers can replace busy/resize cursors as well as the arrow.
            // LoadCursor retrieves the shared system resources. Inspect the
            // current one first, but never reset the global scheme solely
            // because an unrelated application selected a private blank cursor
            // or Windows suppressed it for touch/pen input.
            let cursors = [
                IDC_ARROW,
                IDC_IBEAM,
                IDC_HAND,
                IDC_WAIT,
                IDC_CROSS,
                IDC_UPARROW,
                IDC_SIZENWSE,
                IDC_SIZENESW,
                IDC_SIZEWE,
                IDC_SIZENS,
                IDC_SIZEALL,
                IDC_NO,
                IDC_APPSTARTING,
                IDC_HELP,
            ]
            .map(|id| LoadCursorW(ptr::null_mut(), id));
            let mut current = CURSORINFO {
                cbSize: size_of::<CURSORINFO>() as u32,
                ..Default::default()
            };
            let current = if GetCursorInfo(&mut current) != 0 {
                current.hCursor
            } else {
                ptr::null_mut()
            };
            if !current.is_null() && cursors.contains(&current) && is_blank(current) {
                return true;
            }
            cursors
                .into_iter()
                .any(|cursor| !cursor.is_null() && cursor != current && is_blank(cursor))
        }
    }

    unsafe extern "system" fn find_helper_overlay(window: HWND, state: LPARAM) -> i32 {
        unsafe {
            if IsWindowVisible(window) == 0 {
                return 1;
            }
            let mut class = [0; 256];
            let length = GetClassNameW(window, class.as_mut_ptr(), class.len() as i32);
            let mut helper = String::from_utf16_lossy(&class[..length.max(0) as usize])
                .starts_with("CodexComputerUse");
            if !helper {
                let mut pid = 0;
                GetWindowThreadProcessId(window, &mut pid);
                let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
                if !process.is_null() {
                    let mut path = [0; 32768];
                    let mut length = path.len() as u32;
                    if QueryFullProcessImageNameW(process, 0, path.as_mut_ptr(), &mut length) != 0 {
                        let path = String::from_utf16_lossy(&path[..length as usize]);
                        helper = matches!(
                            path.rsplit('\\')
                                .next()
                                .unwrap_or("")
                                .to_ascii_lowercase()
                                .as_str(),
                            "codex-computer-use.exe"
                                | "codex-computer-use-arm64.exe"
                                | "codex-computer-use-swift.exe"
                        );
                    }
                    CloseHandle(process);
                }
            }
            if helper {
                *(state as *mut bool) = true;
                return 0;
            }
            1
        }
    }

    /// Draw onto black and white off-screen surfaces. CURSOR_SHOWING and a
    /// non-null handle can both describe an entirely transparent cursor.
    fn is_blank(cursor: HCURSOR) -> bool {
        unsafe {
            let mut icon = ICONINFO::default();
            if GetIconInfo(cursor, &mut icon) == 0 {
                return false;
            }
            let mut bitmap = BITMAP::default();
            let monochrome = icon.hbmColor.is_null();
            let source = if monochrome {
                icon.hbmMask
            } else {
                icon.hbmColor
            };
            let read = GetObjectW(
                source,
                size_of::<BITMAP>() as i32,
                ptr::from_mut(&mut bitmap).cast(),
            );
            if !icon.hbmMask.is_null() {
                DeleteObject(icon.hbmMask);
            }
            if !icon.hbmColor.is_null() {
                DeleteObject(icon.hbmColor);
            }
            let width = bitmap.bmWidth;
            let height = if monochrome {
                bitmap.bmHeight / 2
            } else {
                bitmap.bmHeight
            };
            if read == 0 || width <= 0 || height <= 0 || width > 1024 || height > 1024 {
                return false;
            }
            let dc = CreateCompatibleDC(ptr::null_mut());
            if dc.is_null() {
                return false;
            }
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = ptr::null_mut();
            let surface =
                CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, ptr::null_mut(), 0);
            let mut blank = false;
            if !surface.is_null() && !bits.is_null() {
                let previous = SelectObject(dc, surface);
                if !previous.is_null() && previous as isize != -1 {
                    let count = (width * height) as usize;
                    blank = [0, 0x00ff_ffff].into_iter().all(|background| {
                        std::slice::from_raw_parts_mut(bits.cast::<u32>(), count).fill(background);
                        DrawIconEx(
                            dc,
                            0,
                            0,
                            cursor,
                            width,
                            height,
                            0,
                            ptr::null_mut(),
                            DI_NORMAL,
                        ) != 0
                            && GdiFlush() != 0
                            && std::slice::from_raw_parts(bits.cast::<u32>(), count)
                                .iter()
                                .all(|pixel| pixel & 0x00ff_ffff == background)
                    });
                    SelectObject(dc, previous);
                }
            }
            if !surface.is_null() {
                DeleteObject(surface);
            }
            DeleteDC(dc);
            blank
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use windows_sys::Win32::{
            Graphics::Gdi::CreateBitmap,
            UI::WindowsAndMessaging::{CreateCursor, CreateIconIndirect, DestroyCursor},
        };

        #[test]
        fn transparent_and_visible_monochrome_cursors_are_distinguished_off_screen() {
            for (and, xor, blank) in [(255u8, 0u8, true), (0, 0, false), (255, 255, false)] {
                let and = [and; 128];
                let xor = [xor; 128];
                unsafe {
                    let cursor = CreateCursor(
                        ptr::null_mut(),
                        0,
                        0,
                        32,
                        32,
                        and.as_ptr().cast(),
                        xor.as_ptr().cast(),
                    );
                    assert!(!cursor.is_null());
                    let actual = is_blank(cursor);
                    DestroyCursor(cursor);
                    assert_eq!(actual, blank);
                }
            }
        }

        #[test]
        fn transparent_color_and_opaque_black_cursors_are_distinguished_off_screen() {
            for (color, blank) in [(0u32, true), (0xff00_0000, false), (0xffff_ffff, false)] {
                unsafe {
                    let pixels = [color; 1024];
                    let mask = [255u8; 128];
                    let info = ICONINFO {
                        hbmColor: CreateBitmap(32, 32, 1, 32, pixels.as_ptr().cast()),
                        hbmMask: CreateBitmap(32, 32, 1, 1, mask.as_ptr().cast()),
                        ..Default::default()
                    };
                    let cursor = CreateIconIndirect(&info);
                    DeleteObject(info.hbmColor);
                    DeleteObject(info.hbmMask);
                    assert!(!cursor.is_null());
                    let actual = is_blank(cursor);
                    DestroyCursor(cursor);
                    assert_eq!(actual, blank, "color: {color:08x}");
                }
            }
        }

        #[test]
        fn invalid_cursor_is_never_treated_as_transparent() {
            assert!(!is_blank(ptr::null_mut()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use tokio::time::Instant;

    #[tokio::test(start_paused = true)]
    async fn initially_normal_cursor_does_not_end_the_settle_window() {
        let started = Instant::now();
        let mut restored = false;
        restore_after_turn_with(|| {
            if started.elapsed() >= Duration::from_millis(750) {
                restored = true;
                Ok(Observation::Restored)
            } else {
                Ok(Observation::Idle)
            }
        })
        .await
        .unwrap();
        assert!(restored, "late helper teardown was not observed");
        assert_eq!(started.elapsed(), Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn helper_that_releases_after_the_old_retry_window_is_recovered() {
        let started = Instant::now();
        let mut restored = false;
        restore_after_turn_with(|| {
            if started.elapsed() >= Duration::from_secs(2) {
                restored = true;
                Ok(Observation::Restored)
            } else {
                Ok(Observation::Deferred)
            }
        })
        .await
        .unwrap();
        assert!(restored);
        assert_eq!(started.elapsed(), Duration::from_secs(2));
    }

    #[tokio::test(start_paused = true)]
    async fn active_helper_is_deferred_without_a_false_timeout_warning() {
        let started = Instant::now();
        let mut probes = 0;
        restore_after_turn_with(|| {
            probes += 1;
            Ok(Observation::Deferred)
        })
        .await
        .unwrap();
        assert_eq!(probes, TURN_PROBE_MILLIS.len());
        assert_eq!(started.elapsed(), Duration::from_secs(5));
    }

    #[tokio::test(start_paused = true)]
    async fn normal_desktop_finishes_after_the_minimum_settle_window() {
        let started = Instant::now();
        let mut probes = 0;
        restore_after_turn_with(|| {
            probes += 1;
            Ok(Observation::Idle)
        })
        .await
        .unwrap();
        assert_eq!(probes, 5);
        assert_eq!(started.elapsed(), Duration::from_millis(MIN_SETTLE_MILLIS));
    }

    #[tokio::test(start_paused = true)]
    async fn transient_native_errors_do_not_discard_later_recovery_attempts() {
        let mut probes = 0;
        restore_after_turn_with(|| {
            probes += 1;
            if probes < 4 {
                Err(io::Error::other("desktop is switching"))
            } else {
                Ok(Observation::Restored)
            }
        })
        .await
        .unwrap();
        assert_eq!(probes, 4);
    }

    #[tokio::test(start_paused = true)]
    async fn unresolved_native_error_is_reported_after_bounded_retries() {
        let started = Instant::now();
        let error = restore_after_turn_with(|| Err(io::Error::other("enumeration failed")))
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "enumeration failed");
        assert_eq!(started.elapsed(), Duration::from_secs(5));
    }

    #[tokio::test(start_paused = true)]
    async fn restoration_finishes_without_repeated_global_resets() {
        let mut probes = 0;
        restore_after_turn_with(|| {
            probes += 1;
            Ok(Observation::Restored)
        })
        .await
        .unwrap();
        assert_eq!(probes, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn cancelling_recovery_cancels_all_future_probes() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let probes = Arc::new(AtomicUsize::new(0));
        let worker_probes = probes.clone();
        let worker = tokio::spawn(restore_after_turn_with(move || {
            worker_probes.fetch_add(1, Ordering::Relaxed);
            Ok(Observation::Deferred)
        }));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(100)).await;
        tokio::task::yield_now().await;
        worker.abort();
        assert!(worker.await.unwrap_err().is_cancelled());
        let before = probes.load(Ordering::Relaxed);
        assert!(before > 0);
        tokio::time::advance(Duration::from_secs(10)).await;
        assert_eq!(probes.load(Ordering::Relaxed), before);
    }

    #[tokio::test(start_paused = true)]
    async fn closed_gate_skips_the_first_probe_and_stops_immediately() {
        let gate = Mutex::new(false);
        let started = Instant::now();
        restore_after_turn_guarded_with(&gate, || panic!("closed gate must not probe"))
            .await
            .unwrap();
        assert_eq!(started.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn closing_gate_between_attempts_prevents_all_later_probes() {
        use std::sync::atomic::AtomicUsize;
        let gate = Arc::new(Mutex::new(true));
        let probes = Arc::new(AtomicUsize::new(0));
        let worker_gate = gate.clone();
        let worker_probes = probes.clone();
        let worker = tokio::spawn(async move {
            restore_after_turn_guarded_with(&worker_gate, || {
                worker_probes.fetch_add(1, Ordering::Relaxed);
                Ok(Observation::Deferred)
            })
            .await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(100)).await;
        tokio::task::yield_now().await;
        let before = probes.load(Ordering::Relaxed);
        assert!(before > 0);
        // This also verifies that recovery does not hold the gate while asleep.
        *gate.try_lock().expect("gate held across recovery sleep") = false;
        worker.await.unwrap().unwrap();
        assert_eq!(probes.load(Ordering::Relaxed), before);
    }

    #[tokio::test(start_paused = true)]
    async fn gate_is_held_through_the_entire_probe_and_released_afterward() {
        let gate = Mutex::new(true);
        restore_after_turn_guarded_with(&gate, || {
            assert!(matches!(
                gate.try_lock(),
                Err(std::sync::TryLockError::WouldBlock)
            ));
            // Represents the mutation at the end of the native probe.
            assert!(matches!(
                gate.try_lock(),
                Err(std::sync::TryLockError::WouldBlock)
            ));
            Ok(Observation::Restored)
        })
        .await
        .unwrap();
        assert!(gate.try_lock().is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn native_probe_errors_do_not_leak_the_gate_lock() {
        let gate = Mutex::new(true);
        let mut probes = 0;
        restore_after_turn_guarded_with(&gate, || {
            probes += 1;
            assert!(matches!(
                gate.try_lock(),
                Err(std::sync::TryLockError::WouldBlock)
            ));
            if probes == 1 {
                Err(io::Error::other("transient native failure"))
            } else {
                Ok(Observation::Restored)
            }
        })
        .await
        .unwrap();
        assert_eq!(probes, 2, "retry must be able to reacquire the gate");
        assert!(gate.try_lock().is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn poisoned_gate_fails_closed_without_a_native_probe() {
        let gate = Mutex::new(true);
        let _ = std::panic::catch_unwind(|| {
            let _guard = gate.lock().unwrap();
            panic!("simulate owner panic");
        });
        assert!(gate.is_poisoned());
        restore_after_turn_guarded_with(&gate, || panic!("poisoned gate must not probe"))
            .await
            .unwrap();
    }

    #[test]
    fn drop_waits_for_delayed_cleanup_without_a_runtime() {
        let elapsed = Cell::new(Duration::ZERO);
        let mut restored = false;
        restore_on_drop_with(
            || {
                if elapsed.get() >= Duration::from_millis(75) {
                    restored = true;
                    Ok(Observation::Restored)
                } else {
                    Ok(Observation::Idle)
                }
            },
            |delay| elapsed.set(elapsed.get() + delay),
        );
        assert!(restored);
        assert_eq!(elapsed.get(), Duration::from_millis(75));
    }

    #[test]
    fn drop_never_waits_indefinitely_for_an_unrelated_helper() {
        let mut elapsed = Duration::ZERO;
        let mut probes = 0;
        restore_on_drop_with(
            || {
                probes += 1;
                Ok(Observation::Deferred)
            },
            |delay| elapsed += delay,
        );
        assert_eq!(probes, 5);
        assert_eq!(elapsed, Duration::from_millis(250));
    }
}
