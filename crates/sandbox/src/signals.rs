//! Child-side signal hygiene for every confined process launch.
//!
//! `SIG_IGN` dispositions survive `exec`, so a daemon started with `nohup`, from a
//! non-interactive shell job (`&`) or by a supervisor that ignores signals would
//! otherwise hand its children an ignored `SIGINT`/`SIGQUIT`/`SIGHUP`, and tool
//! interrupts would silently stop working. Handlers installed by the parent are
//! also still live between `fork` and `exec`; resetting them first keeps a signal
//! delivered in that window from running parent code in the child.

/// Restores every catchable signal to `SIG_DFL` and empties the blocked mask.
///
/// Only call this from a `pre_exec` hook in a freshly forked child: it changes
/// process-wide state. It performs no allocation, takes no locks and uses only
/// the async-signal-safe `sigemptyset`, `sigaction` and `sigprocmask` calls.
/// Numbers reserved by the C library (glibc NPTL) or absent on the target reject
/// the change with `EINVAL`; they are skipped because user code cannot see them.
///
/// # Safety
/// Must run only in the child between `fork` and `exec`.
#[cfg(unix)]
pub unsafe fn reset_child_signal_state() -> std::io::Result<()> {
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = libc::SIG_DFL;
        action.sa_flags = 0;
        if libc::sigemptyset(&mut action.sa_mask) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        for signal in 1..SIGNAL_LIMIT {
            if signal == libc::SIGKILL || signal == libc::SIGSTOP {
                continue;
            }
            // With a valid zeroed action the only failure is EINVAL for a reserved or
            // nonexistent number, which the executed program cannot observe.
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
        let mut empty: libc::sigset_t = std::mem::zeroed();
        if libc::sigemptyset(&mut empty) != 0
            || libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut()) != 0
        {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// One past the highest signal number to reset. Linux real-time signals end at 64.
#[cfg(target_os = "linux")]
const SIGNAL_LIMIT: libc::c_int = 65;
#[cfg(all(unix, not(target_os = "linux")))]
const SIGNAL_LIMIT: libc::c_int = 33;
