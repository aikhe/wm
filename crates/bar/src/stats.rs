//! Native counters, sampled only while their widgets are enabled. No sysinfo
//! runtime, subprocesses or measurements on the WM thread.
use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::GetSystemTimes;

#[derive(Debug, Clone, Copy)]
struct CpuTimes {
    idle: u64,
    kernel: u64,
    user: u64,
}

impl CpuTimes {
    fn read() -> windows::core::Result<Self> {
        let (mut idle, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        unsafe {
            GetSystemTimes(
                Some(&raw mut idle),
                Some(&raw mut kernel),
                Some(&raw mut user),
            )?
        };
        let ticks =
            |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
        Ok(Self {
            idle: ticks(idle),
            kernel: ticks(kernel),
            user: ticks(user),
        })
    }

    fn usage_since(self, previous: Self) -> Option<u32> {
        // Kernel time already includes idle time. Adding idle again produces
        // misleadingly low utilization, especially on mostly idle machines.
        // https://learn.microsoft.com/windows/win32/api/processthreadsapi/nf-processthreadsapi-getsystemtimes
        let total = self
            .kernel
            .checked_sub(previous.kernel)?
            .checked_add(self.user.checked_sub(previous.user)?)?;
        let idle = self.idle.checked_sub(previous.idle)?;
        if total == 0 {
            return None;
        }
        Some(
            (total.saturating_sub(idle) as f64 * 100.0 / total as f64)
                .round()
                .clamp(0.0, 100.0) as u32,
        )
    }
}

#[derive(Default)]
pub struct CpuSampler {
    previous: Option<CpuTimes>,
}

impl CpuSampler {
    /// The first call primes the counter; it never waits for a second sample.
    /// On machines with >64 logical processors, GetSystemTimes covers the
    /// calling thread's primary processor group.
    pub fn sample(&mut self) -> windows::core::Result<Option<u32>> {
        let now = CpuTimes::read()?;
        Ok(self
            .previous
            .replace(now)
            .and_then(|previous| now.usage_since(previous)))
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MemoryUsage {
    pub used: u64,
    pub total: u64,
}

impl MemoryUsage {
    pub fn read() -> windows::core::Result<Self> {
        let mut status = MEMORYSTATUSEX {
            dwLength: size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        unsafe { GlobalMemoryStatusEx(&raw mut status)? };
        Ok(Self {
            used: status.ullTotalPhys.saturating_sub(status.ullAvailPhys),
            total: status.ullTotalPhys,
        })
    }

    pub fn format(self, format: &str) -> String {
        let gib = |bytes| format!("{:.1}", bytes as f64 / (1024.0 * 1024.0 * 1024.0));
        let percent = if self.total == 0 {
            "--".into()
        } else {
            format!(
                "{:.0}",
                (self.used as f64 * 100.0 / self.total as f64).clamp(0.0, 100.0)
            )
        };
        format
            .replace("{percent}", &percent)
            .replace("{used_gib}", &gib(self.used))
            .replace("{total_gib}", &gib(self.total))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_accounts_for_idle_inside_kernel_and_handles_reset_counters() {
        let before = CpuTimes {
            idle: 100,
            kernel: 200,
            user: 300,
        };
        assert_eq!(
            CpuTimes {
                idle: 160,
                kernel: 280,
                user: 320
            }
            .usage_since(before),
            Some(40)
        );
        assert_eq!(
            CpuTimes {
                idle: 200,
                kernel: 300,
                user: 300
            }
            .usage_since(before),
            Some(0)
        );
        assert_eq!(
            CpuTimes {
                idle: 100,
                kernel: 200,
                user: 400
            }
            .usage_since(before),
            Some(100)
        );
        assert_eq!(before.usage_since(before), None);
        assert_eq!(
            CpuTimes {
                idle: 0,
                kernel: 0,
                user: 0
            }
            .usage_since(before),
            None
        );
    }

    #[test]
    fn physical_memory_formats_used_total_and_percentage_consistently() {
        let usage = MemoryUsage {
            used: 6 * 1024 * 1024 * 1024,
            total: 16 * 1024 * 1024 * 1024,
        };
        assert_eq!(
            usage.format("{used_gib}/{total_gib} GiB ({percent}%)"),
            "6.0/16.0 GiB (38%)"
        );
        let real = MemoryUsage::read().unwrap();
        assert!(real.total > 0 && real.used <= real.total);
    }
}
