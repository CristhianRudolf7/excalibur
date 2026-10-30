//! Linux environment preparation. No browser, listener or profile is started here.
pub mod loopback;
pub mod viewer;
use serde::{Deserialize, Serialize};
use std::{
    ffi::CString,
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
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
    pub host_uid: u32,
    pub socket_gid: Option<u32>,
    pub profiles: std::collections::BTreeMap<String, excalibur_browser::sessions::Profile>,
    pub compact: bool,
    pub chrome_args: Vec<String>,
    pub instance_class: excalibur_browser::Class,
    pub version: excalibur_browser::version::VersionPolicy,
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
            host_uid: 0,
            socket_gid: None,
            profiles: Default::default(),
            compact: false,
            chrome_args: Vec::new(),
            instance_class: Default::default(),
            version: Default::default(),
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
        excalibur_browser::sessions::validate_profiles(&w.profiles).map_err(|e| e.to_string())?;
        excalibur_browser::validate_args(&w.chrome_args).map_err(|e| e.to_string())?;
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
pub use excalibur_browser::installation::{Chrome, detect_chrome};
#[derive(Debug, Serialize)]
pub struct DirectoryCheck {
    pub path: PathBuf,
    pub owner_uid: Option<u32>,
    pub mode: Option<u32>,
    pub private: bool,
    pub writable: bool,
    pub shared_socket_directory: bool,
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
        shared_socket_directory: false,
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
    let mut directories: Vec<_> = [&config.data_dir, &config.run_dir]
        .into_iter()
        .map(|path| check_directory(path, identity.euid))
        .collect();
    if config.host_uid != 0 && config.host_uid != identity.euid {
        let runtime = &mut directories[1];
        runtime.shared_socket_directory =
            fs::symlink_metadata(&config.run_dir).ok().is_some_and(|m| {
                m.is_dir()
                    && m.uid() == identity.euid
                    && m.gid() == config.socket_gid.unwrap_or(identity.egid)
                    && m.mode() & 0o7777 == 0o710
            });
    }
    for directory in &directories {
        if (!directory.private && !directory.shared_socket_directory) || !directory.writable {
            errors.push(format!(
                "directory must be owned by the final UID, private 0700 (or configured shared runtime 0710) and writable: {}",
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
