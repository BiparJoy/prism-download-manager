//! Operating-system abstractions: power actions, keep-awake, wake timers, shell commands.
//!
//! Every function has a Windows, Linux and macOS implementation (or an explicit
//! `Unsupported` error) so the crate builds everywhere.

use std::path::Path;
use std::process::Command;

#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    #[error("not supported on this platform")]
    Unsupported,
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("command failed: {0}")]
    Failed(String),
}

pub type PResult<T> = Result<T, PlatformError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerAction {
    Shutdown,
    Sleep,
    Hibernate,
    Lock,
}

/// Free bytes on the volume containing `path` (walks up to an existing ancestor).
/// The Linux desktop session as GTK and WebKit see it: whether Prism's windows draw through
/// Wayland or X11, the desktop, and the WebKit renderer variables a user set (#36, #44: empty
/// windows depended on them). For the log and the diagnostics zip; `None` on other systems.
/// Only names of sessions and desktops, nothing personal.
pub fn desktop_session() -> Option<serde_json::Value> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    Some(serde_json::json!({
        "sessionType": var("XDG_SESSION_TYPE"),
        "desktop": var("XDG_CURRENT_DESKTOP"),
        "wayland": var("WAYLAND_DISPLAY").is_some(),
        "x11": var("DISPLAY").is_some(),
        "gdkBackend": var("GDK_BACKEND"),
        "webkitDisableDmabufRenderer": var("WEBKIT_DISABLE_DMABUF_RENDERER"),
        "webkitDisableCompositingMode": var("WEBKIT_DISABLE_COMPOSITING_MODE"),
    }))
}

pub fn available_space(path: &Path) -> Option<u64> {
    let mut p = path;
    loop {
        if p.exists() {
            return fs4::available_space(p).ok();
        }
        p = p.parent()?;
    }
}

/// Run a user-supplied command line through the system shell (detached).
pub fn run_shell(command_line: &str) -> PResult<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        Command::new("cmd")
            .raw_arg("/C")
            .raw_arg(command_line)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()?;
    }
    #[cfg(not(windows))]
    {
        Command::new("sh").arg("-c").arg(command_line).spawn()?;
    }
    Ok(())
}

fn run(cmd: &str, args: &[&str]) -> PResult<()> {
    let status = Command::new(cmd).args(args).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(PlatformError::Failed(format!("{cmd} exited with {status}")))
    }
}

pub fn power(action: PowerAction) -> PResult<()> {
    imp::power(action)
}

/// Prevent the system from sleeping while downloads run (display may still sleep).
pub fn set_keep_awake(on: bool) {
    imp::set_keep_awake(on)
}

