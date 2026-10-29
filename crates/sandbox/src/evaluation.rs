//! Explicit host-attested evaluation containers. Namespace observations pin the
//! declaration; they do not claim to prove Docker/Harbor from inside a process.
use std::collections::BTreeMap;
#[derive(Clone, Debug)]
pub struct EvalContainerAttestation {
    runtime: String,
    namespaces: BTreeMap<String, String>,
}
impl EvalContainerAttestation {
    pub fn from_host_declaration(runtime: impl Into<String>) -> Result<Self, String> {
        let runtime = runtime.into();
        if runtime.is_empty() || runtime.len() > 128 || runtime.chars().any(char::is_control) {
            return Err("container runtime declaration must contain 1..128 printable bytes".into());
        }
        if std::env::var("EXCALIBUR_EVAL_CONTAINER").as_deref() != Ok("1") {
            return Err(
                "eval requires EXCALIBUR_EVAL_CONTAINER=1 from the trusted container adapter"
                    .into(),
            );
        }
        Ok(Self {
            runtime,
            namespaces: namespace_facts()?,
        })
    }
    pub fn validate_current(&self) -> Result<(), String> {
        if self.namespaces != namespace_facts()? {
            Err("evaluation container namespace identity changed after host attestation".into())
        } else {
            Ok(())
        }
    }
    pub fn metadata(&self) -> serde_json::Value {
        serde_json::json!({"runtime":self.runtime,"source":"explicit_host_declaration","namespace_facts":self.namespaces,"container_engine_independently_verified":false})
    }
}
fn namespace_facts() -> Result<BTreeMap<String, String>, String> {
    #[cfg(target_os = "linux")]
    {
        ["mnt", "pid", "net", "user"]
            .into_iter()
            .map(|name| {
                std::fs::read_link(format!("/proc/self/ns/{name}"))
                    .map(|value| (name.into(), value.to_string_lossy().into_owned()))
                    .map_err(|_| "Linux namespace observations are unavailable".into())
            })
            .collect()
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err("native evaluation container confinement is implemented only on Linux".into())
    }
}
pub const ABSOLUTE_DENIED: &[&str] = &["/tests", "/logs/verifier", "/solution", "/logs/agent"];

#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
pub struct EvalChildSetup {
    ruleset: i32,
    filter_address: usize,
    filter_len: u16,
}
#[cfg(target_os = "linux")]
impl EvalChildSetup {
    pub(crate) fn new(ruleset: i32, filter: &[libc::sock_filter]) -> Self {
        Self {
            ruleset,
            filter_address: filter.as_ptr() as usize,
            filter_len: filter.len() as u16,
        }
    }
    /// # Safety
    /// Invoke only in the forked pre-exec child while its owning PreparedSandbox
    /// remains alive. The captured ruleset and filter storage must remain valid.
    pub unsafe fn install(self) -> std::io::Result<()> {
        unsafe { linux::install(self.ruleset, self.filter_address, self.filter_len) }
    }
}

