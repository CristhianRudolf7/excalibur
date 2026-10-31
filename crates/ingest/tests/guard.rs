//! Ingestion guard contract (architecture 20.7 and the 20.16 row for fake extensions,
//! compression bombs and scanner samples). All inputs are synthetic and generated here.
mod support;
use excalibur_ingest::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use support::*;

fn guard() -> ContentGuard {
    ContentGuard::default()
}
fn limits(f: impl FnOnce(&mut Limits)) -> ContentGuard {
    let mut policy = Policy::default();
    f(&mut policy.limits);
    ContentGuard::new(policy)
}
fn code(r: Result<Inspection, Rejection>) -> Code {
    match r {
        Ok(i) => panic!("expected rejection, accepted {i:?}"),
        Err(e) => e.code,
    }
}

// ---------------------------------------------------------------- type detection

#[test]
fn existing_size_text_pdf_and_png_rules_are_kept() {
    let g = limits(|l| l.max_input_bytes = 16);
    assert_eq!(code(g.examine(None, "text/plain", b"")), Code::Empty);
    assert_eq!(code(g.examine(None, "text/plain", &[b'a'; 17])), Code::Oversized);
    assert!(g.examine(None, "text/plain", &[b'a'; 16]).is_ok());
    let g = guard();
    assert_eq!(code(g.examine(None, "text/plain", b"a\0b")), Code::InvalidText);
    assert_eq!(code(g.examine(None, "text/markdown", b"\xFF\xFE")), Code::InvalidText);
    assert_eq!(code(g.examine(None, "application/pdf", b"not a pdf")), Code::TypeMismatch);
    assert_eq!(code(g.examine(None, "image/png", b"GIF89a....")), Code::TypeMismatch);
    let ok = g.examine(None, "application/pdf", b"%PDF-1.7 synthetic").unwrap();
    assert_eq!(ok.detected, Kind::Pdf);
    assert!(g.examine(None, "image/png", b"\x89PNG....").is_ok());
    // Declared types without a reliable signature are only rejected on a positive conflict.
    assert!(g.examine(None, "video/mp4", b"....").is_ok());
    assert!(g.examine(None, "audio/ogg", b"OggS").is_ok());
}

#[test]
fn content_signature_wins_over_fake_extensions_and_declared_types() {
    let g = guard();
    // A PNG renamed to .pdf, and a zip pretending to be a picture or a text file.
    let r = g.examine(Some("invoice.pdf"), "application/octet-stream", &png());
    assert_eq!(code(r), Code::TypeMismatch);
    let archive = zip(&[file("a.txt", b"hello")]);
    assert_eq!(code(g.examine(Some("photo.jpg"), "image/jpeg", &archive)), Code::TypeMismatch);
    assert_eq!(code(g.examine(Some("notes.txt"), "application/octet-stream", &archive)), Code::TypeMismatch);
    assert_eq!(code(g.examine(None, "image/jpeg", b"plain words")), Code::TypeMismatch);
    assert_eq!(code(g.examine(None, "audio/ogg", b"%PDF-1.4")), Code::TypeMismatch);
    assert_eq!(code(g.examine(None, "image/heic", &archive)), Code::TypeMismatch);
    // Matching name, declaration and content are accepted.
    let ok = g.examine(Some("Scan.PNG"), "image/png", &png()).unwrap();
    assert_eq!(ok.detected, Kind::Png);
    assert!(ok.flags.is_empty());
    // Text that merely starts with "MZ" is text, not a DOS executable.
    assert_eq!(g.examine(Some("mz.txt"), "text/plain", b"MZ notes").unwrap().detected, Kind::Text);
}

