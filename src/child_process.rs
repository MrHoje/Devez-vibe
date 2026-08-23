use tokio::process::Command;

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};

#[cfg(windows)]
const BACKEND_CREATION_FLAGS: u32 = CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW;

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

    #[cfg(windows)]
    #[test]
    fn backend_flags_isolate_console_and_ctrl_c() {
        use super::{BACKEND_CREATION_FLAGS, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};

        assert_ne!(BACKEND_CREATION_FLAGS & CREATE_NEW_PROCESS_GROUP, 0);
        assert_ne!(BACKEND_CREATION_FLAGS & CREATE_NO_WINDOW, 0);
    }
}
