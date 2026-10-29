//! Read-only platform measurements. Missing counters remain unknown; a cgroup's
//! scope is explicit and is never inferred to be exclusive to one task.
use serde::{Deserialize, Serialize};
#[cfg(target_os = "linux")]
use std::collections::BTreeMap;
use std::path::PathBuf;
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResourceScope {
    TaskChildren,
    TaskContainer,
    #[default]
    Shared,
}
#[derive(Clone)]
pub struct ResourceConfig {
    pub process_id: u32,
    pub cgroup: Option<PathBuf>,
    pub cgroup_scope: ResourceScope,
    /// Host-owned procfs adaptation, overridable with synthetic fixtures.
    pub proc_root: PathBuf,
    #[cfg(windows)]
    pub windows_job: Option<std::sync::Arc<std::os::windows::io::OwnedHandle>>,
}
impl Default for ResourceConfig {
    fn default() -> Self {
        Self {
            process_id: std::process::id(),
            cgroup: None,
            cgroup_scope: ResourceScope::Shared,
            proc_root: "/proc".into(),
            #[cfg(windows)]
            windows_job: None,
        }
    }
}
impl std::fmt::Debug for ResourceConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResourceConfig")
            .field("process_id", &self.process_id)
            .field("cgroup_scope", &self.cgroup_scope)
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ResourceSample {
    pub timestamp_unix_ms: i64,
    pub process_id: u32,
    pub platform: String,
    pub daemon_rss_bytes: Option<u64>,
    pub daemon_lifetime_peak_bytes: Option<u64>,
    pub daemon_cpu_ns: Option<u64>,
    pub cgroup_scope: ResourceScope,
    pub group_memory_current_bytes: Option<u64>,
    pub group_memory_peak_bytes: Option<u64>,
    pub group_cpu_ns: Option<u64>,
    pub group_pids_current: Option<u64>,
    pub group_pids_peak: Option<u64>,
    pub group_oom_events: Option<u64>,
    pub group_oom_kills: Option<u64>,
    pub group_process_lifetime_peak_bytes: Option<u64>,
    pub unavailable: Vec<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ResourceSummary {
    pub sample_count: u64,
    pub first_sample_unix_ms: Option<i64>,
    pub last_sample_unix_ms: Option<i64>,
    pub daemon_rss_initial_bytes: Option<u64>,
    pub daemon_rss_observed_peak_bytes: Option<u64>,
    pub daemon_rss_delta_peak_bytes: Option<u64>,
    pub daemon_lifetime_peak_bytes: Option<u64>,
    pub daemon_cpu_elapsed_ns: Option<u64>,
    pub group_scope: ResourceScope,
    pub group_memory_peak_bytes: Option<u64>,
    pub group_pids_peak: Option<u64>,
    pub group_pids_observed_peak: Option<u64>,
    pub group_cpu_elapsed_ns: Option<u64>,
    pub group_process_lifetime_peak_bytes: Option<u64>,
    pub group_oom_events_delta: Option<u64>,
    pub group_oom_kills_delta: Option<u64>,
    pub unavailable: Vec<String>,
}
fn maximum(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}
impl ResourceSummary {
    pub fn from_samples(samples: &[ResourceSample]) -> Self {
        let Some(first) = samples.first() else {
            return Self::default();
        };
        let last = samples.last().unwrap();
        let mut s = Self {
            sample_count: samples.len() as u64,
            first_sample_unix_ms: Some(first.timestamp_unix_ms),
            last_sample_unix_ms: Some(last.timestamp_unix_ms),
            daemon_rss_initial_bytes: first.daemon_rss_bytes,
            group_scope: first.cgroup_scope,
            ..Default::default()
        };
        for sample in samples {
            s.group_process_lifetime_peak_bytes = maximum(
                s.group_process_lifetime_peak_bytes,
                sample.group_process_lifetime_peak_bytes,
            );
            s.daemon_rss_observed_peak_bytes =
                maximum(s.daemon_rss_observed_peak_bytes, sample.daemon_rss_bytes);
            s.daemon_lifetime_peak_bytes = maximum(
                s.daemon_lifetime_peak_bytes,
                sample.daemon_lifetime_peak_bytes,
            );
            s.group_memory_peak_bytes =
                maximum(s.group_memory_peak_bytes, sample.group_memory_peak_bytes);
            s.group_pids_peak = maximum(s.group_pids_peak, sample.group_pids_peak);
            s.group_pids_observed_peak =
                maximum(s.group_pids_observed_peak, sample.group_pids_current);
            s.unavailable.extend(sample.unavailable.iter().cloned());
        }
        s.daemon_rss_delta_peak_bytes = s
            .daemon_rss_observed_peak_bytes
            .zip(s.daemon_rss_initial_bytes)
            .map(|(peak, initial)| peak.saturating_sub(initial));
        s.daemon_cpu_elapsed_ns = last
            .daemon_cpu_ns
            .zip(first.daemon_cpu_ns)
            .map(|(last, first)| last.saturating_sub(first));
        s.group_cpu_elapsed_ns = last
            .group_cpu_ns
            .zip(first.group_cpu_ns)
            .map(|(last, first)| last.saturating_sub(first));
        s.group_oom_events_delta = last
            .group_oom_events
            .zip(first.group_oom_events)
            .map(|(last, first)| last.saturating_sub(first));
        s.group_oom_kills_delta = last
            .group_oom_kills
            .zip(first.group_oom_kills)
            .map(|(last, first)| last.saturating_sub(first));
        s.unavailable.sort();
        s.unavailable.dedup();
        s
    }
}
pub fn sample(config: &ResourceConfig, timestamp_unix_ms: i64) -> ResourceSample {
    let mut sample = ResourceSample {
        timestamp_unix_ms,
        process_id: config.process_id,
        platform: std::env::consts::OS.into(),
        cgroup_scope: config.cgroup_scope,
        ..Default::default()
    };
    platform(config, &mut sample);
    sample
}
#[cfg(target_os = "linux")]
fn values(text: &str) -> BTreeMap<String, u64> {
    text.lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            Some((
                parts.next()?.trim_end_matches(':').to_owned(),
                parts.next()?.parse().ok()?,
            ))
        })
        .collect()
}
#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn ticks_to_ns(ticks: u64, numerator: u64, denominator: u64) -> Option<u64> {
    (denominator != 0).then(|| {
        ((ticks as u128 * numerator as u128) / denominator as u128).min(u64::MAX as u128) as u64
    })
}
#[cfg(target_os = "linux")]
fn platform(config: &ResourceConfig, s: &mut ResourceSample) {
    let process = config.proc_root.join(config.process_id.to_string());
    if let Ok(status) = std::fs::read_to_string(process.join("status")) {
        let v = values(&status);
        s.daemon_rss_bytes = v.get("VmRSS").map(|v| v.saturating_mul(1024));
        s.daemon_lifetime_peak_bytes = v.get("VmHWM").map(|v| v.saturating_mul(1024));
    } else {
        s.unavailable.push("process.status".into());
    }
    if let Ok(stat) = std::fs::read_to_string(process.join("stat"))
        && let Some((_, tail)) = stat.rsplit_once(") ")
    {
        let fields = tail.split_whitespace().collect::<Vec<_>>();
        let ticks = fields
            .get(11)
            .and_then(|v| v.parse::<u64>().ok())
            .zip(fields.get(12).and_then(|v| v.parse::<u64>().ok()))
            .map(|(a, b)| a.saturating_add(b));
        let rate = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        if rate > 0 {
            s.daemon_cpu_ns = ticks.and_then(|v| ticks_to_ns(v, 1_000_000_000, rate as u64));
        }
    }
    let Some(group) = &config.cgroup else {
        s.unavailable
            .push("task cgroup not configured; daemon RSS is shared".into());
        return;
    };
    let mut read = |name: &str| -> Option<String> {
        match std::fs::read_to_string(group.join(name)) {
            Ok(v) => Some(v),
            Err(_) => {
                s.unavailable.push(format!("cgroup.{name}"));
                None
            }
        }
    };
    s.group_memory_current_bytes = read("memory.current").and_then(|v| v.trim().parse().ok());
    s.group_memory_peak_bytes = read("memory.peak").and_then(|v| v.trim().parse().ok());
    s.group_pids_current = read("pids.current").and_then(|v| v.trim().parse().ok());
    s.group_pids_peak = read("pids.peak").and_then(|v| v.trim().parse().ok());
    if let Some(v) = read("cpu.stat") {
        s.group_cpu_ns = values(&v).get("usage_usec").map(|n| n.saturating_mul(1000));
    }
    if let Some(v) = read("memory.events") {
        let v = values(&v);
        s.group_oom_events = v.get("oom").copied();
        s.group_oom_kills = v.get("oom_kill").copied();
    }
}
#[cfg(target_os = "macos")]
fn platform(config: &ResourceConfig, s: &mut ResourceSample) {
    let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::proc_pid_rusage(
            config.process_id as i32,
            libc::RUSAGE_INFO_V4,
            (&mut info as *mut libc::rusage_info_v4).cast(),
        )
    } == 0
    {
        s.daemon_rss_bytes = Some(info.ri_resident_size);
        s.daemon_lifetime_peak_bytes = Some(info.ri_lifetime_max_phys_footprint);
        // XNU task_power_info_locked records Mach absolute ticks, not ns.
        #[repr(C)]
        struct Timebase {
            numer: u32,
            denom: u32,
        }
        unsafe extern "C" {
            fn mach_timebase_info(info: *mut Timebase) -> i32;
        }
        let mut timebase = Timebase { numer: 0, denom: 0 };
        if unsafe { mach_timebase_info(&mut timebase) } == 0 {
            s.daemon_cpu_ns = ticks_to_ns(
                info.ri_user_time.saturating_add(info.ri_system_time),
                timebase.numer as u64,
                timebase.denom as u64,
            );
        } else {
            s.unavailable.push("mach_timebase_info".into());
        }
    } else {
        s.unavailable.push("proc_pid_rusage".into());
    }
    s.unavailable.push("per-task child peak/cpu/pids/OOM require a host task adapter; process footprint is lifetime scoped".into());
}
#[cfg(windows)]
fn platform(config: &ResourceConfig, s: &mut ResourceSample) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::{
        Foundation::{CloseHandle, FILETIME},
        System::{
            JobObjects::{
                JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
                QueryInformationJobObject,
            },
            ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
            Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ},
        },
    };
    let process = unsafe {
        OpenProcess(
            PROCESS_QUERY_INFORMATION | PROCESS_VM_READ,
            0,
            config.process_id,
        )
    };
    if !process.is_null() {
        let mut memory = PROCESS_MEMORY_COUNTERS {
            cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            ..Default::default()
        };
        if unsafe {
            K32GetProcessMemoryInfo(
                process,
                &mut memory,
                std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            )
        } != 0
        {
            s.daemon_rss_bytes = Some(memory.WorkingSetSize as u64);
            s.daemon_lifetime_peak_bytes = Some(memory.PeakWorkingSetSize as u64);
        }
        let mut created = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        if unsafe { GetProcessTimes(process, &mut created, &mut exit, &mut kernel, &mut user) } != 0
        {
            let ticks = |t: FILETIME| ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64;
            s.daemon_cpu_ns = Some(
                ticks(kernel)
                    .saturating_add(ticks(user))
                    .saturating_mul(100),
            );
        }
        unsafe {
            CloseHandle(process);
        }
    } else {
        s.unavailable.push("process memory/times access".into());
    }
    if let Some(job) = &config.windows_job {
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe {
            QueryInformationJobObject(
                job.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&mut limits as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
                std::ptr::null_mut(),
            )
        } != 0
        {
            s.group_memory_peak_bytes = Some(limits.PeakJobMemoryUsed as u64);
            s.group_process_lifetime_peak_bytes = Some(limits.PeakProcessMemoryUsed as u64);
        }
        let mut accounting: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe {
            QueryInformationJobObject(
                job.as_raw_handle(),
                JobObjectBasicAccountingInformation,
                (&mut accounting as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                std::mem::size_of_val(&accounting) as u32,
                std::ptr::null_mut(),
            )
        } != 0
        {
            s.group_cpu_ns = Some(
                (accounting.TotalUserTime as u64)
                    .saturating_add(accounting.TotalKernelTime as u64)
                    .saturating_mul(100),
            );
            s.group_pids_current = Some(accounting.ActiveProcesses as u64);
        }
    } else {
        s.unavailable
            .push("host-owned Job Object not configured".into());
    }
    s.unavailable.push(
        "Job Object OOM notifications/pids kernel peak unavailable in read-only polling".into(),
    );
}
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn platform(_: &ResourceConfig, s: &mut ResourceSample) {
    s.unavailable
        .push("native process resource adapter unavailable".into());
}

/// Stops on drop; explicit stop also waits for the final durable sample.
pub struct ResourceSampler {
    pub(crate) stop: Option<tokio::sync::oneshot::Sender<()>>,
    pub(crate) task: Option<tokio::task::JoinHandle<crate::Result<ResourceSummary>>>,
}
impl ResourceSampler {
    pub async fn stop(mut self) -> crate::Result<ResourceSummary> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.task
            .take()
            .unwrap()
            .await
            .map_err(|_| crate::Error::Invalid("resource sampler interrupted"))?
    }
}
impl Drop for ResourceSampler {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn mach_ticks_and_large_cpu_counters_convert_without_intermediate_overflow() {
        assert_eq!(super::ticks_to_ns(24, 125, 3), Some(1000));
        assert_eq!(
            super::ticks_to_ns(u64::MAX, 1_000_000_000, 1_000_000_000),
            Some(u64::MAX)
        );
        assert_eq!(super::ticks_to_ns(1, 1, 0), None);
    }
}
