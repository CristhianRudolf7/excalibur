//! Bounded installed-binary version probe, shared by preflight and launch admission.
use serde::Serialize;
use std::{
    io::Read,
    os::{fd::AsRawFd, unix::process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
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
    let mut command = Command::new(binary);
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C.UTF-8")
        .arg("--version")
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // A concurrent writer, including a briefly inherited pre-exec descriptor,
    // can make an otherwise executable fixture or updated binary temporarily busy.
    let child = loop {
        if Instant::now() >= deadline {
            return Err("Chrome version check timed out".into());
        }
        match command.spawn() {
            Ok(child) => break child,
            Err(error) if error.raw_os_error() == Some(libc::ETXTBSY) => {
                std::thread::sleep(Duration::from_millis(5))
            }
            Err(error) => return Err(error.to_string()),
        }
    };
    let mut probe = Probe(child);
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

/// Ask only whether the user's Secret Service responds. No secret is requested.
pub fn secret_service_available() -> bool {
    use std::os::unix::process::CommandExt;
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
        return false;
    }
    let Ok(mut child) = std::process::Command::new("/usr/bin/gdbus")
        .args([
            "call",
            "--session",
            "--dest",
            "org.freedesktop.secrets",
            "--object-path",
            "/org/freedesktop/secrets",
            "--method",
            "org.freedesktop.DBus.Peer.Ping",
        ])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            std::env::var_os("DBUS_SESSION_BUS_ADDRESS").unwrap_or_default(),
        )
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
    else {
        return false;
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let ready = loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Err(_) => break false,
            _ => {}
        }
        if std::time::Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.wait();
    ready
}
