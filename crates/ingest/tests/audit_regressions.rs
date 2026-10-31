//! Regressions for the independent audit of the first ingestion guard (findings IG-1 to IG-11).
//! Each test fails against that version. All inputs are synthetic and generated here.
mod support;
use excalibur_ingest::*;
use std::sync::Arc;
use support::*;

fn code(r: Result<Inspection, Rejection>) -> Code {
    match r {
        Ok(i) => panic!("expected rejection, accepted {i:?}"),
        Err(e) => e.code,
    }
}
fn guard() -> ContentGuard {
    ContentGuard::default()
}
fn find(z: &[u8], sig: &[u8; 4]) -> usize {
    z.windows(4).position(|w| w == sig).unwrap()
}
fn put16(z: &mut [u8], at: usize, v: u16) {
    z[at..at + 2].copy_from_slice(&v.to_le_bytes());
}
fn put32(z: &mut [u8], at: usize, v: u32) {
    z[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

// IG-1: stored entries count against the expansion budget.
#[test]
fn stored_entries_count_against_the_expansion_budget() {
    let g = ContentGuard::new(Policy {
        limits: Limits {
            max_expanded_bytes: 1024 * 1024,
            ..Default::default()
        },
        ..Default::default()
    });
    let stored = |name: &str, data: &[u8]| ZipEntry {
        deflate: false,
        ..file(name, data)
    };
    let big = vec![b'x'; 2 * 1024 * 1024];
    let r = g.examine(None, "application/zip", &zip(&[stored("big.txt", &big)]));
    assert_eq!(code(r), Code::ExpansionLimit);
    let half = vec![b'y'; 600 * 1024];
    let two = zip(&[stored("a.txt", &half), stored("b.txt", &half)]);
    assert_eq!(
        code(g.examine(None, "application/zip", &two)),
        Code::ExpansionLimit
    );
    let ok = g
        .examine(None, "application/zip", &zip(&[stored("a.txt", &half)]))
        .unwrap();
    assert_eq!(ok.expanded_bytes, half.len() as u64);
}

// IG-2: every central-directory byte must belong to a declared entry.
#[test]
fn hidden_central_directory_entries_are_rejected() {
    let mut z = zip(&[file("a.txt", b"visible"), file("payload", &elf())]);
    let e = find(&z, b"PK\x05\x06");
    put16(&mut z, e + 8, 1);
    put16(&mut z, e + 10, 1);
    let r = guard().examine(None, "application/zip", &z).unwrap_err();
    assert_eq!(r.code, Code::Malformed, "{r}");
}

// IG-2/IG-6: data between local records that no central entry accounts for is rejected.
#[test]
fn unaccounted_local_records_are_rejected() {
    let hidden = zip(&[file("payload", &elf())]);
    let cd = find(&hidden, b"PK\x01\x02");
    let mut z = hidden[..cd].to_vec();
    let visible = zip(&[file("a.txt", b"visible")]);
    let vcd = find(&visible, b"PK\x01\x02");
    let shift = z.len() as u32;
    z.extend_from_slice(&visible[..vcd]);
    let mut central = visible[vcd..].to_vec();
    put32(&mut central, 42, shift);
    let e = central.len() - 22;
    put32(&mut central, e + 16, z.len() as u32);
    z.extend_from_slice(&central);
    let r = guard().examine(None, "application/zip", &z).unwrap_err();
    assert_eq!(r.code, Code::Malformed, "{r}");
    assert!(r.message.contains("unaccounted"), "{r}");
}

// IG-3: the local header must agree with the central directory.
#[test]
fn local_headers_disagreeing_with_the_central_directory_are_rejected() {
    let base = zip(&[ZipEntry {
        deflate: false,
        ..file("doc.bin", b"plain stored bytes")
    }]);
    let cases: [(&str, usize, u32, bool); 5] = [
        ("method", 8, 8, true),
        ("flags", 6, 0x0800, true),
        ("crc", 14, 0xDEAD_BEEF, false),
        ("compressed size", 18, 3, false),
        ("size", 22, 3, false),
    ];
    for (what, at, value, short) in cases {
        let mut z = base.clone();
        if short {
            put16(&mut z, at, value as u16);
        } else {
            put32(&mut z, at, value);
        }
        let r = guard().examine(None, "application/zip", &z).unwrap_err();
        assert_eq!(r.code, Code::Malformed, "{what}: {r}");
    }
    assert!(guard().examine(None, "application/zip", &base).is_ok());
}

// IG-4: Unix modes are honoured whatever the host byte (VMS, BeOS, AtheOS, FAT, ...).
#[test]
fn symlink_and_executable_modes_are_detected_for_every_host_system() {
    for host in [0u8, 2, 3, 10, 16, 19, 30] {
        let mut z = zip(&[ZipEntry {
            unix_mode: Some(0o120777),
            deflate: false,
            ..file("evil", b"/etc/hostname")
        }]);
        let cd = find(&z, b"PK\x01\x02");
        z[cd + 5] = host;
        assert_eq!(
            code(guard().examine(None, "application/zip", &z)),
            Code::Link,
            "host {host}"
        );

        let mut z = zip(&[ZipEntry {
            unix_mode: Some(0o100755),
            ..file("tool.txt", b"text")
        }]);
        let cd = find(&z, b"PK\x01\x02");
        z[cd + 5] = host;
        let ok = guard().examine(None, "application/zip", &z).unwrap();
        assert!(ok.entries[0].executable_mode_ignored, "host {host}");
    }
}

// IG-5: valid text promised as text stays text despite a leading magic prefix.
#[test]
fn text_that_starts_with_a_magic_prefix_is_still_text() {
    let mut ustar = vec![b' '; 257];
    ustar.extend_from_slice(b"ustar and more words");
    let samples: Vec<Vec<u8>> = vec![
        b"ID3 tags explained".to_vec(),
        b"GIF89a is a 1989 format".to_vec(),
        b"%PDF-1.7 notes".to_vec(),
        b"OggS container notes".to_vec(),
        b"fLaC is lossless".to_vec(),
        b"abcdftyp box notes".to_vec(),
        b"RIFF1234WAVE notes".to_vec(),
        ustar,
    ];
    for s in &samples {
        for declared in ["text/markdown", "text/plain"] {
            let ok = guard().examine(None, declared, s).unwrap();
            assert_eq!(
                ok.detected,
                Kind::Text,
                "{declared}: {:?}",
                String::from_utf8_lossy(s)
            );
        }
        assert_eq!(
            guard()
                .examine(Some("notes.md"), "application/octet-stream", s)
                .unwrap()
                .detected,
            Kind::Text
        );
    }
    // Inside an archive the entry name promises text as well.
    let z = zip(&[file("notes.md", b"%PDF-1.7 notes")]);
    assert!(guard().examine(None, "application/zip", &z).is_ok());
    // Executables and real binaries are unaffected.
    assert_eq!(
        code(guard().examine(None, "text/plain", &elf())),
        Code::InvalidText
    );
    assert_eq!(
        code(guard().examine(Some("a.md"), "application/octet-stream", &elf())),
        Code::Executable
    );
    assert_eq!(
        code(guard().examine(None, "image/png", b"%PDF-1.7")),
        Code::TypeMismatch
    );
}

// IG-6: trailing data inside containers is rejected.
#[test]
fn trailing_data_in_tar_and_deflate_entries_is_rejected() {
    let mut t = tar(&[tar_file("a.txt", b"visible")]);
    t.extend_from_slice(&tar(&[tar_file("payload", &elf())]));
    let r = guard().examine(None, "application/x-tar", &t).unwrap_err();
    assert_eq!(r.code, Code::Malformed, "{r}");
    let mut padded = tar(&[tar_file("a.txt", b"visible")]);
    padded.extend_from_slice(&[0u8; 8192]);
    assert!(guard().examine(None, "application/x-tar", &padded).is_ok());

    let z = zip(&[ZipEntry {
        trailing: elf(),
        ..file("a.txt", b"visible text")
    }]);
    let r = guard().examine(None, "application/zip", &z).unwrap_err();
    assert_eq!(r.code, Code::Malformed, "{r}");
    assert!(r.message.contains("trailing data"), "{r}");
}

// IG-7: member and frame floods are bounded.
#[test]
fn gzip_member_and_zstd_frame_floods_are_bounded() {
    let member = gzip(None, b"");
    let flood = member.repeat(4097);
    let r = guard()
        .examine(None, "application/gzip", &flood)
        .unwrap_err();
    assert_eq!(r.code, Code::EntryLimit, "{r}");
    assert!(
        guard()
            .examine(None, "application/gzip", &member.repeat(4096))
            .is_ok()
    );
    let frame = zstd(b"");
    let r = guard()
        .examine(None, "application/zstd", &frame.repeat(4097))
        .unwrap_err();
    assert_eq!(r.code, Code::EntryLimit, "{r}");
    assert!(
        guard()
            .examine(None, "application/zstd", &frame.repeat(64))
            .is_ok()
    );
}

// IG-8: attacker-controlled text in messages and manifests is bounded.
#[test]
fn echoed_names_and_types_are_truncated() {
    let long = "a".repeat(60_000);
    let r = guard()
        .examine(None, "application/zip", &zip(&[file(&long, b"x")]))
        .unwrap_err();
    assert_eq!(r.code, Code::UnsafePath);
    assert!(r.message.len() < 400, "{} bytes", r.message.len());
    let declared = format!("image/png{}", ";x".repeat(20_000));
    let dir = tempfile::tempdir().unwrap();
    let q = Arc::new(Quarantine::open(dir.path().join("q")).unwrap());
    let r = guard()
        .with_quarantine(q)
        .examine(Some(&long), &declared, b"plain text")
        .unwrap_err();
    assert_eq!(r.code, Code::TypeMismatch);
    assert!(r.message.len() < 400, "{} bytes", r.message.len());
    let record = r.quarantine.unwrap();
    assert!(record.original_name.unwrap().len() <= 1024 + 3);
    assert!(record.declared_type.len() <= 256 + 3);
}

// IG-9: the quarantine never follows planted symlinks and re-verifies existing objects.
#[cfg(unix)]
#[test]
fn quarantine_replaces_planted_or_corrupted_objects() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let qdir = dir.path().join("q");
    let q = Arc::new(Quarantine::open(&qdir).unwrap());
    let g = guard().with_quarantine(q.clone());
    let bytes = elf();
    let sha = sha256_hex(&bytes);

    let victim = dir.path().join("victim.txt");
    std::fs::write(&victim, b"keep").unwrap();
    std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::os::unix::fs::symlink(&victim, q.object_path(&sha)).unwrap();
    g.examine(None, "application/octet-stream", &bytes)
        .unwrap_err();
    assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
    assert_eq!(
        std::fs::metadata(&victim).unwrap().permissions().mode() & 0o777,
        0o644
    );
    let meta = std::fs::symlink_metadata(q.object_path(&sha)).unwrap();
    assert!(meta.file_type().is_file());
    assert_eq!(std::fs::read(q.object_path(&sha)).unwrap(), bytes);

    std::fs::write(q.object_path(&sha), b"tampered").unwrap();
    g.examine(None, "application/octet-stream", &bytes)
        .unwrap_err();
    assert_eq!(std::fs::read(q.object_path(&sha)).unwrap(), bytes);
    let mode = std::fs::metadata(q.object_path(&sha))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);

    // A symlinked manifest is refused; the rejection still stands.
    let other = tempfile::tempdir().unwrap();
    let q2 = Quarantine::open(other.path().join("q")).unwrap();
    let target = other.path().join("elsewhere.jsonl");
    std::fs::write(&target, b"").unwrap();
    std::os::unix::fs::symlink(&target, q2.manifest_path()).unwrap();
    let r = guard()
        .with_quarantine(Arc::new(q2))
        .examine(None, "application/octet-stream", &bytes)
        .unwrap_err();
    assert_eq!(r.code, Code::Executable);
    assert!(r.quarantine.is_none() && r.quarantine_error.is_some());
    assert!(std::fs::read(&target).unwrap().is_empty());
}