#[test]
fn executables_are_never_accepted_whatever_they_claim_to_be() {
    let g = guard();
    for (name, declared) in [
        (Some("report.pdf"), "application/pdf"),
        (Some("song.mp3"), "audio/mpeg"),
        (None, "application/octet-stream"),
        (Some("tool"), "application/x-executable"),
    ] {
        assert_eq!(code(g.examine(name, declared, &elf())), Code::Executable);
        assert_eq!(code(g.examine(name, declared, &pe())), Code::Executable);
    }
    assert_eq!(code(g.examine(None, "application/wasm", b"\0asm\x01\0\0\0")), Code::Executable);
    // ELF bytes declared as text fail the UTF-8 rule first.
    assert_eq!(code(g.examine(None, "text/plain", &elf())), Code::InvalidText);
}

#[test]
fn flag_policy_accepts_mismatches_but_records_them() {
    let mut policy = Policy::default();
    policy.mismatch = MismatchPolicy::Flag;
    let g = ContentGuard::new(policy);
    let ok = g.examine(Some("invoice.pdf"), "application/pdf", &png()).unwrap();
    assert_eq!(ok.detected, Kind::Png);
    assert_eq!(ok.flags.len(), 2, "{:?}", ok.flags);
    assert!(ok.flags.iter().all(|f| f.starts_with("type_mismatch")));
    // Executables stay rejected under the flag policy.
    assert_eq!(code(g.examine(Some("a.pdf"), "application/pdf", &elf())), Code::Executable);
}

// ---------------------------------------------------------------- archives and bombs

#[test]
fn ordinary_archives_are_expanded_and_reported() {
    let g = guard();
    let inner = tar(&[tar_file("docs/readme.md", b"# Title\n"), tar_file("img.png", &png())]);
    let archive = zip(&[
        file("a.txt", b"alpha"),
        ZipEntry {
            deflate: false,
            ..file("data.tar", &inner)
        },
        file("dir/", b""),
    ]);
    let ok = g.examine(Some("bundle.zip"), "application/zip", &archive).unwrap();
    let paths: Vec<_> = ok.entries.iter().map(|e| (e.path.as_str(), e.depth)).collect();
    assert_eq!(
        paths,
        [("a.txt", 1), ("data.tar", 1), ("docs/readme.md", 2), ("img.png", 2), ("dir", 1)]
    );
    assert!(ok.entries[4].directory);

    let gz = gzip(Some("logs.tar"), &tar(&[tar_file("x.log", b"line\n")]));
    let ok = g.examine(Some("logs.tar.gz"), "application/gzip", &gz).unwrap();
    assert_eq!(ok.entries[0].path, "logs.tar");
    assert_eq!(ok.entries[1].path, "x.log");
    assert_eq!(ok.entries[1].depth, 2);

    let zs = zstd(b"zstd payload");
    let ok = g.examine(Some("p.zst"), "application/zstd", &zs).unwrap();
    assert_eq!(ok.expanded_bytes, 12);
}

#[test]
fn high_ratio_bombs_stop_at_the_ratio_limit() {
    let g = guard();
    let zeros = vec![0u8; 8 * 1024 * 1024];
    let gz = gzip(None, &zeros);
    assert!(gz.len() < 64 * 1024, "synthetic bomb is small: {}", gz.len());
    assert_eq!(code(g.examine(None, "application/gzip", &gz)), Code::RatioLimit);
    assert_eq!(code(g.examine(None, "application/zstd", &zstd(&zeros))), Code::RatioLimit);
    assert_eq!(
        code(g.examine(None, "application/zip", &zip(&[file("zeros.bin", &zeros)]))),
        Code::RatioLimit
    );
    // The same payload is accepted when the policy allows the ratio.
    let lenient = limits(|l| l.max_ratio = 100_000);
    assert!(lenient.examine(None, "application/gzip", &gz).is_ok());
}

#[test]
fn total_expansion_limit_is_enforced_without_streaming_past_it() {
    let g = limits(|l| {
        l.max_ratio = 1_000_000;
        l.max_expanded_bytes = 1024 * 1024;
    });
    let payload = vec![b'x'; 4 * 1024 * 1024];
    for bytes in [gzip(None, &payload), zstd(&payload), zip(&[file("x", &payload)])] {
        let r = g.examine(None, "application/octet-stream", &bytes).unwrap_err();
        assert_eq!(r.code, Code::ExpansionLimit, "{r}");
    }
    assert!(g.examine(None, "application/gzip", &gzip(None, &payload[..1000 * 1024])).is_ok());
}

