//! Tiered memory sampling from architecture section 13.1.
//!
//! Level A reads the private cgroup (`memory.current` minus `inactive_file`).
//! Level B sums `RssAnon + VmSwap` from `/proc/<pid>/status` (about 70 us per
//! process). Level C sums `Pss` from `/proc/<pid>/smaps_rollup` (about 8.2 ms
//! per Chrome process). Level D reacts to `memory.events` growth or PSI
//! pressure. Never sum RSS: shared libraries triple-count it.

use std::collections::BTreeMap;
use std::path::Path;

use crate::budgets::EVICTION_PSI_AVG10_TRIGGER_PCT;

/// Sampling tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SampleLevel {
    /// 2 s cgroup working set; primary limit metric when available.
    A,
    /// 10 s per-process `RssAnon + VmSwap`; primary metric without cgroup.
    B,
    /// 60 s PSS rollups near the budget or under `browser_status{detail}`.
    C,
    /// Event driven: cgroup events or PSI pressure.
    D,
}

impl SampleLevel {
    /// Nominal sampling interval in seconds (D is event driven: 0).
    #[must_use]
    pub fn interval_secs(self) -> u64 {
        match self {
            Self::A => 2,
            Self::B => 10,
            Self::C => 60,
            Self::D => 0,
        }
    }
}

/// Chrome process class parsed from the `--type=` command-line token.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ProcessType {
    /// Main browser process (no `--type=` token).
    Browser,
    /// Zygote template process.
    Zygote,
    /// GPU process.
    Gpu,
    /// Utility process (network, storage, audio...).
    Utility,
    /// Renderer process.
    Renderer,
    /// Crashpad handler.
    Crashpad,
    /// webd itself.
    Webd,
    /// Anything else, with the raw `--type=` value.
    Other(String),
}

impl ProcessType {
    /// Human-readable label used in reports.
    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            Self::Browser => "browser",
            Self::Zygote => "zygote",
            Self::Gpu => "gpu-process",
            Self::Utility => "utility",
            Self::Renderer => "renderer",
            Self::Crashpad => "crashpad_handler",
            Self::Webd => "webd",
            Self::Other(value) => value.as_str(),
        }
    }
}

/// Classify a Chrome command line. Chrome rewrites its command line as one
/// space-separated string; the process class is the `--type=` token. A main
/// browser process carries no `--type=` token.
#[must_use]
pub fn classify_cmdline(cmdline: &str) -> ProcessType {
    for token in cmdline.split(' ') {
        let Some(value) = token.strip_prefix("--type=") else {
            continue;
        };
        return match value {
            "renderer" => ProcessType::Renderer,
            "gpu-process" => ProcessType::Gpu,
            "zygote" => ProcessType::Zygote,
            "utility" => ProcessType::Utility,
            "crashpad-handler" | "crashpad_handler" => ProcessType::Crashpad,
            other => ProcessType::Other(other.to_string()),
        };
    }
    ProcessType::Browser
}

/// Parse `RssAnon` and `VmSwap` (kB) from `/proc/<pid>/status` content and
/// return the level-B working set in bytes (`RssAnon + VmSwap`).
pub fn parse_status_working_set(status: &str) -> Option<u64> {
    let mut rss_anon: Option<u64> = None;
    let mut swap: Option<u64> = None;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("RssAnon:") {
            rss_anon = first_kb(rest);
        } else if let Some(rest) = line.strip_prefix("VmSwap:") {
            swap = first_kb(rest);
        }
    }
    match (rss_anon, swap) {
        (Some(anon_kb), Some(swap_kb)) => Some((anon_kb + swap_kb) * 1024),
        _ => None,
    }
}

/// Parse the `Pss:` line (kB) from `smaps_rollup` content into bytes.
pub fn parse_smaps_pss(smaps_rollup: &str) -> Option<u64> {
    for line in smaps_rollup.lines() {
        if let Some(rest) = line.strip_prefix("Pss:") {
            return first_kb(rest).map(|kb| kb * 1024);
        }
    }
    None
}

/// Parse a bare byte counter such as `memory.current`.
pub fn parse_byte_counter(content: &str) -> Option<u64> {
    content.split_whitespace().next()?.parse::<u64>().ok()
}

/// Parse `inactive_file` (bytes) from `memory.stat` content.
pub fn parse_memory_stat_inactive_file(memory_stat: &str) -> Option<u64> {
    for line in memory_stat.lines() {
        if let Some(rest) = line.strip_prefix("inactive_file ") {
            return rest.split_whitespace().next()?.parse::<u64>().ok();
        }
    }
    None
}

/// Level-A working set: `memory.current - inactive_file`, saturating.
#[must_use]
pub fn working_set_a(memory_current: u64, inactive_file: u64) -> u64 {
    memory_current.saturating_sub(inactive_file)
}

