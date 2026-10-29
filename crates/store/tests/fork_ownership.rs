#![cfg(unix)]
//! A child waiting between fork and exec is not a database owner. Closing the
//! final Rust owner must release its advisory lock even during that interval.
use excalibur_store::{Database, DatabaseKind, StorageConfig};
use std::{
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::{net::UnixStream, process::CommandExt},
    },
    process::Command,
};
#[tokio::test]
async fn final_owner_drop_releases_database_and_blob_locks_before_inherited_fd_is_closed() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("main.db");
    let db = Database::open(&path, DatabaseKind::Main, StorageConfig::default()).unwrap();
    db.shutdown().await.unwrap();
    let (mut ready_parent, ready_child) = UnixStream::pair().unwrap();
    let (mut release_parent, release_child) = UnixStream::pair().unwrap();
    let (ready_fd, release_fd) = (ready_child.as_raw_fd(), release_child.as_raw_fd());
    let spawner = std::thread::spawn(move || {
        let mut command = Command::new("/bin/true");
        // Only async-signal-safe libc syscalls run in the child before exec.
        unsafe {
            command.pre_exec(move || {
                let byte = 1u8;
                if libc::write(ready_fd, (&byte as *const u8).cast(), 1) != 1 {
                    return Err(std::io::Error::last_os_error());
                }
                let mut release = 0u8;
                if libc::read(release_fd, (&mut release as *mut u8).cast(), 1) != 1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command.spawn().unwrap().wait().unwrap()
    });
    let mut ready = [0];
    ready_parent.read_exact(&mut ready).unwrap();
    drop(db);
    // This must succeed while the child still holds inherited, CLOEXEC FDs.
    let reopened = Database::open(&path, DatabaseKind::Main, StorageConfig::default());
    release_parent.write_all(&[1]).unwrap();
    assert!(spawner.join().unwrap().success());
    drop((ready_child, release_child));
    let reopened = reopened.expect("a forked non-owner must not retain database or blob ownership");
    reopened.shutdown().await.unwrap();
}