#[test]
fn nested_bombs_hit_the_depth_or_cumulative_limits() {
    let g = guard();
    let mut nested = zip(&[file("core.txt", b"payload")]);
    for level in 0..3 {
        nested = zip(&[file(&format!("layer{level}.zip"), &nested)]);
    }
    // Four levels of zip in the default depth-3 policy.
    assert_eq!(code(g.examine(None, "application/zip", &nested)), Code::DepthLimit);
    let deep = limits(|l| l.max_depth = 4);
    assert_eq!(deep.examine(None, "application/zip", &nested).unwrap().entries.len(), 4);

    // Each member stays under the per-stream grace, but together they explode.
    let chunk = vec![0u8; 900 * 1024];
    let members: Vec<_> = (0..12)
        .map(|i| file(&format!("part{i}.gz"), &gzip(None, &chunk)))
        .collect();
    let r = g.examine(None, "application/zip", &zip(&members)).unwrap_err();
    assert_eq!(r.code, Code::RatioLimit, "{r}");
    assert!(r.message.contains("total expansion"), "{r}");
}

#[test]
fn entry_count_limit_applies_across_levels() {
    let g = limits(|l| l.max_entries = 10);
    let many: Vec<_> = (0..11).map(|i| file(&format!("f{i}.txt"), b"x")).collect();
    assert_eq!(code(g.examine(None, "application/zip", &zip(&many))), Code::EntryLimit);
    let tarred: Vec<_> = (0..11).map(|i| tar_file(&format!("f{i}"), b"x")).collect();
    assert_eq!(code(g.examine(None, "application/x-tar", &tar(&tarred))), Code::EntryLimit);
    // Six outer entries plus five inner entries.
    let inner: Vec<_> = (0..5).map(|i| tar_file(&format!("i{i}"), b"x")).collect();
    let mut outer: Vec<_> = (0..5).map(|i| file(&format!("o{i}"), b"x")).collect();
    outer.push(file("inner.tar", &tar(&inner)));
    assert_eq!(code(g.examine(None, "application/zip", &zip(&outer))), Code::EntryLimit);
}

#[test]
fn lying_or_damaged_archives_are_malformed() {
    let g = guard();
    let mut archive = zip(&[file("a.txt", b"hello world")]);
    // Corrupt one byte of the stored CRC in the central directory.
    let cd = archive.windows(4).rposition(|w| w == b"PK\x01\x02").unwrap();
    archive[cd + 16] ^= 0xFF;
    assert_eq!(code(g.examine(None, "application/zip", &archive)), Code::Malformed);
    let mut gz = gzip(None, b"hello");
    let n = gz.len();
    gz[n - 1] ^= 1;
    assert_eq!(code(g.examine(None, "application/gzip", &gz)), Code::Malformed);
    let mut seed = 0x2545_F491u32;
    let noise: Vec<u8> = (0..4096)
        .map(|_| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 24) as u8
        })
        .collect();
    let full = gzip(None, &noise);
    assert!(full.len() > 1000);
    let truncated = &full[..full.len() / 2];
    assert_eq!(code(g.examine(None, "application/gzip", truncated)), Code::Malformed);
    let dup = zip(&[file("a.txt", b"1"), file("a.txt", b"2")]);
    assert_eq!(code(g.examine(None, "application/zip", &dup)), Code::Malformed);
}

// ---------------------------------------------------------------- paths and links

