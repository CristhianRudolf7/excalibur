//! Linux service-manager notifications. Heartbeats run on the interactive executor.
use std::{ffi::OsString, sync::Arc, time::Duration};
use tokio::{
    runtime::Handle,
    sync::{Notify, watch},
    task::JoinHandle,
};

/// Environment is captured once by the host; it is never modified in a threaded process.
pub struct ServiceNotifier {
    #[cfg(target_os = "linux")]
    socket: std::os::unix::net::UnixDatagram,
    interval: Option<Duration>,
}
impl ServiceNotifier {
    pub fn from_environment() -> Result<Option<Self>, String> {
        Self::from_values(
            std::env::var_os("NOTIFY_SOCKET"),
            std::env::var_os("WATCHDOG_USEC"),
            std::env::var_os("WATCHDOG_PID"),
            std::process::id(),
        )
    }
    /// Explicit values also allow hosts and local fixtures to avoid global environment mutation.
    pub fn from_values(
        socket: Option<OsString>,
        usec: Option<OsString>,
        pid: Option<OsString>,
        current_pid: u32,
    ) -> Result<Option<Self>, String> {
        let number = |v: &OsString| -> Result<u64, String> {
            let s = v
                .to_str()
                .ok_or("invalid service notification numeric value")?;
            if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
                return Err("invalid service notification numeric value".into());
            }
            s.parse()
                .map_err(|_| "invalid service notification numeric value".into())
        };
        let applies = match pid.as_ref() {
            Some(value) => {
                let value = number(value)?;
                if value == 0 || value > u32::MAX as u64 {
                    return Err("invalid WATCHDOG_PID".into());
                }
                value == current_pid as u64
            }
            None => true,
        };
        let interval = if applies {
            usec.as_ref()
                .map(|value| {
                    let value = number(value)?;
                    if value < 2 {
                        return Err("WATCHDOG_USEC must be at least two microseconds".into());
                    }
                    Ok(Duration::from_micros(value / 2))
                })
                .transpose()?
        } else {
            None
        };
        let Some(path) = socket else {
            return if interval.is_some() {
                Err("WATCHDOG_USEC requires NOTIFY_SOCKET".into())
            } else {
                Ok(None)
            };
        };
        #[cfg(target_os = "linux")]
        {
            use std::os::{
                linux::net::SocketAddrExt,
                unix::{
                    ffi::OsStrExt,
                    net::{SocketAddr, UnixDatagram},
                },
            };
            let bytes = path.as_os_str().as_bytes();
            let address = if bytes.first() == Some(&b'@') && bytes.len() > 1 {
                SocketAddr::from_abstract_name(&bytes[1..])
            } else if bytes.first() == Some(&b'/') {
                SocketAddr::from_pathname(&path)
            } else {
                return Err("NOTIFY_SOCKET must be an absolute or abstract Unix address".into());
            }
            .map_err(|_| "invalid NOTIFY_SOCKET address")?;
            let socket =
                UnixDatagram::unbound().map_err(|_| "service notification socket unavailable")?;
            socket
                .set_nonblocking(true)
                .map_err(|_| "service notification socket configuration failed")?;
            socket
                .connect_addr(&address)
                .map_err(|_| "service notification socket connection failed")?;
            Ok(Some(Self { socket, interval }))
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (path, interval);
            Err("service notification is supported only on Linux".into())
        }
    }
    pub fn interval(&self) -> Option<Duration> {
        self.interval
    }
    fn send(&self, message: &[u8]) -> Result<(), String> {
        #[cfg(target_os = "linux")]
        {
            match self.socket.send(message) {
                Ok(n) if n == message.len() => Ok(()),
                _ => Err("service notification send failed".into()),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = message;
            Err("service notification is supported only on Linux".into())
        }
    }
    /// Spawn on the same executor that serves interactive requests, never a side thread.
    pub fn start(
        self,
        runtime: &Handle,
        shutdown: Arc<Notify>,
        mut stop: watch::Receiver<bool>,
    ) -> JoinHandle<Result<(), String>> {
        runtime.spawn(async move {
            let result = async {
                self.send(b"READY=1")?;
                if let Some(period) = self.interval {
                    let mut ticks = tokio::time::interval(period);
                    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    loop {
                        if *stop.borrow() {
                            break;
                        }
                        tokio::select! {
                            biased;
                            _ = stop.changed() => break,
                            _ = ticks.tick() => self.send(b"WATCHDOG=1")?,
                        }
                    }
                } else if !*stop.borrow() {
                    let _ = stop.changed().await;
                }
                self.send(b"STOPPING=1")
            }
            .await;
            if result.is_err() {
                shutdown.notify_one();
            }
            result
        })
    }
}
