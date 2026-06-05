#[cfg(windows)]
use std::ffi::c_void;

#[cfg(windows)]
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod, TIMERR_NOERROR};
#[cfg(windows)]
use windows::Win32::System::Threading::{
    GetCurrentProcess, ProcessPowerThrottling, SetProcessInformation,
    PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
    PROCESS_POWER_THROTTLING_STATE,
};

/// 持有 Windows 高精度 timer resolution 请求。
///
/// compositor 目标 tick 是 8.333ms；默认 Windows timer resolution 约 15.6ms，
/// 会把 `tokio::time::interval` 实际限制到约 64Hz。Core / demo 在需要
/// 120Hz pacing 时持有这个 guard，Drop 时对称释放请求。
pub struct HighResolutionTimerGuard {
    #[cfg(windows)]
    period_ms: u32,
    #[cfg(windows)]
    active: bool,
}

impl HighResolutionTimerGuard {
    pub fn request_1ms(owner: &str) -> Self {
        #[cfg(windows)]
        {
            disable_ignore_timer_resolution(owner);

            let period_ms = 1;
            let result = unsafe { timeBeginPeriod(period_ms) };
            let active = result == TIMERR_NOERROR;
            if active {
                eprintln!("[timer] {owner}: requested {period_ms}ms Windows timer resolution");
            } else {
                eprintln!(
                    "[timer] {owner}: timeBeginPeriod({period_ms}) failed with MMRESULT={result}"
                );
            }
            Self { period_ms, active }
        }

        #[cfg(not(windows))]
        {
            let _ = owner;
            Self {}
        }
    }
}

#[cfg(windows)]
impl Drop for HighResolutionTimerGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let result = unsafe { timeEndPeriod(self.period_ms) };
        if result != TIMERR_NOERROR {
            eprintln!(
                "[timer] timeEndPeriod({}) failed with MMRESULT={}",
                self.period_ms, result
            );
        }
    }
}

#[cfg(windows)]
fn disable_ignore_timer_resolution(owner: &str) {
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
        StateMask: 0,
    };

    let result = unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            &state as *const PROCESS_POWER_THROTTLING_STATE as *const c_void,
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
    };

    if let Err(e) = result {
        eprintln!("[timer] {owner}: SetProcessInformation(ProcessPowerThrottling) failed: {e}");
    }
}
