//! Private sandbox bootstrap. Domain authorization stays in the host-side broker.
//! This process relays only loopback TCP to its configured UDS; it does not parse,
//! resolve or authorize HTTP/SOCKS destinations. User children cannot open named
//! UNIX sockets; anonymous connected stream/seqpacket IPC remains available.
use std::{
    ffi::OsString,
    io,
    net::{Ipv4Addr, Shutdown, SocketAddr, SocketAddrV4, TcpListener, TcpStream},
    os::unix::{
        fs::FileTypeExt,
        net::UnixStream,
        process::{CommandExt, ExitStatusExt},
    },
    path::{Component, Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};
const MAX_CONNECTIONS: usize = 64;

/// Parse the private helper's bounded host-supplied argument contract.
/// This is an internal adapter entry point, not a product command interface.
pub fn run_arguments(arguments: impl IntoIterator<Item = OsString>) -> Result<i32, String> {
    let mut bounded = Vec::new();
    let mut total = 0_usize;
    for argument in arguments {
        total = total.saturating_add(argument.as_encoded_bytes().len());
        if bounded.len() == 256 || total > 1024 * 1024 {
            return Err("private network bootstrap arguments exceed limits".into());
        }
        bounded.push(argument);
    }
    let arguments = bounded;
    if arguments.len() < 6
        || arguments.len() > 256
        || arguments[0] != "--socket"
        || arguments[2] != "--listen"
        || arguments[4] != "--"
    {
        return Err("invalid private network bootstrap arguments".into());
    }
    let socket = PathBuf::from(&arguments[1]);
    let program = PathBuf::from(&arguments[5]);
    let address = arguments[3]
        .to_str()
        .and_then(|a| a.parse::<SocketAddrV4>().ok())
        .filter(|a| *a.ip() == Ipv4Addr::LOCALHOST)
        .ok_or("network bootstrap must listen on IPv4 loopback")?;
    run(&socket, address, &program, &arguments[6..])
}

fn run(
    socket: &Path,
    address: SocketAddrV4,
    program: &Path,
    arguments: &[OsString],
) -> Result<i32, String> {
    if !socket.is_absolute()
        || !program.is_absolute()
        || socket
            .components()
            .chain(program.components())
            .any(|c| matches!(c, Component::ParentDir))
    {
        return Err("network bootstrap paths must be absolute without traversal".into());
    }
    if !std::fs::symlink_metadata(socket)
        .map_err(|_| "proxy socket is unavailable")?
        .file_type()
        .is_socket()
    {
        return Err("proxy endpoint must be a UNIX socket".into());
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    return Err("network bootstrap has no verified syscall filter for this architecture".into());
    // Remains undumpable after the child exec resets its own dumpability. This
    // prevents the child from recovering the parent's trusted UDS descriptors.
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        return Err("network bootstrap cannot protect its descriptors".into());
    }
    drop(UnixStream::connect(socket).map_err(|_| "proxy socket cannot be reached")?);
    let listener = TcpListener::bind(address).map_err(|_| "private proxy listener cannot bind")?;
    let address = listener
        .local_addr()
        .map_err(|_| "private proxy address unavailable")?;
    listener
        .set_nonblocking(true)
        .map_err(|_| "private proxy listener configuration failed")?;
    let stopping = Arc::new(AtomicBool::new(false));
    let stop = stopping.clone();
    let socket = socket.to_path_buf();
    let acceptor = thread::Builder::new()
        .name("excalibur-proxy-loopback".into())
        .spawn(move || accept(listener, socket, stop))
        .map_err(|_| "private proxy acceptor unavailable")?;
    let mut command = Command::new(program);
    command.args(arguments);
    let http = format!("http://{address}");
    let socks = format!("socks5h://{address}");
    command
        .env("HTTP_PROXY", &http)
        .env("http_proxy", &http)
        .env("HTTPS_PROXY", &http)
        .env("https_proxy", &http)
        .env("ALL_PROXY", &socks)
        .env("all_proxy", &socks)
        .env_remove("NO_PROXY")
        .env_remove("no_proxy");
    // std::process owns fork/exec bookkeeping. Only constant stack data and
    // async-signal-safe syscalls execute in the child-side hook.
    unsafe {
        command.pre_exec(child_filter);
    }
    let status = command.spawn().and_then(|mut child| child.wait());
    stopping.store(true, Ordering::Release);
    let _ = acceptor.join();
    let status = status.map_err(|error| {
        format!(
            "network bootstrap child could not run (OS error {})",
            error.raw_os_error().unwrap_or(-1)
        )
    })?;
    Ok(status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)))
}
fn accept(listener: TcpListener, socket: PathBuf, stopping: Arc<AtomicBool>) {
    let active = Arc::new(AtomicUsize::new(0));
    while !stopping.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, SocketAddr::V4(peer))) if *peer.ip() == Ipv4Addr::LOCALHOST => {
                if active
                    .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                        (n < MAX_CONNECTIONS).then_some(n + 1)
                    })
                    .is_err()
                {
                    continue;
                }
                let socket = socket.clone();
                let counter = active.clone();
                if thread::Builder::new()
                    .name("excalibur-proxy-relay".into())
                    .spawn(move || {
                        let _ = relay(stream, &socket);
                        counter.fetch_sub(1, Ordering::AcqRel);
                    })
                    .is_err()
                {
                    active.fetch_sub(1, Ordering::AcqRel);
                }
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5))
            }
            Err(_) => break,
        }
    }
}
fn relay(mut tcp: TcpStream, socket: &Path) -> io::Result<()> {
    let mut unix = UnixStream::connect(socket)?;
    let mut tcp_read = tcp.try_clone()?;
    let mut unix_write = unix.try_clone()?;
    let sending = thread::Builder::new()
        .name("excalibur-proxy-upload".into())
        .spawn(move || {
            let _ = io::copy(&mut tcp_read, &mut unix_write);
            let _ = unix_write.shutdown(Shutdown::Write);
        })?;
    let _ = io::copy(&mut unix, &mut tcp);
    let _ = tcp.shutdown(Shutdown::Write);
    let _ = sending.join();
    Ok(())
}
#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xc000003e;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xc00000b7;
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
const AUDIT_ARCH: u32 = 0;
const fn instruction(code: u16, jt: u8, jf: u8, k: u32) -> libc::sock_filter {
    libc::sock_filter { code, jt, jf, k }
}
fn child_filter() -> io::Result<()> {
    const LD: u16 = 0x20;
    const JEQ: u16 = 0x15;
    const JSET: u16 = 0x45;
    const AND: u16 = 0x54;
    const RET: u16 = 0x06;
    const DENY: u32 = 0x00050000 | libc::EPERM as u32;
    const ALLOW: u32 = 0x7fff0000;
    let mut instructions = [
        instruction(LD, 0, 0, 4),
        instruction(JEQ, 1, 0, AUDIT_ARCH),
        instruction(RET, 0, 0, 0x80000000),
        instruction(LD, 0, 0, 0),
        instruction(JSET, 0, 1, 0x40000000),
        instruction(RET, 0, 0, DENY),
        instruction(JEQ, 0, 13, libc::SYS_socket as u32),
        instruction(LD, 0, 0, 16),
        instruction(JEQ, 2, 0, libc::AF_INET as u32),
        instruction(JEQ, 1, 0, libc::AF_INET6 as u32),
        instruction(RET, 0, 0, DENY),
        instruction(LD, 0, 0, 24),
        instruction(
            AND,
            0,
            0,
            !(libc::SOCK_CLOEXEC as u32 | libc::SOCK_NONBLOCK as u32),
        ),
        instruction(JEQ, 1, 0, libc::SOCK_STREAM as u32),
        instruction(RET, 0, 0, DENY),
        instruction(LD, 0, 0, 32),
        instruction(JEQ, 2, 0, 0),
        instruction(JEQ, 1, 0, libc::IPPROTO_TCP as u32),
        instruction(RET, 0, 0, DENY),
        instruction(RET, 0, 0, ALLOW),
        // Anonymous connected stream/seqpacket pairs are needed by Rust's
        // process error channel. Datagram pairs can be retargeted and remain
        // denied; named UNIX sockets are still denied by socket() above.
        instruction(JEQ, 0, 12, libc::SYS_socketpair as u32),
        instruction(LD, 0, 0, 16),
        instruction(JEQ, 1, 0, libc::AF_UNIX as u32),
        instruction(RET, 0, 0, DENY),
        instruction(LD, 0, 0, 24),
        instruction(
            AND,
            0,
            0,
            !(libc::SOCK_CLOEXEC as u32 | libc::SOCK_NONBLOCK as u32),
        ),
        instruction(JEQ, 2, 0, libc::SOCK_STREAM as u32),
        instruction(JEQ, 1, 0, libc::SOCK_SEQPACKET as u32),
        instruction(RET, 0, 0, DENY),
        instruction(LD, 0, 0, 32),
        instruction(JEQ, 1, 0, 0),
        instruction(RET, 0, 0, DENY),
        instruction(RET, 0, 0, ALLOW),
        instruction(RET, 0, 0, ALLOW),
    ];
    let program = libc::sock_fprog {
        len: instructions.len() as u16,
        filter: instructions.as_mut_ptr(),
    };
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // All helper listeners, UDS relays and unrelated inherited descriptors close
    // on successful exec; std::process's error pipe remains usable before exec.
    if unsafe {
        libc::syscall(
            libc::SYS_close_range,
            3_u32,
            u32::MAX,
            libc::CLOSE_RANGE_CLOEXEC,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if unsafe {
        libc::prctl(
            libc::PR_SET_SECCOMP,
            2,
            &program as *const libc::sock_fprog,
            0,
            0,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
