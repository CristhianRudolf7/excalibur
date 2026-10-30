//! Loopback proxy listener with UID ownership checks (12.3).
//!
//! Chrome only speaks TCP to its proxy while the egress broker listens on a
//! Unix socket. webd binds `127.0.0.1:<random>` and forwards to the broker
//! socket. Every accepted connection is mapped through `/proc/net/tcp` and
//! `/proc/net/tcp6` (inode to UID); a peer owned by another UID is closed
//! without forwarding so the listener can never be used as an open proxy.
use std::net::SocketAddr;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

/// Read both proc tables; tests inject fixture strings instead.
pub fn check_peer(peer: SocketAddr, listener_port: u16, expected_uid: u32) -> Result<(), String> {
    let tcp = std::fs::read_to_string("/proc/net/tcp").map_err(|_| "proc unavailable")?;
    let tcp6 = std::fs::read_to_string("/proc/net/tcp6").unwrap_or_default();
    excalibur_browser::net::check_loopback_uid(peer, listener_port, expected_uid, &tcp, &tcp6)
}

/// Accept one connection, closing foreign-UID peers. Returns `Some(stream)`
/// for same-UID peers and `None` for rejected peers.
pub async fn accept_owned(
    listener: &TcpListener,
    expected_uid: u32,
) -> std::io::Result<Option<TcpStream>> {
    let (stream, peer) = listener.accept().await?;
    let port = listener.local_addr()?.port();
    match check_peer(peer, port, expected_uid) {
        Ok(()) => Ok(Some(stream)),
        Err(_) => Ok(None),
    }
}

/// Forward a loopback stream to the Unix broker socket in both directions.
pub async fn forward_to_unix(tcp: TcpStream, broker: &std::path::Path) -> Result<(), String> {
    let unix = tokio::net::UnixStream::connect(broker)
        .await
        .map_err(|_| "proxy socket is unavailable")?;
    let (mut tcp_read, mut tcp_write) = tcp.into_split();
    let (mut unix_read, mut unix_write) = unix.into_split();
    let upstream = tokio::spawn(async move {
        let _ = tokio::io::copy(&mut tcp_read, &mut unix_write).await;
        let _ = unix_write.shutdown().await;
    });
    let _ = tokio::io::copy(&mut unix_read, &mut tcp_write).await;
    let _ = upstream.await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_tables_parse_for_uid_checks() {
        let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 0100007F:1F90 0100007F:0035 01 00000000:00000000 00:00000000 00000000  1000        0 12345 1 0000000000000000 100 0 0 10 0\n";
        let peer: SocketAddr = "127.0.0.1:53".parse().unwrap();
        assert!(excalibur_browser::net::check_loopback_uid(peer, 8080, 1000, table, "").is_ok());
        assert!(excalibur_browser::net::check_loopback_uid(peer, 8080, 999, table, "").is_err());
    }
}