/// Arrange for the computer to wake from sleep at `unix_secs` (Windows wake timers).
pub fn schedule_wake(unix_secs: i64) -> PResult<()> {
    imp::schedule_wake(unix_secs)
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use windows_sys::Win32::System::Power::{
        ES_CONTINUOUS, ES_SYSTEM_REQUIRED, SetSuspendState, SetThreadExecutionState,
    };
    use windows_sys::Win32::System::Shutdown::LockWorkStation;

    pub fn power(action: PowerAction) -> PResult<()> {
        match action {
            // `shutdown.exe` handles privileges and gives the user 60 s to abort (`shutdown /a`).
            PowerAction::Shutdown => run(
                "shutdown",
                &[
                    "/s",
                    "/t",
                    "60",
                    "/c",
                    "Prism finished all downloads. Run 'shutdown /a' to cancel.",
                ],
            ),
            PowerAction::Sleep => suspend(false),
            PowerAction::Hibernate => suspend(true),
            PowerAction::Lock => {
                if unsafe { LockWorkStation() } != 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error().into())
                }
            }
        }
    }

    fn suspend(hibernate: bool) -> PResult<()> {
        let ok = unsafe { SetSuspendState(hibernate, false, false) };
        if ok {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error().into())
        }
    }

    // SetThreadExecutionState is per-thread, so a dedicated thread owns the request.
    static AWAKE: AtomicBool = AtomicBool::new(false);

    pub fn set_keep_awake(on: bool) {
        if AWAKE.swap(on, Ordering::SeqCst) == on {
            return;
        }
        if on {
            std::thread::spawn(|| {
                unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED) };
                while AWAKE.load(Ordering::SeqCst) {
                    std::thread::sleep(std::time::Duration::from_secs(5));
                }
                unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
            });
        }
    }

    pub fn schedule_wake(unix_secs: i64) -> PResult<()> {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            CreateWaitableTimerW, INFINITE, SetWaitableTimer, WaitForSingleObject,
        };
        // FILETIME: 100 ns intervals since 1601-01-01.
        let due: i64 = (unix_secs + 11_644_473_600) * 10_000_000;
        std::thread::spawn(move || unsafe {
            let timer = CreateWaitableTimerW(std::ptr::null(), 1, std::ptr::null());
            if timer.is_null() {
                return;
            }
            if SetWaitableTimer(timer, &due, 0, None, std::ptr::null(), 1) != 0 {
                WaitForSingleObject(timer, INFINITE);
            }
            CloseHandle(timer);
        });
        Ok(())
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use std::sync::Mutex;

    pub fn power(action: PowerAction) -> PResult<()> {
        match action {
            PowerAction::Shutdown => run("systemctl", &["poweroff"]),
            PowerAction::Sleep => run("systemctl", &["suspend"]),
            PowerAction::Hibernate => run("systemctl", &["hibernate"]),
            PowerAction::Lock => run("loginctl", &["lock-session"]),
        }
    }

    static INHIBITOR: Mutex<Option<std::process::Child>> = Mutex::new(None);

    pub fn set_keep_awake(on: bool) {
        let mut guard = INHIBITOR.lock().unwrap_or_else(|e| e.into_inner());
        match (on, guard.is_some()) {
            (true, false) => {
                // The lock is held by `tail --pid`, which ends with Prism: a crashed or killed
                // Prism doesn't leave the computer unable to sleep (`sleep infinity` did).
                let pid = std::process::id().to_string();
                *guard = Command::new("systemd-inhibit")
                    .args([
                        "--what=sleep",
                        "--who=Prism",
                        "--why=Downloads in progress",
                        "tail",
                        "--pid",
                        &pid,
                        "-f",
                        "/dev/null",
                    ])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .ok();
            }
            (false, true) => {
                if let Some(mut child) = guard.take() {
                    // Ending `tail` ends systemd-inhibit and its lock; killing only
                    // systemd-inhibit would leave `tail` running until Prism quits.
                    let _ = Command::new("pkill")
                        .args(["-P", &child.id().to_string()])
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status();
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
            _ => {}
        }
    }

    pub fn schedule_wake(_unix_secs: i64) -> PResult<()> {
        // rtcwake requires root; not attempted.
        Err(PlatformError::Unsupported)
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use std::sync::Mutex;

    pub fn power(action: PowerAction) -> PResult<()> {
        match action {
            PowerAction::Shutdown => run(
                "osascript",
                &["-e", "tell application \"System Events\" to shut down"],
            ),
            PowerAction::Sleep | PowerAction::Hibernate => run("pmset", &["sleepnow"]),
            PowerAction::Lock => run("pmset", &["displaysleepnow"]),
        }
    }

    static CAFFEINATE: Mutex<Option<std::process::Child>> = Mutex::new(None);

    pub fn set_keep_awake(on: bool) {
        let mut guard = CAFFEINATE.lock().unwrap_or_else(|e| e.into_inner());
        match (on, guard.is_some()) {
            // `-w`: caffeinate ends with Prism, so a crashed Prism doesn't keep the Mac awake.
            (true, false) => {
                *guard = Command::new("caffeinate")
                    .args(["-i", "-w", &std::process::id().to_string()])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .ok()
            }
            (false, true) => {
                if let Some(mut child) = guard.take() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
            _ => {}
        }
    }

    pub fn schedule_wake(_unix_secs: i64) -> PResult<()> {
        // `pmset schedule wake` requires administrator rights.
        Err(PlatformError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn free_space_of_temp_dir() {
        let space = super::available_space(&std::env::temp_dir().join("does/not/exist"));
        assert!(space.unwrap_or(0) > 0);
    }
}