#[cfg(target_os = "linux")]
pub(crate) mod linux {
    use super::*;
    use crate::SandboxConfig;
    use std::{
        ffi::CString,
        fs::File,
        io,
        os::{
            fd::{AsRawFd, FromRawFd, RawFd},
            unix::ffi::OsStrExt,
        },
        path::{Component, Path, PathBuf},
    };
    const READ: u64 = (1 << 0) | (1 << 2) | (1 << 3);
    const ALL: u64 = (1 << 15) - 1;
    #[repr(C)]
    struct RuleSet {
        fs: u64,
    }
    #[repr(C, packed)]
    struct PathRule {
        access: u64,
        parent_fd: i32,
    }
    pub(crate) struct PreparedEval {
        ruleset: File,
    }
    impl PreparedEval {
        pub(crate) fn new(config: &SandboxConfig) -> Result<Self, String> {
            config
                .eval_container
                .as_ref()
                .ok_or("eval requires a host-attested container boundary")?
                .validate_current()?;
            let abi = unsafe {
                libc::syscall(
                    libc::SYS_landlock_create_ruleset,
                    std::ptr::null::<u8>(),
                    0,
                    1,
                )
            };
            if abi < 3 {
                return Err("eval requires Linux Landlock ABI 3 or newer; no project or unsandboxed fallback is permitted".into());
            }
            let attr = RuleSet { fs: ALL };
            let fd = unsafe {
                libc::syscall(
                    libc::SYS_landlock_create_ruleset,
                    &attr,
                    std::mem::size_of::<RuleSet>(),
                    0,
                )
            };
            if fd < 0 {
                return Err(format!(
                    "evaluation Landlock ruleset creation failed: {}",
                    io::Error::last_os_error()
                ));
            }
            let this = Self {
                ruleset: unsafe { File::from_raw_fd(fd as i32) },
            };
            let mut denied = config
                .hidden_paths
                .iter()
                .map(|path| normalized_future(path))
                .collect::<Result<Vec<_>, _>>()?;
            for path in ABSOLUTE_DENIED {
                denied.push(PathBuf::from(path));
                denied.push(normalized_future(Path::new(path))?);
            }
            for root in &config.write_roots {
                let root = crate::canonical(root)?;
                if root == Path::new("/")
                    || denied
                        .iter()
                        .any(|path| path.starts_with(&root) || root.starts_with(path))
                {
                    return Err("evaluation writable roots must be disjoint from private data, configuration and evaluator paths".into());
                }
                if !root.is_dir() {
                    return Err("evaluation writable roots must be existing directories".into());
                }
                for metadata in crate::protected_paths(&root, false, &denied)? {
                    if !readonly_mount(&metadata)? {
                        return Err("existing sensitive project metadata requires an actual read-only mount supplied by the container adapter".into());
                    }
                }
                this.add(&root, ALL)?;
            }
            let mut excluded = denied.clone();
            for root in &config.write_roots {
                excluded.extend(crate::protected_paths(root, false, &denied)?);
            }
            if !crate::linux::cross_boundary_hardlink_aliases(&config.write_roots, &excluded)?
                .is_empty()
            {
                return Err("evaluation writable roots contain hardlink aliases outside their authority; the container adapter must provision independent files".into());
            }
            let mut reads = config.read_roots.clone();
            reads.extend(
                [
                    "/usr",
                    "/bin",
                    "/sbin",
                    "/lib",
                    "/lib64",
                    "/etc/ld.so.cache",
                    "/etc/ld.so.conf",
                    "/etc/ld.so.conf.d",
                    "/etc/alternatives",
                    "/etc/passwd",
                    "/etc/group",
                    "/etc/nsswitch.conf",
                    "/etc/hosts",
                    "/etc/resolv.conf",
                    "/etc/localtime",
                    "/etc/ssl/certs",
                    "/etc/ssl/openssl.cnf",
                ]
                .into_iter()
                .map(PathBuf::from),
            );
            reads.sort();
            reads.dedup();
            for root in reads {
                if !root.exists() {
                    continue;
                }
                let root = crate::canonical(&root)?;
                if root == Path::new("/")
                    || denied
                        .iter()
                        .any(|path| path.starts_with(&root) || root.starts_with(path))
                {
                    return Err("evaluation read roots overlap private or evaluator paths".into());
                }
                this.add(
                    &root,
                    if root.is_dir() {
                        READ
                    } else {
                        READ & !(1 << 3)
                    },
                )?;
            }
            for device in [
                "/dev/null",
                "/dev/zero",
                "/dev/random",
                "/dev/urandom",
                "/dev/tty",
            ] {
                if Path::new(device).exists() {
                    this.add(Path::new(device), (1 << 1) | (1 << 2))?;
                }
            }
            Ok(this)
        }
        fn add(&self, path: &Path, access: u64) -> Result<(), String> {
            let path =
                CString::new(path.as_os_str().as_bytes()).map_err(|_| "invalid Landlock path")?;
            let fd = unsafe { libc::open(path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
            if fd < 0 {
                return Err(format!(
                    "evaluation path unavailable: {}",
                    io::Error::last_os_error()
                ));
            }
            let file = unsafe { File::from_raw_fd(fd) };
            let rule = PathRule {
                access,
                parent_fd: file.as_raw_fd(),
            };
            if unsafe {
                libc::syscall(
                    libc::SYS_landlock_add_rule,
                    self.ruleset.as_raw_fd(),
                    1,
                    &rule,
                    0,
                )
            } < 0
            {
                return Err(format!(
                    "evaluation Landlock rule failed: {}",
                    io::Error::last_os_error()
                ));
            }
            Ok(())
        }
        pub(crate) fn fd(&self) -> RawFd {
            self.ruleset.as_raw_fd()
        }
    }
    fn readonly_mount(path: &Path) -> Result<bool, String> {
        let path =
            CString::new(path.as_os_str().as_bytes()).map_err(|_| "invalid metadata path")?;
        let mut info: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(path.as_ptr(), &mut info) } < 0 {
            return Err(io::Error::last_os_error().to_string());
        }
        Ok(info.f_flag & libc::ST_RDONLY != 0)
    }
    fn normalized_future(path: &Path) -> Result<PathBuf, String> {
        if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err(
                "evaluation private paths must be absolute without parent traversal".into(),
            );
        }
        let mut ancestor = path;
        let mut tail = Vec::new();
        while !ancestor.exists() {
            tail.push(
                ancestor
                    .file_name()
                    .ok_or("private path has no existing ancestor")?
                    .to_owned(),
            );
            ancestor = ancestor
                .parent()
                .ok_or("private path has no existing ancestor")?;
        }
        let mut normalized = ancestor.canonicalize().map_err(|e| e.to_string())?;
        for name in tail.into_iter().rev() {
            normalized.push(name);
        }
        Ok(normalized)
    }
    /// Only async-signal-safe syscalls run after fork, before the first user instruction.
    pub unsafe fn install(
        ruleset: RawFd,
        filter_address: usize,
        filter_len: u16,
    ) -> io::Result<()> {
        if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } < 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { libc::syscall(libc::SYS_landlock_restrict_self, ruleset, 0) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let program = libc::sock_fprog {
            len: filter_len,
            filter: filter_address as *mut libc::sock_filter,
        };
        if unsafe { libc::prctl(libc::PR_SET_SECCOMP, 2, &program, 0, 0) } < 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe {
            libc::syscall(
                libc::SYS_close_range,
                3_u32,
                u32::MAX,
                libc::CLOSE_RANGE_CLOEXEC,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}
