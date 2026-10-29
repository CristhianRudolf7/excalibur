//! Linux environment preparation. No browser, listener or profile is started here.
use serde::{Deserialize, Serialize};
use std::{
    ffi::CString,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

pub const GIB: u64 = 1024 * 1024 * 1024;
#[derive(Debug, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub web: WebConfig,
}
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebConfig {
    pub os_user: String,
    pub chrome_binary: Option<PathBuf>,
    pub data_dir: PathBuf,
    pub run_dir: PathBuf,
    pub disk_reserve_bytes: u64,
    pub search_budget_bytes: u64,
}
impl Default for WebConfig {
    fn default() -> Self {
        Self {
            os_user: "excalibur-web".into(),
            chrome_binary: None,
            data_dir: "/var/lib/excalibur-web".into(),
            run_dir: "/run/excalibur-web".into(),
            disk_reserve_bytes: 5 * GIB,
            search_budget_bytes: 2 * GIB,
        }
    }
}
impl Config {
    pub fn parse(text: &str) -> Result<Self, String> {
        let config: Self = toml::from_str(text).map_err(|e| e.to_string())?;
        let w = &config.web;
        if w.os_user.is_empty()
            || !w.data_dir.is_absolute()
            || !w.run_dir.is_absolute()
            || w.chrome_binary.as_ref().is_some_and(|p| !p.is_absolute())
            || w.disk_reserve_bytes < 5 * GIB
            || w.search_budget_bytes == 0
            || w.disk_reserve_bytes
                .checked_add(w.search_budget_bytes)
                .is_none()
        {
            return Err(
                "invalid web configuration: absolute paths and at least 5 GiB reserve required"
                    .into(),
            );
        }
        Ok(config)
    }
}
#[derive(Debug, Serialize)]
pub struct Identity {
    pub uid: u32,
    pub euid: u32,
    pub gid: u32,
    pub egid: u32,
    pub supplementary_groups: Vec<u32>,
    pub capabilities: String,
}
/// Call once, on the single main thread, before opening runtime resources.
pub fn drop_privileges(user: &str) -> Result<Identity, String> {
    // SAFETY: libc identity calls are confined to startup on the main thread.
    unsafe {
        if libc::geteuid() == 0 || libc::getuid() == 0 {
            let name = CString::new(user).map_err(|_| "invalid OS user")?;
            let entry = libc::getpwnam(name.as_ptr());
            if entry.is_null() {
                return Err("configured OS user not found".into());
            }
            let (uid, gid) = ((*entry).pw_uid, (*entry).pw_gid);
            if uid == 0 || gid == 0 {
                return Err("root identity is forbidden".into());
            }
            if libc::setgroups(0, std::ptr::null()) != 0
                || libc::setgid(gid) != 0
                || libc::setuid(uid) != 0
            {
                return Err("cannot drop privileges".into());
            }
            if libc::setuid(0) == 0 || libc::setgid(0) == 0 {
                return Err("privileges can be regained".into());
            }
        }
        let count = libc::getgroups(0, std::ptr::null_mut());
        if count < 0 {
            return Err("cannot inspect groups".into());
        }
        let mut groups = vec![0; count as usize];
        if libc::getgroups(count, groups.as_mut_ptr()) < 0 {
            return Err("cannot inspect groups".into());
        }
        let status = fs::read_to_string("/proc/self/status").map_err(|e| e.to_string())?;
        for field in ["CapInh:", "CapPrm:", "CapEff:", "CapAmb:"] {
            let value = status
                .lines()
                .find_map(|l| l.strip_prefix(field))
                .ok_or("missing capabilities")?;
            if u64::from_str_radix(value.trim(), 16).map_err(|_| "invalid capabilities")? != 0 {
                return Err("process retains capabilities".into());
            }
        }
        let identity = Identity {
            uid: libc::getuid(),
            euid: libc::geteuid(),
            gid: libc::getgid(),
            egid: libc::getegid(),
            supplementary_groups: groups,
            capabilities: "zero".into(),
        };
        if identity.uid == 0 || identity.euid == 0 || identity.gid == 0 || identity.egid == 0 {
            return Err("root identity is forbidden".into());
        }
        Ok(identity)
    }
}
#[derive(Debug, Serialize)]
pub struct Chrome {
    pub binary: PathBuf,
    pub version: String,
}
pub fn detect_chrome(explicit: Option<&Path>) -> Result<Chrome, String> {
    let candidates = match explicit {
        Some(p) => vec![p.to_owned()],
        None => vec![
            "/usr/bin/google-chrome-stable".into(),
            "/usr/bin/google-chrome".into(),
            "/usr/bin/chromium".into(),
            "/usr/bin/chromium-browser".into(),
        ],
    };
    let binary = candidates
        .into_iter()
        .find(|p| p.is_file())
        .ok_or("Chrome binary unavailable")?;
    let mut child = Command::new(&binary)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if child.try_wait().map_err(|e| e.to_string())?.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err("Chrome version check timed out".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    let version = String::from_utf8(output.stdout)
        .map_err(|_| "invalid Chrome version")?
        .trim()
        .to_owned();
    if !output.status.success()
        || !(version.starts_with("Google Chrome ") || version.starts_with("Chromium "))
        || !version.split_whitespace().any(|part| {
            part.split('.').count() == 4 && part.split('.').all(|n| n.parse::<u32>().is_ok())
        })
    {
        return Err("unrecognized Chrome version".into());
    }
    Ok(Chrome { binary, version })
}
pub fn disk_available(path: &Path) -> Result<u64, String> {
    use std::os::unix::ffi::OsStrExt;
    let name = CString::new(path.as_os_str().as_bytes()).map_err(|_| "invalid disk path")?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: pointers reference valid storage and a NUL-terminated path.
    unsafe {
        if libc::statvfs(name.as_ptr(), stat.as_mut_ptr()) != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let stat = stat.assume_init();
        stat.f_bavail
            .checked_mul(stat.f_frsize)
            .ok_or_else(|| "disk size overflow".into())
    }
}
pub fn admits_growth(available: u64, growth: u64, reserve: u64) -> bool {
    available
        .checked_sub(growth)
        .is_some_and(|remaining| remaining >= reserve)
}
#[derive(Debug, Serialize)]
pub struct Cgroup {
    pub v2: bool,
    pub path: Option<PathBuf>,
    pub controllers: Vec<String>,
    pub exclusive: bool,
    pub memory_current: Option<u64>,
    pub fallback: &'static str,
}
pub fn cgroup() -> Cgroup {
    let relative = fs::read_to_string("/proc/self/cgroup").ok().and_then(|s| {
        s.lines()
            .find_map(|l| l.strip_prefix("0::").map(str::to_owned))
    });
    let path = relative.and_then(|p| {
        let p = Path::new(&p).strip_prefix("/").ok()?;
        if p.components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return None;
        }
        Some(Path::new("/sys/fs/cgroup").join(p))
    });
    let read = |name: &str| {
        path.as_ref()
            .and_then(|p| fs::read_to_string(p.join(name)).ok())
    };
    let pids = read("cgroup.procs");
    let exclusive = pids.is_some_and(|s| {
        let ids: Vec<_> = s.split_whitespace().collect();
        ids == [std::process::id().to_string()]
    });
    Cgroup {
        v2: Path::new("/sys/fs/cgroup/cgroup.controllers").is_file(),
        controllers: read("cgroup.controllers")
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_owned)
            .collect(),
        memory_current: read("memory.current").and_then(|s| s.trim().parse().ok()),
        path,
        exclusive,
        fallback: "/proc process-tree accounting required when cgroup is shared or inaccessible",
    }
}
#[derive(Debug, Serialize)]
pub struct Preflight {
    pub ready: bool,
    pub identity: Identity,
    pub chrome: Option<Chrome>,
    pub disk_available_bytes: Option<u64>,
    pub disk_reserve_bytes: u64,
    pub search_budget_bytes: u64,
    pub search_admissible: bool,
    pub cgroup: Cgroup,
    pub errors: Vec<String>,
    pub sandbox_verified: bool,
}
pub fn preflight(config: &WebConfig, identity: Identity) -> Preflight {
    let mut errors = Vec::new();
    let chrome = detect_chrome(config.chrome_binary.as_deref())
        .map_err(|e| errors.push(e))
        .ok();
    let disk = disk_available(&config.data_dir)
        .map_err(|e| errors.push(format!("data directory disk check: {e}")))
        .ok();
    let admissible = disk
        .is_some_and(|n| admits_growth(n, config.search_budget_bytes, config.disk_reserve_bytes));
    if !admissible {
        errors.push("insufficient or unavailable disk for search budget plus reserve".into());
    }
    // Check access under the final identity; never create sockets or profiles.
    for path in [&config.data_dir, &config.run_dir] {
        use std::os::unix::ffi::OsStrExt;
        let accessible = CString::new(path.as_os_str().as_bytes())
            .ok()
            .is_some_and(|p| unsafe { libc::access(p.as_ptr(), libc::W_OK | libc::X_OK) == 0 });
        if !path.is_dir() || !accessible {
            errors.push(format!(
                "directory unavailable or not writable: {}",
                path.display()
            ));
        }
    }
    Preflight {
        ready: errors.is_empty(),
        identity,
        chrome,
        disk_available_bytes: disk,
        disk_reserve_bytes: config.disk_reserve_bytes,
        search_budget_bytes: config.search_budget_bytes,
        search_admissible: admissible,
        cgroup: cgroup(),
        errors,
        sandbox_verified: false,
    }
}
