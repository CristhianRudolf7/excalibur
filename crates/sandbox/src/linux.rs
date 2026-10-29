use super::{SandboxConfig, SandboxProfile, canonical, ordinary_directories, protected_paths};
use std::{
    ffi::OsString,
    fs::File,
    io::{Seek, SeekFrom, Write},
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
    _private: tempfile::TempDir,
}
impl PreparedSandbox {
    pub fn new(config: &SandboxConfig) -> Result<Self, String> {
        match config.profile {
            SandboxProfile::ReadOnly
            | SandboxProfile::ProjectRestricted
            | SandboxProfile::FileAnalysis
            | SandboxProfile::ToolPackage => {}
            SandboxProfile::ProjectNet => {
                return Err(
                    "project-net requires an enforcing proxy; unavailable, execution denied".into(),
                );
            }
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
        for path in read.iter().chain(write.iter()) {
            if !path.starts_with(&workspace) {
                return Err("sandbox roots must be within the fixed project boundary".into());
            }
        }
        read.sort();
        read.dedup();
        write.sort();
        write.dedup();
        let program = validated_bubblewrap(config.bubblewrap.as_deref(), &workspace, &write)?;
        let private = tempfile::Builder::new()
            .prefix("excalibur-sandbox-")
            .tempdir()
            .map_err(|e| e.to_string())?;
        let empty_dir = private.path().join("masked-directory");
        std::fs::create_dir(&empty_dir).map_err(|e| e.to_string())?;
        std::fs::set_permissions(&empty_dir, std::fs::Permissions::from_mode(0o0))
            .map_err(|e| e.to_string())?;
        let empty_file = private.path().join("masked-file");
        std::fs::write(&empty_file, []).map_err(|e| e.to_string())?;
        std::fs::set_permissions(&empty_file, std::fs::Permissions::from_mode(0o0))
            .map_err(|e| e.to_string())?;
        let filter = filter(matches!(
            config.profile,
            SandboxProfile::ProjectRestricted | SandboxProfile::ToolPackage
        ))?;
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
        ] {
            if Path::new(path).exists() {
                push(&mut args, "--ro-bind", Path::new(path), Path::new(path));
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
        for path in protected {
            push(&mut args, "--ro-bind", &path, &path);
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
        for path in hidden {
            if !path.starts_with(&workspace) || !path.exists() {
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
            _private: private,
        })
    }
    pub fn configure(&self, command: &mut Command, shell: &Path, shell_args: &[String], tty: bool) {
        command
            .args(&self.args)
            .arg("--setenv")
            .arg("TERM")
            .arg(if tty { "xterm-256color" } else { "dumb" });
        if !tty {
            command.args(["--setenv", "NO_COLOR", "1", "--setenv", "CLICOLOR", "0"]);
        }
        command.arg("--").arg(shell).args(shell_args);
    }
    pub fn filter_fd(&self) -> RawFd {
        self.filter.as_raw_fd()
    }
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
fn filter(restricted: bool) -> Result<File, String> {
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
        libc::SYS_socket,
        libc::SYS_socketpair,
        libc::SYS_connect,
        libc::SYS_bind,
        libc::SYS_listen,
        libc::SYS_accept,
        libc::SYS_accept4,
        libc::SYS_sendto,
        libc::SYS_sendmsg,
        libc::SYS_sendmmsg,
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