#[test]
fn traversal_absolute_and_drive_paths_are_rejected() {
    let g = guard();
    for name in ["../evil.txt", "a/../../evil", "/etc/passwd", "C:\\Windows\\x", "a\\..\\..\\b", "./"] {
        let r = g.examine(None, "application/zip", &zip(&[file(name, b"x")])).unwrap_err();
        assert_eq!(r.code, Code::UnsafePath, "{name}: {r}");
        let r = g.examine(None, "application/x-tar", &tar(&[tar_file(name, b"x")])).unwrap_err();
        assert_eq!(r.code, Code::UnsafePath, "{name}: {r}");
    }
    let r = g.examine(None, "application/gzip", &gzip(Some("../../.bashrc"), b"x"));
    assert_eq!(code(r), Code::UnsafePath);
    assert_eq!(safe_entry_path("a/./b//c.txt").unwrap(), "a/b/c.txt");
}

#[test]
fn symlinks_hard_links_and_devices_are_rejected() {
    let g = guard();
    let link = ZipEntry {
        unix_mode: Some(0o120777),
        deflate: false,
        ..file("link", b"/etc/passwd")
    };
    assert_eq!(code(g.examine(None, "application/zip", &zip(&[link]))), Code::Link);
    for kind in [b'1', b'2', b'3', b'4', b'6'] {
        let entry = TarEntry {
            kind,
            link: "/etc/shadow".into(),
            ..tar_file("x", b"")
        };
        assert_eq!(code(g.examine(None, "application/x-tar", &tar(&[entry]))), Code::Link);
    }
    let pax = TarEntry {
        kind: b'x',
        ..tar_file("pax", b"20 path=../../evil\n")
    };
    assert_eq!(code(g.examine(None, "application/x-tar", &tar(&[pax]))), Code::Unsupported);
}

#[test]
fn executable_modes_and_executable_entries_are_never_honoured() {
    let g = guard();
    let script = TarEntry {
        mode: 0o755,
        ..tar_file("run.sh", b"#!/bin/sh\necho hi\n")
    };
    let ok = g.examine(None, "application/x-tar", &tar(&[script])).unwrap();
    assert!(ok.entries[0].executable_mode_ignored);
    assert!(ok.flags.iter().any(|f| f.starts_with("executable_mode_ignored")));
    let exec_zip = ZipEntry {
        unix_mode: Some(0o100755),
        ..file("tool.txt", b"text")
    };
    assert!(g.examine(None, "application/zip", &zip(&[exec_zip])).unwrap().entries[0].executable_mode_ignored);
    // Native code inside an archive, even renamed, is refused.
    let hidden = zip(&[file("readme.txt", b"hi"), file("image.png", &elf())]);
    assert_eq!(code(g.examine(None, "application/zip", &hidden)), Code::Executable);
    // An entry whose name disagrees with its content is a mismatch.
    let renamed = zip(&[file("photo.png", b"%PDF-1.4")]);
    assert_eq!(code(g.examine(None, "application/zip", &renamed)), Code::TypeMismatch);
}

// ---------------------------------------------------------------- scanner hook

struct Failing;
impl Scanner for Failing {
    fn name(&self) -> &str {
        "failing-mock"
    }
    fn scan(&self, _: Option<&str>, _: &[u8]) -> ScanVerdict {
        ScanVerdict::Failed {
            reason: "engine offline".into(),
        }
    }
}

struct Counting(AtomicUsize, SignatureScanner);
impl Scanner for Counting {
    fn name(&self) -> &str {
        "counting-mock"
    }
    fn scan(&self, path: Option<&str>, bytes: &[u8]) -> ScanVerdict {
        self.0.fetch_add(1, Ordering::SeqCst);
        self.1.scan(path, bytes)
    }
}