#[test]
fn reopening_removes_stale_temporary_objects_only() {
    let dir = tempfile::tempdir().unwrap();
    let qdir = dir.path().join("q");
    drop(Quarantine::open(&qdir).unwrap());
    let objects = qdir.join("objects");
    let stale = objects.join(".tmp-stale");
    let fresh = objects.join(".tmp-fresh");
    std::fs::write(&stale, b"partial").unwrap();
    std::fs::write(&fresh, b"in progress").unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    std::fs::File::options()
        .write(true)
        .open(&stale)
        .unwrap()
        .set_modified(old)
        .unwrap();
    drop(Quarantine::open(&qdir).unwrap());
    assert!(!stale.exists());
    assert!(fresh.exists());
}

// IG-11: legacy zstd frames are refused before decoding, wherever they appear.
#[test]
fn legacy_zstd_frames_are_refused() {
    let mut legacy = 0xFD2F_B527u32.to_le_bytes().to_vec();
    legacy.extend_from_slice(&[0u8; 32]);
    assert_eq!(
        code(guard().examine(None, "application/zstd", &legacy)),
        Code::Unsupported
    );
    // Not passed off as unknown binary under a neutral declared type either.
    assert_eq!(
        code(guard().examine(None, "application/octet-stream", &legacy)),
        Code::Unsupported
    );
    let mut mixed = zstd(b"first frame");
    let mut tail = 0xFD2F_B525u32.to_le_bytes().to_vec();
    tail.extend_from_slice(&[0u8; 32]);
    mixed.extend_from_slice(&tail);
    assert_eq!(
        code(guard().examine(None, "application/zstd", &mixed)),
        Code::Unsupported
    );
    let mut junk = zstd(b"first frame");
    junk.extend_from_slice(b"junk");
    assert_eq!(
        code(guard().examine(None, "application/zstd", &junk)),
        Code::Malformed
    );
    // Skippable frames are still accepted.
    let mut skippable = 0x184D_2A50u32.to_le_bytes().to_vec();
    skippable.extend_from_slice(&4u32.to_le_bytes());
    skippable.extend_from_slice(b"meta");
    let mut ok = zstd(b"payload");
    ok.extend_from_slice(&skippable);
    assert!(guard().examine(None, "application/zstd", &ok).is_ok());
}
