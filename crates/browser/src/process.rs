use serde::Serialize;
use std::{collections::HashSet, fs, path::Path};
#[derive(Clone, Debug, Serialize)]
pub struct Process {
    pub pid: u32,
    pub parent: u32,
    pub start_ticks: u64,
    pub kind: String,
    pub seccomp: Option<u32>,
    pub no_new_privs: Option<u32>,
    pub user_ns: String,
    pub net_ns: String,
}
fn inspect(root: &Path, pid: u32) -> Option<Process> {
    let path = root.join(pid.to_string());
    let stat = fs::read_to_string(path.join("stat")).ok()?;
    let fields: Vec<_> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    if matches!(*fields.first()?, "Z" | "X") {
        return None;
    }
    let status = match fs::read_to_string(path.join("status")) {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(_) => String::new(),
    };
    let number = |key: &str| {
        status
            .lines()
            .find_map(|l| l.strip_prefix(key))
            .and_then(|v| v.trim().parse().ok())
    };
    let (cmd, unreadable) = match fs::read(path.join("cmdline")) {
        Ok(v) => (v, false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(_) => (Vec::new(), true),
    };
    let cmd = String::from_utf8_lossy(&cmd).replace('\0', " ");
    let kind = cmd
        .split_whitespace()
        .find_map(|s| s.strip_prefix("--type="))
        .unwrap_or(if unreadable || status.is_empty() {
            "uninspectable"
        } else if cmd.contains("crashpad_handler") {
            "crashpad_handler"
        } else {
            "helper"
        })
        .into();
    Some(Process {
        pid,
        parent: fields.get(1)?.parse().ok()?,
        start_ticks: fields.get(19)?.parse().ok()?,
        kind,
        seccomp: number("Seccomp:"),
        no_new_privs: number("NoNewPrivs:"),
        user_ns: match fs::read_link(path.join("ns/user")) {
            Ok(p) => p.to_string_lossy().into_owned(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(_) => String::new(),
        },
        net_ns: match fs::read_link(path.join("ns/net")) {
            Ok(p) => p.to_string_lossy().into_owned(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(_) => String::new(),
        },
    })
}
pub fn tree(pid: u32) -> Vec<Process> {
    tree_at(Path::new("/proc"), pid)
}
pub fn tree_at(root: &Path, pid: u32) -> Vec<Process> {
    let Some(mut browser) = inspect(root, pid) else {
        return Vec::new();
    };
    browser.kind = "browser".into();
    let all: Vec<_> = fs::read_dir(root)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let id = entry.file_name().to_str()?.parse::<u32>().ok()?;
            let stat = fs::read_to_string(entry.path().join("stat")).ok()?;
            let parent = stat
                .rsplit_once(')')?
                .1
                .split_whitespace()
                .nth(1)?
                .parse::<u32>()
                .ok()?;
            Some((id, parent))
        })
        .collect();
    let mut known = HashSet::from([pid]);
    loop {
        let previous = known.len();
        for &(id, parent) in &all {
            if known.contains(&parent) {
                known.insert(id);
            }
        }
        if previous == known.len() {
            break;
        }
    }
    let mut result = vec![browser];
    result.extend(
        known
            .into_iter()
            .filter(|id| *id != pid)
            .filter_map(|id| inspect(root, id)),
    );
    result
}
pub fn verify(processes: &[Process]) -> Result<usize, String> {
    let browser = processes.first().ok_or("browser process unavailable")?;
    if processes.iter().any(|p| p.kind == "uninspectable") {
        return Err("sandbox_unavailable: unreadable process metadata".into());
    }
    if browser.user_ns.is_empty() || browser.net_ns.is_empty() {
        return Err("browser namespaces unreadable".into());
    }
    let mut count = 0;
    for p in processes.iter().filter(|p| p.kind == "renderer") {
        if p.seccomp != Some(2)
            || p.no_new_privs != Some(1)
            || p.user_ns.is_empty()
            || p.net_ns.is_empty()
            || p.user_ns == browser.user_ns
            || p.net_ns == browser.net_ns
        {
            return Err(format!("sandbox_unavailable: renderer {}", p.pid));
        }
        count += 1;
    }
    Ok(count)
}

/// Include Chrome's double-forked crash handlers by executable and private profile,
/// without absorbing unrelated Chrome processes running under the same UID.
pub fn tree_for_profile(pid: u32, profile: &Path) -> Vec<Process> {
    use std::os::unix::fs::MetadataExt;
    let mut result = tree(pid);
    let Some(parent_exe) = fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .and_then(|p| p.parent().map(Path::to_owned))
    else {
        return result;
    };
    let marker = format!("--database={}/", profile.display());
    for entry in fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
    {
        let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if result.iter().any(|p| p.pid == id) {
            continue;
        }
        let path = entry.path();
        if fs::metadata(&path).is_ok_and(|m| m.uid() == unsafe { libc::geteuid() })
            && fs::read_link(path.join("exe")).is_ok_and(|p| {
                p.parent() == Some(parent_exe.as_path())
                    && p.file_name()
                        .is_some_and(|n| n == "chrome_crashpad_handler")
            })
            && fs::read(path.join("cmdline")).is_ok_and(|cmd| {
                cmd.split(|b| *b == 0)
                    .any(|arg| arg.starts_with(marker.as_bytes()))
            })
            && let Some(process) = inspect(Path::new("/proc"), id)
        {
            result.push(process);
        }
    }
    result
}

/// New targets stay debugger-paused while renderer sandbox setup completes.
/// Once a PID/start-time identity has passed, any regression fails immediately.
#[derive(Default)]
pub struct SandboxGate {
    seen: std::collections::HashMap<(u32, u64), (std::time::Instant, bool)>,
}
impl SandboxGate {
    pub fn check(
        &mut self,
        processes: &[Process],
        now: std::time::Instant,
    ) -> Result<bool, String> {
        let browser = processes.first().ok_or("browser process unavailable")?;
        if browser.user_ns.is_empty() || browser.net_ns.is_empty() {
            return Err("browser namespaces unreadable".into());
        }
        self.seen
            .retain(|key, _| processes.iter().any(|p| (p.pid, p.start_ticks) == *key));
        let mut ready = true;
        for process in processes.iter().skip(1) {
            let valid = process.kind != "uninspectable"
                && (process.kind != "renderer"
                    || verify(&[browser.clone(), process.clone()]).is_ok());
            let entry = self
                .seen
                .entry((process.pid, process.start_ticks))
                .or_insert((now, false));
            if valid {
                if process.kind == "renderer" {
                    entry.1 = true;
                }
            } else {
                if entry.1 || now.duration_since(entry.0) >= std::time::Duration::from_secs(1) {
                    return Err(format!(
                        "sandbox_unavailable: unverified process {}",
                        process.pid
                    ));
                }
                ready = false;
            }
        }
        Ok(ready)
    }
}
