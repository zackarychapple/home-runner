/// Keeps the machine from idle-sleeping while the supervisor runs. Dropping it restores normal sleep.
pub struct SleepGuard {
    #[cfg(target_os = "macos")]
    caffeinate: Option<std::process::Child>,
}

#[cfg(target_os = "macos")]
pub fn prevent_sleep() -> SleepGuard {
    // -w ties caffeinate's lifetime to ours, so a crash can't leave the Mac permanently awake.
    let caffeinate = std::process::Command::new("caffeinate")
        .args(["-i", "-s", "-w", &std::process::id().to_string()])
        .spawn()
        .map_err(|e| tracing::warn!("could not start caffeinate: {e}"))
        .ok();
    SleepGuard { caffeinate }
}

#[cfg(windows)]
pub fn prevent_sleep() -> SleepGuard {
    use windows_sys::Win32::System::Power::{
        ES_CONTINUOUS, ES_SYSTEM_REQUIRED, SetThreadExecutionState,
    };
    unsafe {
        SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED);
    }
    SleepGuard {}
}

#[cfg(not(any(target_os = "macos", windows)))]
pub fn prevent_sleep() -> SleepGuard {
    SleepGuard {}
}

impl Drop for SleepGuard {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        if let Some(child) = self.caffeinate.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::System::Power::SetThreadExecutionState(
                windows_sys::Win32::System::Power::ES_CONTINUOUS,
            );
        }
    }
}