/// cgroup event counters of interest.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemoryEvents {
    /// `high` counter.
    pub high: u64,
    /// `max` counter.
    pub max: u64,
    /// `oom_kill` counter.
    pub oom_kill: u64,
}

/// Parse `high`, `max` and `oom_kill` from `memory.events` content.
#[must_use]
pub fn parse_memory_events(content: &str) -> MemoryEvents {
    let mut events = MemoryEvents::default();
    for line in content.lines() {
        let mut parts = line.split_whitespace();
        let (Some(key), Some(value)) = (parts.next(), parts.next()) else {
            continue;
        };
        let Ok(number) = value.parse::<u64>() else {
            continue;
        };
        match key {
            "high" => events.high = number,
            "max" => events.max = number,
            "oom_kill" => events.oom_kill = number,
            _ => {}
        }
    }
    events
}

/// Parse PSI `some avg10=` from `/proc/pressure/memory` content.
pub fn parse_psi_some_avg10(content: &str) -> Option<f64> {
    for line in content.lines() {
        let line = line.strip_prefix("some")?;
        for token in line.split_whitespace() {
            if let Some(value) = token.strip_prefix("avg10=") {
                return value.parse::<f64>().ok();
            }
        }
    }
    None
}

/// True when PSI pressure exceeds the level-D eviction trigger (> 10 %).
#[must_use]
pub fn psi_triggers_eviction(some_avg10_pct: f64) -> bool {
    some_avg10_pct > EVICTION_PSI_AVG10_TRIGGER_PCT
}

/// One sampled process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcEntry {
    /// Operating-system process id.
    pub pid: u32,
    /// Chrome process class.
    pub process_type: ProcessType,
    /// Level-B working set (`RssAnon + VmSwap`) in bytes.
    pub working_set_bytes: u64,
    /// Level-C PSS in bytes when the rollup was readable.
    pub pss_bytes: Option<u64>,
}

/// Aggregate sample over one enumeration pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sample {
    /// Entries in PID order.
    pub entries: Vec<ProcEntry>,
    /// Level used for `working_set_bytes`.
    pub level: Option<SampleLevel>,
    /// Total working set in bytes.
    pub working_set_bytes: u64,
    /// Total PSS in bytes when every entry carried a rollup.
    pub pss_bytes: Option<u64>,
}

impl Sample {
    /// PSS totals grouped by process class, in bytes.
    #[must_use]
    pub fn pss_by_type(&self) -> BTreeMap<ProcessType, u64> {
        let mut totals: BTreeMap<ProcessType, u64> = BTreeMap::new();
        for entry in &self.entries {
            if let Some(pss) = entry.pss_bytes {
                *totals.entry(entry.process_type.clone()).or_default() += pss;
            }
        }
        totals
    }

    /// Working-set totals grouped by process class, in bytes.
    #[must_use]
    pub fn working_set_by_type(&self) -> BTreeMap<ProcessType, u64> {
        let mut totals: BTreeMap<ProcessType, u64> = BTreeMap::new();
        for entry in &self.entries {
            *totals.entry(entry.process_type.clone()).or_default() += entry.working_set_bytes;
        }
        totals
    }
}

fn first_kb(rest: &str) -> Option<u64> {
    rest.split_whitespace().next()?.parse::<u64>().ok()
}

fn read_file(root: &Path, pid: u32, name: &str) -> Option<String> {
    std::fs::read_to_string(root.join(pid.to_string()).join(name)).ok()
}

/// Enumerate `pids` under a `/proc`-shaped `root`.
///
/// Each `<root>/<pid>/` directory may contain `status`, `smaps_rollup` and
/// `cmdline` files with the same layout as Linux `/proc`. Missing `status`
/// entries are skipped; missing rollups leave `pss_bytes` empty. The result
/// is sorted by PID so independent reads of the same instant agree.
#[must_use]
pub fn enumerate_proc_root(root: &Path, pids: &[u32]) -> Sample {
    let mut entries = Vec::new();
    let mut sorted: Vec<u32> = pids.to_vec();
    sorted.sort_unstable();
    for pid in sorted {
        let Some(status) = read_file(root, pid, "status") else {
            continue;
        };
        let Some(working_set_bytes) = parse_status_working_set(&status) else {
            continue;
        };
        let cmdline = read_file(root, pid, "cmdline").unwrap_or_default();
        let process_type = classify_cmdline(&cmdline.replace('\0', " "));
        let pss_bytes = read_file(root, pid, "smaps_rollup")
            .as_deref()
            .and_then(parse_smaps_pss);
        entries.push(ProcEntry {
            pid,
            process_type,
            working_set_bytes,
            pss_bytes,
        });
    }
    let working_set_bytes = entries.iter().map(|e| e.working_set_bytes).sum();
    let all_have_pss = !entries.is_empty() && entries.iter().all(|e| e.pss_bytes.is_some());
    let pss_bytes = if all_have_pss {
        Some(entries.iter().map(|e| e.pss_bytes.unwrap_or(0)).sum())
    } else {
        None
    };
    let level = if entries.is_empty() {
        None
    } else {
        Some(SampleLevel::B)
    };
    Sample {
        entries,
        level,
        working_set_bytes,
        pss_bytes,
    }
}

