//! Linux environment preparation. No browser, listener or profile is started here.
use serde::{Deserialize, Serialize};
use std::{
    ffi::CString,
    fs,
    io::Read,
    os::{
        fd::AsRawFd,
        unix::{fs::MetadataExt, process::CommandExt},
    },
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
    let (status, stdout) = probe_version(&binary)?;
    let version = String::from_utf8(stdout)
        .map_err(|_| "invalid Chrome version")?
        .trim()
        .to_owned();
    if !status.success()
        || !(version.starts_with("Google Chrome ") || version.starts_with("Chromium "))
        || !version.split_whitespace().any(|part| {
            part.split('.').count() == 4 && part.split('.').all(|n| n.parse::<u32>().is_ok())
        })
    {
        return Err("unrecognized Chrome version".into());
    }
    Ok(Chrome { binary, version })
}
// Own only the probe's process group, including descendants holding stdout open.
struct Probe(std::process::Child);
impl Drop for Probe {
    fn drop(&mut self) {
        // SAFETY: process_group(0) created a separate group with this child's PID.
        unsafe {
            libc::kill(-(self.0.id() as libc::pid_t), libc::SIGKILL);
        }
        let _ = self.0.wait();
    }
}
fn probe_version(binary: &Path) -> Result<(std::process::ExitStatus, Vec<u8>), String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut probe = Probe(
        Command::new(binary)
            .arg("--version")
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| e.to_string())?,
    );
    let mut stdout = probe.0.stdout.take().ok_or("missing probe stdout")?;
    // SAFETY: stdout owns a valid descriptor; preserve its existing flags.
    unsafe {
        let flags = libc::fcntl(stdout.as_raw_fd(), libc::F_GETFL);
        if flags < 0 || libc::fcntl(stdout.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) < 0
        {
            return Err(std::io::Error::last_os_error().to_string());
        }
    }
    let mut output = Vec::new();
    let mut eof = false;
    loop {
        if Instant::now() >= deadline {
            return Err("Chrome version check timed out".into());
        }
        let mut buffer = [0; 1024];
        if !eof {
            match stdout.read(&mut buffer) {
                Ok(0) => eof = true,
                Ok(n) => {
                    output.extend_from_slice(&buffer[..n]);
                    if output.len() > 4096 {
                        return Err("Chrome version output exceeds 4096 bytes".into());
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        if let Some(status) = probe.0.try_wait().map_err(|e| e.to_string())?
            && eof
        {
            return Ok((status, output));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[derive(Debug, Serialize)]
pub struct DirectoryCheck {
    pub path: PathBuf,
    pub owner_uid: Option<u32>,
    pub mode: Option<u32>,
    pub private: bool,
    pub writable: bool,
}
fn check_directory(path: &Path, uid: u32) -> DirectoryCheck {
    use std::os::unix::ffi::OsStrExt;
    let metadata = fs::symlink_metadata(path).ok();
    let private = metadata
        .as_ref()
        .is_some_and(|m| m.is_dir() && m.uid() == uid && m.mode() & 0o7777 == 0o700);
    let writable = CString::new(path.as_os_str().as_bytes())
        .ok()
        .is_some_and(|p| {
            // SAFETY: p is a valid NUL-terminated path; no filesystem mutation.
            unsafe { libc::access(p.as_ptr(), libc::W_OK | libc::X_OK) == 0 }
        });
    DirectoryCheck {
        path: path.to_owned(),
        owner_uid: metadata.as_ref().map(MetadataExt::uid),
        mode: metadata.as_ref().map(|m| m.mode() & 0o7777),
        private,
        writable,
    }
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
    pub directories: Vec<DirectoryCheck>,
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
    // Require private, owned directories under the final identity; create nothing.
    let directories: Vec<_> = [&config.data_dir, &config.run_dir]
        .into_iter()
        .map(|path| check_directory(path, identity.euid))
        .collect();
    for directory in &directories {
        if !directory.private || !directory.writable {
            errors.push(format!(
                "directory must be owned by the final UID, mode 0700 and writable: {}",
                directory.path.display()
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
        directories,
    }
}
