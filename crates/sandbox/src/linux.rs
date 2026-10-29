use super::{SandboxConfig, SandboxProfile, canonical, ordinary_directories, protected_paths};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    os::{
        fd::{AsRawFd, FromRawFd, RawFd},
        unix::fs::{MetadataExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    process::Command,
};
/// Owns the sealed BPF descriptor and private masks until the child has finished.
pub struct PreparedSandbox {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    filter: File,
    proxy: Option<super::network::HostProxy>,
    eval: Option<super::evaluation::linux::PreparedEval>,
    eval_filter: Vec<libc::sock_filter>,
    _private: tempfile::TempDir,
}
impl PreparedSandbox {
    pub fn for_shell(config: &SandboxConfig, shell: &Path) -> Result<Self, String> {
        if config.profile != SandboxProfile::Eval {
            return Self::new(config);
        }
        let writable = config
            .write_roots
            .iter()
            .map(|path| canonical(path))
            .collect::<Result<Vec<_>, _>>()?;
        let eval = super::evaluation::linux::PreparedEval::new(config)?;
        let mut filter = filter(false, config.eval_network, config.eval_network)?;
        let mut bytes = Vec::new();
        filter.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
        filter.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
        let eval_filter = bytes
            .as_chunks::<8>()
            .0
            .iter()
            .map(|b| libc::sock_filter {
                code: u16::from_ne_bytes([b[0], b[1]]),
                jt: b[2],
                jf: b[3],
                k: u32::from_ne_bytes([b[4], b[5], b[6], b[7]]),
            })
            .collect();
        Ok(Self {
            program: shell.canonicalize().map_err(|e| e.to_string())?,
            args: Vec::new(),
            filter,
            proxy: None,
            eval: Some(eval),
            eval_filter,
            _private: runtime_tempdir(&writable)?,
        })
    }
    pub fn new(config: &SandboxConfig) -> Result<Self, String> {
        match config.profile {
            SandboxProfile::ReadOnly
            | SandboxProfile::Project
            | SandboxProfile::ProjectRestricted
            | SandboxProfile::ProjectNet
            | SandboxProfile::FileAnalysis
            | SandboxProfile::ToolPackage => {}
            _ => {
                return Err(
                    "requested sandbox profile has no verified Linux implementation".into(),
                );
            }
        }
        if matches!(
            config.profile,
            SandboxProfile::ReadOnly | SandboxProfile::FileAnalysis
        ) && !config.write_roots.is_empty()
        {
            return Err("read-only sandbox cannot grant write roots".into());
        }
        let workspace = canonical(&config.workspace)?;
        if config.hidden_paths.iter().any(|path| path == &workspace) {
            return Err("application data root overlaps the entire workspace; configure separate storage and project directories".into());
        }
        if !workspace.is_dir() {
            return Err("sandbox working directory must be a directory".into());
        }
        let mut read = config
            .read_roots
            .iter()
            .map(|p| canonical(p))
            .collect::<Result<Vec<_>, _>>()?;
        let mut write = config
            .write_roots
            .iter()
            .map(|p| canonical(p))
            .collect::<Result<Vec<_>, _>>()?;
        for path in &read {
            if ["/proc", "/sys", "/dev", "/run"]
                .iter()
                .any(|reserved| path.starts_with(reserved))
            {
                return Err(
                    "host kernel and runtime filesystems cannot be sandbox read roots".into(),
                );
            }
            if !path.is_dir() && !path.is_file() {
                return Err("read roots must be regular files or directories".into());
            }
        }
        for path in &write {
            if !path.starts_with(&workspace) {
                return Err("sandbox roots must be within the fixed project boundary".into());
            }
        }
        read.sort();
        read.dedup();
        write.sort();
        write.dedup();
        let networked = config.profile == SandboxProfile::ProjectNet;
        let bridge = if networked {
            let path = config
                .proxy_bridge
                .as_deref()
                .ok_or("project-net requires a trusted internal network bridge")?;
            Some(validated_native_tool(path, &workspace, &write)?)
        } else {
            None
        };
        let program = validated_bubblewrap(config.bubblewrap.as_deref(), &workspace, &write)?;
        let private = runtime_tempdir(&write)?;
        let proxy = if networked {
            Some(super::network::HostProxy::start_in(
                config
                    .network
                    .clone()
                    .ok_or("project-net requires an enforcing host proxy configuration")?,
                private.path(),
            )?)
        } else {
            None
        };
        let empty_dir = private.path().join("masked-directory");
        std::fs::create_dir(&empty_dir).map_err(|e| e.to_string())?;
        std::fs::set_permissions(&empty_dir, std::fs::Permissions::from_mode(0o0))
            .map_err(|e| e.to_string())?;
        let empty_file = private.path().join("masked-file");
        std::fs::write(&empty_file, []).map_err(|e| e.to_string())?;
        std::fs::set_permissions(&empty_file, std::fs::Permissions::from_mode(0o0))
            .map_err(|e| e.to_string())?;
        let filter = filter(
            matches!(config.profile, SandboxProfile::ProjectRestricted),
            networked,
            false,
        )?;
        let mut args: Vec<OsString> = [
            "--unshare-all",
            "--unshare-user",
            "--uid",
            "1000",
            "--gid",
            "1000",
            "--die-with-parent",
            "--disable-userns",
            "--new-session",
            "--cap-drop",
            "ALL",
            "--clearenv",
        ]
        .into_iter()
        .map(Into::into)
        .collect();
        for dir in ["/usr", "/bin", "/sbin", "/lib", "/lib64"] {
            if Path::new(dir).exists() {
                push(&mut args, "--ro-bind", Path::new(dir), Path::new(dir));
            }
        }
        args.extend(["--dir", "/etc"].into_iter().map(OsString::from));
        for path in [
            "/etc/ld.so.cache",
            "/etc/ld.so.conf",
            "/etc/ld.so.conf.d",
            "/etc/passwd",
            "/etc/group",
            "/etc/nsswitch.conf",
            "/etc/localtime",
            "/etc/terminfo",
            "/etc/ssl/certs",
            "/etc/ssl/openssl.cnf",
        ] {
            if Path::new(path).exists() {
                push(&mut args, "--ro-bind", Path::new(path), Path::new(path));
            }
        }
        // Debian-style compiler/awk entry points are symlinks through this
        // directory. Projecting only links whose final targets are already in
        // the read-only system runtime does not expose additional host files.
        if Path::new("/etc/alternatives").is_dir() {
            args.extend(
                ["--dir", "/etc/alternatives"]
                    .into_iter()
                    .map(OsString::from),
            );
            for entry in std::fs::read_dir("/etc/alternatives").map_err(|e| e.to_string())? {
                let entry = entry.map_err(|e| e.to_string())?;
                if !entry.file_type().map_err(|e| e.to_string())?.is_symlink() {
                    continue;
                }
                if let Ok(target) = entry.path().canonicalize()
                    && target.starts_with("/usr")
                {
                    args.extend([
                        OsString::from("--symlink"),
                        target.into_os_string(),
                        entry.path().into_os_string(),
                    ]);
                }
            }
        }
        args.extend(
            [
                "--proc",
                "/proc",
                "--dev",
                "/dev",
                "--tmpfs",
                "/tmp",
                "--dir",
                "/home",
                "--dir",
                "/home/excalibur",
            ]
            .into_iter()
            .map(Into::into),
        );
        for (key, value) in [
            ("PATH", "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"),
            ("HOME", "/home/excalibur"),
            ("PAGER", "cat"),
            ("GIT_PAGER", "cat"),
            ("TMPDIR", "/tmp"),
            ("TMP", "/tmp"),
            ("TEMP", "/tmp"),
        ] {
            args.extend([OsString::from("--setenv"), key.into(), value.into()]);
        }
        let mut protected = Vec::new();
        args.extend([OsString::from("--dir"), workspace.clone().into_os_string()]);
        for root in &read {
            push(&mut args, "--ro-bind", root, root);
        }
        for root in &write {
            if !root.is_dir() {
                return Err("writable sandbox roots must be directories".into());
            }
            protected.extend(protected_paths(root, true, &config.hidden_paths)?);
            push(&mut args, "--bind", root, root);
        }
        protected.sort();
        protected.dedup();
        for path in &protected {
            push(&mut args, "--ro-bind", path, path);
        }
        // Existing credential material is hidden even when it is inside an authorized project.
        let mut hidden = config.hidden_paths.clone();
        for root in read.iter().chain(write.iter()) {
            if root.is_dir() {
                for dir in ordinary_directories(root, &config.hidden_paths)? {
                    for entry in std::fs::read_dir(&dir).map_err(|e| e.to_string())? {
                        let entry = entry.map_err(|e| e.to_string())?;
                        let name = entry.file_name();
                        let name = name.to_string_lossy();
                        if name == ".env"
                            || name.starts_with(".env.")
                            || name.ends_with(".pem")
                            || name.ends_with(".key")
                        {
                            hidden.push(entry.path());
                        }
                    }
                    for name in [".ssh", ".aws", ".config"] {
                        let path = dir.join(name);
                        if path.exists() {
                            hidden.push(path);
                        }
                    }
                }
            }
        }
        hidden.sort();
        hidden.dedup();
        let mut nonwritable = hidden.clone();
        nonwritable.extend(protected);
        // Existing read-only metadata may be a file (for example a worktree's
        // .git entry), so skipping traversal of reserved directories is not
        // enough to exclude every protected inode from ordinary accounting.
        for root in &read {
            if root.is_dir() {
                nonwritable.extend(protected_paths(root, false, &hidden)?);
            }
        }
        // Preserve the stricter write boundary: an alias in writable project
        // space must not make a read-only SDK inode writable through that name.
        hidden.extend(cross_boundary_hardlink_aliases(&write, &nonwritable)?);
        // Read-only mounts also need inode accounting: a public name can be a
        // hardlink to hidden material even when the process cannot write it.
        let mut visible_roots = vec![workspace.clone()];
        visible_roots.extend(read.iter().chain(write.iter()).cloned());
        visible_roots.sort();
        visible_roots.dedup();
        hidden.extend(cross_boundary_hardlink_aliases(
            &visible_roots,
            &nonwritable,
        )?);
        hidden.sort();
        hidden.dedup();
        for path in hidden {
            if !read
                .iter()
                .chain(write.iter())
                .any(|root| path.starts_with(root))
                || !path.exists()
            {
                continue;
            }
            let meta = std::fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
            if meta.file_type().is_symlink() {
                return Err("hidden sandbox paths cannot be symlinks".into());
            }
            push(
                &mut args,
                "--ro-bind",
                if meta.is_dir() {
                    &empty_dir
                } else {
                    &empty_file
                },
                &path,
            );
        }
        if let (Some(proxy), Some(bridge)) = (&proxy, &bridge) {
            args.extend(
                ["--dir", "/run", "--dir", "/run/excalibur"]
                    .into_iter()
                    .map(OsString::from),
            );
            push(
                &mut args,
                "--ro-bind",
                proxy.socket_path(),
                Path::new("/run/excalibur/proxy.sock"),
            );
            push(
                &mut args,
                "--ro-bind",
                bridge,
                Path::new("/run/excalibur/network-bridge"),
            );
        }
        args.extend([
            OsString::from("--chdir"),
            workspace.into_os_string(),
            "--remount-ro".into(),
            "/".into(),
            "--seccomp".into(),
            filter.as_raw_fd().to_string().into(),
        ]);
        Ok(Self {
            program,
            args,
            filter,
            proxy,
            eval: None,
            eval_filter: Vec::new(),
            _private: private,
        })
    }
    pub fn configure(&self, command: &mut Command, shell: &Path, shell_args: &[String], tty: bool) {
        if self.eval.is_some() {
            command.args(shell_args);
            return;
        }
        command
            .args(&self.args)
            .arg("--setenv")
            .arg("TERM")
            .arg(if tty { "xterm-256color" } else { "dumb" });
        if !tty {
            command.args(["--setenv", "NO_COLOR", "1", "--setenv", "CLICOLOR", "0"]);
        }
        command.arg("--");
        if self.proxy.is_some() {
            command.args([
                "/run/excalibur/network-bridge",
                "--socket",
                "/run/excalibur/proxy.sock",
                "--listen",
                "127.0.0.1:3128",
                "--",
            ]);
        }
        command.arg(shell).args(shell_args);
    }
    pub fn filter_fd(&self) -> RawFd {
        self.filter.as_raw_fd()
    }
    pub fn evaluation_setup(&self) -> Option<super::evaluation::EvalChildSetup> {
        self.eval
            .as_ref()
            .map(|eval| super::evaluation::EvalChildSetup::new(eval.fd(), &self.eval_filter))
    }
}
fn runtime_tempdir(writable: &[PathBuf]) -> Result<tempfile::TempDir, String> {
    // Security resources never follow process-global TMPDIR/TMP/TEMP. Another
    // sandbox must not be able to mutate their bind sources before namespace setup.
    for candidate in ["/tmp", "/var/tmp"] {
        let Ok(parent) = Path::new(candidate).canonicalize() else {
            continue;
        };
        let Ok(metadata) = std::fs::metadata(&parent) else {
            continue;
        };
        if !metadata.is_dir()
            || writable.iter().any(|root| parent.starts_with(root))
            || ![0, unsafe { libc::geteuid() }].contains(&metadata.uid())
            || metadata.mode() & 0o022 != 0 && metadata.mode() & 0o1000 == 0
        {
            continue;
        }
        return tempfile::Builder::new()
            .prefix("excalibur-sandbox-")
            .tempdir_in(parent)
            .map_err(|_| "private sandbox runtime directory is unavailable".into());
    }
    Err("no trusted sandbox runtime directory exists outside writable roots".into())
}
/// A visible alias must account for every link to its inode inside the same
/// visible authority. Otherwise another link may belong to protected metadata,
/// a hidden vault or another host directory. Hide aliases without inspecting
/// private trees or reading file content. Ordinary hardlinks wholly inside the
/// permitted union of read and write roots remain available.
pub(crate) fn cross_boundary_hardlink_aliases(
    visible: &[PathBuf],
    hidden: &[PathBuf],
) -> Result<Vec<PathBuf>, String> {
    let mut paths = BTreeSet::new();
    struct Inode {
        links: u64,
        names: BTreeSet<(u64, u64, OsString)>,
        paths: Vec<PathBuf>,
    }
    let mut inodes: BTreeMap<(u64, u64), Inode> = BTreeMap::new();
    let mut roots = visible.iter().collect::<Vec<_>>();
    roots.sort();
    let mut covered: Vec<&PathBuf> = Vec::new();
    for root in roots {
        if hidden.iter().any(|excluded| root.starts_with(excluded)) {
            continue;
        }
        if covered.iter().any(|parent| root.starts_with(parent)) {
            continue;
        }
        covered.push(root);
        let single_file = root.is_file();
        let directories = if single_file {
            vec![
                root.parent()
                    .ok_or("read root has no parent")?
                    .to_path_buf(),
            ]
        } else {
            ordinary_directories(root, hidden)?
        };
        for directory in directories {
            let parent = std::fs::metadata(&directory).map_err(|e| e.to_string())?;
            // A file read root grants only that entry, never a scan of siblings.
            let entries = if single_file {
                vec![root.to_path_buf()]
            } else {
                std::fs::read_dir(directory)
                    .map_err(|e| e.to_string())?
                    .map(|entry| entry.map(|entry| entry.path()))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| e.to_string())?
            };
            for path in entries {
                if hidden.iter().any(|excluded| path.starts_with(excluded))
                    || !paths.insert(path.clone())
                {
                    continue;
                }
                if paths.len() > super::MAX_ENTRIES {
                    return Err("sandbox visible hardlink inventory exceeds its entry limit".into());
                }
                let metadata = std::fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
                if !metadata.is_file() || metadata.nlink() <= 1 {
                    continue;
                }
                let inode = inodes
                    .entry((metadata.dev(), metadata.ino()))
                    .or_insert_with(|| Inode {
                        links: metadata.nlink(),
                        names: BTreeSet::new(),
                        paths: Vec::new(),
                    });
                if inode.links != metadata.nlink() {
                    return Err(
                        "sandbox visible hardlink inventory changed during preparation".into(),
                    );
                }
                // Bind aliases expose the same directory entry under different
                // absolute paths without increasing the inode's link count.
                inode.names.insert((
                    parent.dev(),
                    parent.ino(),
                    path.file_name()
                        .ok_or("read root has no filename")?
                        .to_os_string(),
                ));
                inode.paths.push(path);
            }
        }
    }
    let mut aliases = Vec::new();
    for ((device, inode), info) in inodes {
        for path in &info.paths {
            let metadata = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
            if metadata.dev() != device || metadata.ino() != inode || metadata.nlink() != info.links
            {
                return Err("sandbox visible hardlink inventory changed during preparation".into());
            }
        }
        if info.links < info.names.len() as u64 {
            return Err("sandbox hardlink directory entries changed during preparation".into());
        }
        if info.links > info.names.len() as u64 {
            aliases.extend(info.paths);
        }
    }
    Ok(aliases)
}
fn push(args: &mut Vec<OsString>, option: &str, source: &Path, target: &Path) {
    args.extend([
        option.into(),
        source.as_os_str().into(),
        target.as_os_str().into(),
    ]);
}
fn validated_bubblewrap(
    configured: Option<&Path>,
    workspace: &Path,
    writable: &[PathBuf],
) -> Result<PathBuf, String> {
    let candidate = if let Some(path) = configured {
        path.to_owned()
    } else {
        std::env::var_os("PATH")
            .into_iter()
            .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
            .map(|dir| dir.join("bwrap"))
            .find(|path| path.is_file())
            .ok_or("mandatory bubblewrap sandbox is unavailable; command was not executed")?
    };
    let path = candidate
        .canonicalize()
        .map_err(|e| format!("bubblewrap unavailable: {e}"))?;
    let meta = std::fs::metadata(&path).map_err(|e| e.to_string())?;
    if !meta.is_file()
        || meta.mode() & 0o6022 != 0
        || path.starts_with(workspace)
        || writable.iter().any(|root| path.starts_with(root))
    {
        return Err(
            "bubblewrap executable is not a trusted non-setuid, non-writable toolchain path".into(),
        );
    }
    let output = Command::new(&path)
        .arg("--version")
        .env_clear()
        .output()
        .map_err(|e| e.to_string())?;
    let version = String::from_utf8_lossy(&output.stdout);
    let value = version
        .split_whitespace()
        .nth(1)
        .ok_or("invalid bubblewrap version response")?;
    let parts = value
        .split('.')
        .take(3)
        .map(str::parse::<u32>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "invalid bubblewrap version")?;
    if !output.status.success() || parts.len() < 2 || parts[0] == 0 && parts[1] < 12 {
        return Err("bubblewrap build predates the verified upstream CVE-2026-87766 fix; install a verified patched build".into());
    }
    Ok(path)
}
fn validated_native_tool(
    path: &Path,
    workspace: &Path,
    writable: &[PathBuf],
) -> Result<PathBuf, String> {
    let path = path
        .canonicalize()
        .map_err(|_| "trusted network bridge is unavailable")?;
    let meta =
        std::fs::metadata(&path).map_err(|_| "trusted network bridge metadata unavailable")?;
    if !meta.is_file()
        || meta.mode() & 0o6022 != 0
        || path.starts_with(workspace)
        || writable.iter().any(|root| path.starts_with(root))
    {
        return Err("network bridge must be a trusted non-setuid, non-writable toolchain executable outside the project".into());
    }
    Ok(path)
}
fn filter(restricted: bool, networked: bool, container_network: bool) -> Result<File, String> {
    #[cfg(target_arch = "x86_64")]
    const ARCH: u32 = 0xc000003e;
    #[cfg(target_arch = "aarch64")]
    const ARCH: u32 = 0xc00000b7;
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    return Err("seccomp architecture policy is unavailable".into());
    const LD: u16 = 0x20;
    const JEQ: u16 = 0x15;
    const JSET: u16 = 0x45;
    const RET: u16 = 0x06;
    const ALLOW: u32 = 0x7fff0000;
    const ERRNO: u32 = 0x00050000;
    const KILL: u32 = 0x80000000;
    let mut b = Vec::<libc::sock_filter>::new();
    let mut add = |code, jt, jf, k| b.push(libc::sock_filter { code, jt, jf, k });
    add(LD, 0, 0, 4);
    add(JEQ, 1, 0, ARCH);
    add(RET, 0, 0, KILL);
    add(LD, 0, 0, 0);
    #[cfg(target_arch = "x86_64")]
    {
        add(JSET, 0, 1, 0x40000000);
        add(RET, 0, 0, KILL);
    }
    let mut denied = vec![
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_kcmp,
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
        libc::SYS_userfaultfd,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_chroot,
        libc::SYS_setns,
        libc::SYS_unshare,
        libc::SYS_open_by_handle_at,
        libc::SYS_name_to_handle_at,
        libc::SYS_fsopen,
        libc::SYS_fsconfig,
        libc::SYS_fsmount,
        libc::SYS_fspick,
        libc::SYS_open_tree,
        libc::SYS_move_mount,
        libc::SYS_mount_setattr,
        libc::SYS_init_module,
        libc::SYS_finit_module,
        libc::SYS_delete_module,
        libc::SYS_kexec_load,
        libc::SYS_kexec_file_load,
        libc::SYS_reboot,
        libc::SYS_swapon,
        libc::SYS_swapoff,
        libc::SYS_quotactl,
        libc::SYS_add_key,
        libc::SYS_request_key,
        libc::SYS_keyctl,
        libc::SYS_acct,
        libc::SYS_pidfd_getfd,
        libc::SYS_fanotify_init,
        libc::SYS_mknodat,
    ];
    if !networked {
        denied.extend([
            libc::SYS_socket,
            libc::SYS_connect,
            libc::SYS_bind,
            libc::SYS_listen,
            libc::SYS_accept,
            libc::SYS_accept4,
        ]);
    }
    #[cfg(target_arch = "x86_64")]
    denied.push(libc::SYS_mknod);
    if restricted {
        denied.extend([libc::SYS_mkdirat, libc::SYS_renameat, libc::SYS_renameat2]);
        #[cfg(target_arch = "x86_64")]
        denied.extend([libc::SYS_mkdir, libc::SYS_rename]);
    }
    denied.sort();
    denied.dedup();
    for syscall in denied {
        add(JEQ, 0, 1, syscall as u32);
        add(RET, 0, 0, ERRNO | libc::EPERM as u32);
    }
    if !networked {
        // Connected anonymous IPC may use send()/sendto(NULL, 0), as Python
        // does. No destination address can be supplied through this exception.
        add(JEQ, 0, 13, libc::SYS_sendto as u32);
        for offset in [48, 52, 56, 60] {
            add(LD, 0, 0, offset);
            add(JEQ, 1, 0, 0);
            add(RET, 0, 0, ERRNO | libc::EPERM as u32);
        }
        add(RET, 0, 0, ALLOW);
        add(LD, 0, 0, 0);
    }
    // sendmsg/recvmsg operate only on sockets admitted below: connected anonymous
    // IPC, plus namespaced TCP in proxy mode. There is no datagram/name-based
    // socket from which to acquire another peer or an external host descriptor.
    // Anonymous stream/seqpacket IPC is required by Rust's pre-exec error
    // channel. No pathname is involved; datagram pairs are excluded because
    // they can disconnect and reconnect to a filesystem UNIX socket.
    add(JEQ, 0, 12, libc::SYS_socketpair as u32);
    add(LD, 0, 0, 16);
    add(JEQ, 1, 0, libc::AF_UNIX as u32);
    add(RET, 0, 0, ERRNO | libc::EPERM as u32);
    add(LD, 0, 0, 24);
    add(
        0x54,
        0,
        0,
        !(libc::SOCK_CLOEXEC as u32 | libc::SOCK_NONBLOCK as u32),
    );
    add(JEQ, 2, 0, libc::SOCK_STREAM as u32);
    add(JEQ, 1, 0, libc::SOCK_SEQPACKET as u32);
    add(RET, 0, 0, ERRNO | libc::EPERM as u32);
    add(LD, 0, 0, 32);
    add(JEQ, 1, 0, 0);
    add(RET, 0, 0, ERRNO | libc::EPERM as u32);
    add(RET, 0, 0, ALLOW);
    add(LD, 0, 0, 0);
    if container_network {
        add(JEQ, 0, 15, libc::SYS_socket as u32);
        add(LD, 0, 0, 16);
        add(JEQ, 2, 0, libc::AF_INET as u32);
        add(JEQ, 1, 0, libc::AF_INET6 as u32);
        add(RET, 0, 0, ERRNO | libc::EPERM as u32);
        add(LD, 0, 0, 24);
        add(
            0x54,
            0,
            0,
            !(libc::SOCK_CLOEXEC as u32 | libc::SOCK_NONBLOCK as u32),
        );
        add(JEQ, 2, 0, libc::SOCK_STREAM as u32);
        add(JEQ, 1, 0, libc::SOCK_DGRAM as u32);
        add(RET, 0, 0, ERRNO | libc::EPERM as u32);
        add(LD, 0, 0, 32);
        add(JEQ, 3, 0, 0);
        add(JEQ, 2, 0, libc::IPPROTO_TCP as u32);
        add(JEQ, 1, 0, libc::IPPROTO_UDP as u32);
        add(RET, 0, 0, ERRNO | libc::EPERM as u32);
        add(RET, 0, 0, ALLOW);
        add(LD, 0, 0, 0);
    } else if networked {
        // The trusted bridge needs UNIX stream and loopback TCP sockets. It adds
        // an irreversible second filter denying UNIX sockets in its user child.
        // Neither process receives UDP/raw/packet sockets or an external route.
        add(JEQ, 0, 11, libc::SYS_socket as u32);
        add(LD, 0, 0, 16);
        add(JEQ, 3, 0, libc::AF_UNIX as u32);
        add(JEQ, 2, 0, libc::AF_INET as u32);
        add(JEQ, 1, 0, libc::AF_INET6 as u32);
        add(RET, 0, 0, ERRNO | libc::EPERM as u32);
        add(LD, 0, 0, 24);
        // SOCK_CLOEXEC/NONBLOCK do not change the socket's transport type.
        add(0x54, 0, 0, 0xf);
        add(JEQ, 1, 0, libc::SOCK_STREAM as u32);
        add(RET, 0, 0, ERRNO | libc::EPERM as u32);
        add(RET, 0, 0, ALLOW);
        add(LD, 0, 0, 0);
    }
    // clone3 carries flags behind a pointer. Returning ENOSYS selects libc's
    // ordinary clone fallback, whose namespace flags can be inspected safely.
    add(JEQ, 0, 1, libc::SYS_clone3 as u32);
    add(RET, 0, 0, ERRNO | libc::ENOSYS as u32);
    add(JEQ, 0, 3, libc::SYS_clone as u32);
    add(LD, 0, 0, 16);
    add(
        JSET,
        0,
        1,
        (libc::CLONE_NEWNS
            | libc::CLONE_NEWCGROUP
            | libc::CLONE_NEWUTS
            | libc::CLONE_NEWIPC
            | libc::CLONE_NEWUSER
            | libc::CLONE_NEWPID
            | libc::CLONE_NEWNET) as u32,
    );
    add(RET, 0, 0, ERRNO | libc::EPERM as u32);
    // Disallow terminal input injection even when the process owns a PTY.
    add(LD, 0, 0, 0);
    add(JEQ, 0, 5, libc::SYS_ioctl as u32);
    add(LD, 0, 0, 24);
    add(JEQ, 1, 0, 0x5412);
    add(JEQ, 0, 1, 0x541c);
    add(RET, 0, 0, ERRNO | libc::EPERM as u32);
    add(RET, 0, 0, ALLOW);
    add(RET, 0, 0, ALLOW);
    let name = std::ffi::CString::new("excalibur-seccomp").unwrap();
    let fd =
        unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    for instruction in b {
        file.write_all(&instruction.code.to_ne_bytes())
            .and_then(|()| file.write_all(&[instruction.jt, instruction.jf]))
            .and_then(|()| file.write_all(&instruction.k.to_ne_bytes()))
            .map_err(|e| e.to_string())?;
    }
    file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    let flags = libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
    if unsafe { libc::fcntl(fd, libc::F_ADD_SEALS, flags) } < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(file)
}
