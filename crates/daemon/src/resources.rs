use std::io;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessSample {
    pub pid: u32,
    pub rss_bytes: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemorySample {
    pub daemon: ProcessSample,
    pub descendants: Vec<ProcessSample>,
    pub total_rss_bytes: u64,
}
/// Read native process accounting. Unsupported environments report an error,
/// never a synthetic zero that might reopen admission incorrectly.
pub fn sample_memory() -> io::Result<MemorySample> {
    #[cfg(target_os = "linux")]
    {
        let own = std::process::id();
        let mut processes = Vec::new();
        for entry in std::fs::read_dir("/proc")? {
            let entry = entry?;
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<u32>().ok())
            else {
                continue;
            };
            // Processes may exit between enumeration and reads.
            let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            let Some((_, tail)) = stat.rsplit_once(')') else {
                continue;
            };
            let fields = tail.split_whitespace().collect::<Vec<_>>();
            let Some(ppid) = fields.get(1).and_then(|v| v.parse::<u32>().ok()) else {
                continue;
            };
            let Ok(status) = std::fs::read_to_string(entry.path().join("status")) else {
                continue;
            };
            let rss = status
                .lines()
                .find_map(|line| {
                    line.strip_prefix("VmRSS:")
                        .and_then(|v| v.split_whitespace().next())
                        .and_then(|v| v.parse::<u64>().ok())
                })
                .unwrap_or(0)
                .saturating_mul(1024);
            processes.push((pid, ppid, rss));
        }
        let rss = processes
            .iter()
            .find(|(pid, _, _)| *pid == own)
            .map(|(_, _, rss)| *rss)
            .ok_or_else(|| io::Error::other("daemon RSS unavailable"))?;
        let mut selected = std::collections::BTreeSet::from([own]);
        loop {
            let before = selected.len();
            for (pid, ppid, _) in &processes {
                if selected.contains(ppid) {
                    selected.insert(*pid);
                }
            }
            if selected.len() == before {
                break;
            }
        }
        let descendants = processes
            .into_iter()
            .filter(|(pid, _, _)| *pid != own && selected.contains(pid))
            .map(|(pid, _, rss_bytes)| ProcessSample { pid, rss_bytes })
            .collect::<Vec<_>>();
        let total_rss_bytes = descendants
            .iter()
            .fold(rss, |sum, p| sum.saturating_add(p.rss_bytes));
        Ok(MemorySample {
            daemon: ProcessSample {
                pid: own,
                rss_bytes: rss,
            },
            descendants,
            total_rss_bytes,
        })
    }
    #[cfg(not(target_os = "linux"))]
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "native RSS adapter unavailable on this host",
    ))
}