#[test]
fn scanner_hits_and_misses_are_deterministic() {
    let eicar = eicar_test_string();
    assert_eq!(eicar.len(), 68);
    let g = ContentGuard::default().with_scanner(Arc::new(SignatureScanner::eicar()));
    let clean = g.examine(Some("notes.txt"), "text/plain", b"nothing to see").unwrap();
    assert_eq!(clean.scanner.as_deref(), Some("signature-mock"));

    let r = g.examine(Some("eicar.txt"), "text/plain", &eicar).unwrap_err();
    assert_eq!(r.code, Code::ScannerHit);
    assert!(r.message.contains("EICAR-Test-File"), "{r}");

    // Hidden inside a tar inside a gzip inside a zip: the decompressed tar already matches.
    let tgz = gzip(Some("x.tar"), &tar(&[tar_file("deep/eicar.com.txt", &eicar)]));
    let archive = zip(&[file("ok.txt", b"fine"), file("x.tar.gz", &tgz)]);
    let r = g.examine(Some("bundle.zip"), "application/zip", &archive).unwrap_err();
    assert_eq!(r.code, Code::ScannerHit);
    assert!(r.message.contains("\"x.tar\""), "{r}");
    // Deflated inside a zip inside a zip: only the innermost entry carries the plain bytes.
    let inner = zip(&[file("deep/eicar.txt", &eicar)]);
    let archive = zip(&[file("ok.txt", b"fine"), file("inner.zip", &inner)]);
    let r = g.examine(None, "application/zip", &archive).unwrap_err();
    assert_eq!(r.code, Code::ScannerHit);
    assert!(r.message.contains("deep/eicar.txt"), "{r}");

    // Every blob is offered to the scanner: file, zip entries, gzip member, tar entry.
    let counting = Arc::new(Counting(AtomicUsize::new(0), SignatureScanner::eicar()));
    let g2 = ContentGuard::default().with_scanner(counting.clone());
    let tgz_clean = gzip(Some("x.tar"), &tar(&[tar_file("a.txt", b"a")]));
    g2.examine(None, "application/zip", &zip(&[file("ok.txt", b"fine"), file("x.tar.gz", &tgz_clean)]))
        .unwrap();
    assert_eq!(counting.0.load(Ordering::SeqCst), 5);
}

#[test]
fn scanner_failure_and_missing_required_scanner_fail_closed() {
    let g = ContentGuard::default().with_scanner(Arc::new(Failing));
    assert_eq!(code(g.examine(None, "text/plain", b"hello")), Code::ScannerFailed);
    let mut policy = Policy::default();
    policy.require_scanner = true;
    let strict = ContentGuard::new(policy.clone());
    assert_eq!(code(strict.examine(None, "text/plain", b"hello")), Code::ScannerRequired);
    let strict = ContentGuard::new(policy).with_scanner(Arc::new(SignatureScanner::eicar()));
    assert!(strict.examine(None, "text/plain", b"hello").is_ok());
}

// ---------------------------------------------------------------- quarantine

