use tokio::process::Command;

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};

#[cfg(windows)]
const BACKEND_CREATION_FLAGS: u32 = CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW;

/// 백엔드가 다시 띄우는 MCP 서버 같은 손자 프로세스는 백엔드만 종료해서는 정리되지
/// 않는다. 백엔드마다 작업 개체를 하나 두고 이 값을 백엔드와 함께 버리면, 세션이나
/// 제공자만 바꿔 닫을 때도 커널이 트리째 종료한다. dvz가 어떤 경로로 끝나든 핸들이
/// 닫히므로 같은 정리가 일어난다.
#[cfg(windows)]
pub struct BackendJob(usize);

#[cfg(not(windows))]
pub struct BackendJob;

#[cfg(windows)]
impl Drop for BackendJob {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0 as _) };
    }
}

#[cfg(windows)]
pub fn adopt_backend(child: &tokio::process::Child) -> Option<BackendJob> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };

    let process = child.raw_handle()?;
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return None;
        }
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            std::ptr::from_ref(&limits).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if configured == 0 || AssignProcessToJobObject(job, process) == 0 {
            CloseHandle(job);
            return None;
        }
        Some(BackendJob(job as usize))
    }
}

#[cfg(not(windows))]
pub fn adopt_backend(_: &tokio::process::Child) -> Option<BackendJob> {
    None
}

pub fn isolate_backend(command: &mut Command) {
    #[cfg(windows)]
    command.creation_flags(BACKEND_CREATION_FLAGS);

    normalize_color_environment(
        command,
        std::env::var_os("FORCE_COLOR").is_some(),
        std::env::var_os("NO_COLOR").is_some(),
    );
}

/// `FORCE_COLOR` already wins over `NO_COLOR`; removing the ignored value
/// preserves that result and prevents Node from printing a warning about it.
fn normalize_color_environment(
    command: &mut Command,
    force_color_present: bool,
    no_color_present: bool,
) {
    if force_color_present && no_color_present {
        command.env_remove("NO_COLOR");
    }
}

/// Keep a fire-and-forget launcher from inheriting the active TUI console.
#[cfg(windows)]
pub fn isolate_launcher(command: &mut std::process::Command) {
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
pub fn isolate_launcher(_: &mut std::process::Command) {}

#[cfg(test)]
mod tests {
    use super::normalize_color_environment;

    #[test]
    fn force_color_removes_only_the_conflicting_no_color_value() {
        let mut conflicting = tokio::process::Command::new("backend");
        normalize_color_environment(&mut conflicting, true, true);
        assert!(
            conflicting
                .as_std()
                .get_envs()
                .any(|(key, value)| { key == std::ffi::OsStr::new("NO_COLOR") && value.is_none() })
        );

        let mut no_force_color = tokio::process::Command::new("backend");
        normalize_color_environment(&mut no_force_color, false, true);
        assert!(no_force_color.as_std().get_envs().next().is_none());
    }

    /// 백엔드가 다시 띄운 프로세스도 같은 작업 개체에 상속되는지 확인한다.
    #[cfg(windows)]
    #[tokio::test]
    async fn the_backend_and_what_it_spawns_share_one_job() {
        use windows_sys::Win32::System::JobObjects::{
            JOBOBJECT_BASIC_PROCESS_ID_LIST, JobObjectBasicProcessIdList,
            QueryInformationJobObject,
        };

        let mut child = spawn_backend_with_a_child();
        let job = super::adopt_backend(&child).expect("작업 개체 생성 실패");

        let mut assigned = 0;
        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            // 버퍼가 한 항목뿐이라 목록은 넘치지만 편입된 개수는 그대로 채워진다.
            let mut list: JOBOBJECT_BASIC_PROCESS_ID_LIST = unsafe { std::mem::zeroed() };
            unsafe {
                QueryInformationJobObject(
                    job.0 as _,
                    JobObjectBasicProcessIdList,
                    std::ptr::from_mut(&mut list).cast(),
                    size_of::<JOBOBJECT_BASIC_PROCESS_ID_LIST>() as u32,
                    std::ptr::null_mut(),
                )
            };
            assigned = list.NumberOfAssignedProcesses;
            if assigned >= 2 {
                break;
            }
        }
        let _ = child.kill().await;

        assert!(assigned >= 2, "손자 프로세스가 작업 개체에 없습니다: {assigned}");
    }

    /// 작업 개체를 버리면 세션만 닫아도 백엔드 트리가 끝난다.
    #[cfg(windows)]
    #[tokio::test]
    async fn dropping_the_job_ends_the_backend() {
        let mut child = spawn_backend_with_a_child();
        let job = super::adopt_backend(&child).expect("작업 개체 생성 실패");

        drop(job);

        let ended = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await;
        assert!(ended.is_ok(), "작업 개체를 닫았는데도 백엔드가 살아 있습니다.");
    }

    /// 자식(`cmd.exe`)이 손자(`ping`)를 띄우고 그동안 살아 있는 백엔드를 흉내 낸다.
    #[cfg(windows)]
    fn spawn_backend_with_a_child() -> tokio::process::Child {
        let mut command = tokio::process::Command::new("cmd.exe");
        command
            .args(["/c", "ping -n 60 127.0.0.1 >nul"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped());
        super::isolate_backend(&mut command);
        command.spawn().expect("테스트용 백엔드 실행 실패")
    }

    #[cfg(windows)]
    #[test]
    fn backend_flags_isolate_console_and_ctrl_c() {
        use super::{BACKEND_CREATION_FLAGS, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};

        assert_ne!(BACKEND_CREATION_FLAGS & CREATE_NEW_PROCESS_GROUP, 0);
        assert_ne!(BACKEND_CREATION_FLAGS & CREATE_NO_WINDOW, 0);
    }
}