/// Read the current process level-B working set from the live `/proc`.
pub fn current_process_working_set() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    parse_status_working_set(&status)
}

/// Resolve the caller cgroup directory from `/proc/self/cgroup` (cgroup v2
/// `0::<relative>`). Returns `None` when the layout is unexpected so callers
/// fail closed to the level-B `/proc` tree instead of guessing a path.
#[must_use]
pub fn cgroup_dir_from_self() -> Option<std::path::PathBuf> {
    let content = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let relative = content.lines().find_map(|line| line.strip_prefix("0::"))?;
    // Reject absolute escapes and `..` so a hostile `/proc` cannot steer the
    // read outside `/sys/fs/cgroup`.
    let relative = relative.strip_prefix('/').unwrap_or(relative);
    if relative.is_empty() {
        return None;
    }
    let mut joined = std::path::PathBuf::from("/sys/fs/cgroup");
    for component in std::path::Path::new(relative).components() {
        match component {
            std::path::Component::Normal(part) => joined.push(part),
            _ => return None,
        }
    }
    Some(joined)
}

/// Read the live level-A working set (`memory.current - inactive_file`) from
/// a cgroup directory. Returns `None` when the cgroup is shared, inaccessible
/// or lacks the memory controller so callers fall back to level B.
#[must_use]
pub fn read_cgroup_working_set(dir: &std::path::Path) -> Option<u64> {
    let current = parse_byte_counter(&std::fs::read_to_string(dir.join("memory.current")).ok()?)?;
    let stat = std::fs::read_to_string(dir.join("memory.stat")).ok()?;
    let inactive = parse_memory_stat_inactive_file(&stat)?;
    Some(working_set_a(current, inactive))
}

/// Read the live `memory.events` counters from a cgroup directory.
#[must_use]
pub fn read_cgroup_events(dir: &std::path::Path) -> Option<MemoryEvents> {
    let content = std::fs::read_to_string(dir.join("memory.events")).ok()?;
    Some(parse_memory_events(&content))
}

/// True when the cgroup `memory.high` knob is writable without changing it.
/// The B6 forcing test only writes when this probe succeeds; otherwise it
/// fails closed with a clear message instead of throttling a shared scope.
#[must_use]
pub fn cgroup_memory_high_writable(dir: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(dir.join("memory.high"))
        .map(|meta| meta.permissions().mode() & 0o200 != 0)
        .unwrap_or(false)
}

/// Read the live PSI `some avg10` pressure in percent, when the kernel
/// exposes `/proc/pressure/memory`.
#[must_use]
pub fn live_psi_some_avg10() -> Option<f64> {
    let content = std::fs::read_to_string("/proc/pressure/memory").ok()?;
    parse_psi_some_avg10(&content)
}

/// Instance class selecting the OOM policy.
///
/// Canonical [`crate::eviction::InstanceKind`] aliased here so sampling and
/// eviction share one enum; `InstanceClass` remains as a compatibility name.
/// The enforcing layer for OOM policy is the webd service wiring, which reads
/// these values when it adjusts scores; sampling only computes them.
pub use crate::eviction::InstanceKind as InstanceClass;

/// Desired `oom_score_adj` for a process class.
///
/// Chrome ships 300 for renderers, 200 for GPU/utility and 0 for the browser
/// process. webd raises ephemeral renderers to 1000 so a global OOM picks
/// them before any listening session.
#[must_use]
pub fn desired_oom_score_adj(process_type: &ProcessType, instance: InstanceClass) -> i32 {
    match (process_type, instance) {
        (ProcessType::Renderer, InstanceClass::Ephemeral) => 1000,
        (ProcessType::Renderer, InstanceClass::Persistent) => 300,
        (ProcessType::Gpu | ProcessType::Utility, _) => 200,
        (ProcessType::Browser, _) => 0,
        (ProcessType::Webd, _) => 0,
        (ProcessType::Zygote, _) => 0,
        (ProcessType::Crashpad, _) => 0,
        (ProcessType::Other(_), _) => 0,
    }
}

/// An unprivileged process of the same user may raise (numerically increase)
/// another process `oom_score_adj`; lowering it needs privilege. B6 verifies
/// the raise path on the target machine.
#[must_use]
pub fn oom_raise_allowed(current: i32, desired: i32, same_uid: bool) -> bool {
    same_uid && desired >= current
}