#[cfg(unix)]
fn mode(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn rejected_files_enter_a_durable_non_executable_quarantine() {
    let dir = tempfile::tempdir().unwrap();
    let qdir = dir.path().join("quarantine");
    let q = Arc::new(Quarantine::open(&qdir).unwrap());
    let g = ContentGuard::default()
        .with_scanner(Arc::new(SignatureScanner::eicar()))
        .with_quarantine(q.clone());

    let bytes = elf();
    let r = g.examine(Some("invoice.pdf"), "application/pdf", &bytes).unwrap_err();
    let record = r.quarantine.clone().expect("quarantined");
    assert_eq!(record.sha256, sha256_hex(&bytes));
    assert_eq!(record.reason_code, "executable");
    assert_eq!(record.original_name.as_deref(), Some("invoice.pdf"));
    assert_eq!(record.declared_type, "application/pdf");
    assert_eq!(record.detected_type, "elf");
    assert_eq!(record.size, bytes.len() as u64);
    let object = q.object_path(&record.sha256);
    assert_eq!(std::fs::read(&object).unwrap(), bytes);
    assert_eq!(qdir.join(&record.object), object);
    #[cfg(unix)]
    {
        assert_eq!(mode(&object), 0o600);
        assert_eq!(mode(&q.manifest_path()), 0o600);
        assert_eq!(mode(&qdir), 0o700);
        assert_eq!(mode(&qdir.join("objects")), 0o700);
    }

    // Accepted files are not quarantined; a scanner hit is.
    g.examine(None, "text/plain", b"clean").unwrap();
    let hit = g.examine(Some("eicar.txt"), "text/plain", &eicar_test_string()).unwrap_err();
    assert_eq!(hit.quarantine.unwrap().reason_code, "scanner_hit");
    // The same bytes rejected again add a record but reuse the object.
    g.examine(Some("again.pdf"), "application/pdf", &bytes).unwrap_err();
    let objects = std::fs::read_dir(qdir.join("objects")).unwrap().count();
    assert_eq!(objects, 2);

    // Reopen from disk, as after a restart.
    drop(g);
    drop(q);
    let reopened = Quarantine::open(&qdir).unwrap();
    let records = reopened.records().unwrap();
    let names: Vec<_> = records.iter().map(|r| r.original_name.as_deref().unwrap()).collect();
    assert_eq!(names, ["invoice.pdf", "eicar.txt", "again.pdf"]);

    // A torn final line from an interrupted append is ignored, then repaired by the next append.
    use std::io::Write;
    let mut m = std::fs::OpenOptions::new().append(true).open(reopened.manifest_path()).unwrap();
    m.write_all(b"{\"sha256\":\"trunc").unwrap();
    drop(m);
    assert_eq!(reopened.records().unwrap().len(), 3);
    let g = ContentGuard::default().with_quarantine(Arc::new(reopened));
    g.examine(Some("x.png"), "image/png", b"not png").unwrap_err();
    let q = g.quarantine.as_ref().unwrap();
    let records = q.records().unwrap();
    assert_eq!(records.len(), 4);
    assert_eq!(records[3].reason_code, "type_mismatch");
}

#[test]
fn quarantined_archives_with_executable_modes_stay_non_executable() {
    let dir = tempfile::tempdir().unwrap();
    let q = Arc::new(Quarantine::open(dir.path().join("q")).unwrap());
    let g = ContentGuard::default().with_quarantine(q.clone());
    let entry = TarEntry {
        mode: 0o4755,
        ..tar_file("../escape.sh", b"#!/bin/sh\n")
    };
    let r = g.examine(Some("a.tar"), "application/x-tar", &tar(&[entry])).unwrap_err();
    assert_eq!(r.code, Code::UnsafePath);
    let record = r.quarantine.unwrap();
    #[cfg(unix)]
    assert_eq!(mode(&q.object_path(&record.sha256)), 0o600);
    // Nothing from the archive was written anywhere except the quarantine object.
    let mut found = Vec::new();
    for e in walk(dir.path()) {
        found.push(e.strip_prefix(dir.path()).unwrap().to_string_lossy().into_owned());
    }
    found.sort();
    assert_eq!(found, ["q/manifest.jsonl".to_string(), format!("q/objects/{}.bin", record.sha256)]);
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}

#[cfg(unix)]
#[test]
fn quarantine_refuses_a_symlinked_directory() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert!(Quarantine::open(&link).is_err());
}

#[test]
fn concurrent_rejections_keep_every_manifest_record() {
    let dir = tempfile::tempdir().unwrap();
    let q = Arc::new(Quarantine::open(dir.path().join("q")).unwrap());
    let g = Arc::new(ContentGuard::default().with_quarantine(q.clone()));
    let threads: Vec<_> = (0..8)
        .map(|t| {
            let g = g.clone();
            std::thread::spawn(move || {
                for i in 0..10 {
                    let mut bytes = elf();
                    bytes.extend_from_slice(format!("{t}-{i}").as_bytes());
                    g.examine(None, "application/octet-stream", &bytes).unwrap_err();
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    // A second handle on the same directory appends through the file lock as well.
    let other = Quarantine::open(dir.path().join("q")).unwrap();
    let g2 = ContentGuard::default().with_quarantine(Arc::new(other));
    g2.examine(None, "application/octet-stream", &pe()).unwrap_err();
    let records = q.records().unwrap();
    assert_eq!(records.len(), 81);
    let unique: std::collections::BTreeSet<_> = records.iter().map(|r| &r.sha256).collect();
    assert_eq!(unique.len(), 81);
}
