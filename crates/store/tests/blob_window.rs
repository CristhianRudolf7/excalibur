use excalibur_store::{
    BlobState, Database, DatabaseKind, Error, Priority, ReadLane, StorageConfig, StoreEvent,
};
use std::{
    fs,
    io::{self, Read},
};
const SIZE: u64 = 20 * 1024 * 1024;
struct Pattern {
    position: u64,
    size: u64,
    flip_last: bool,
}
impl Pattern {
    fn new(size: u64) -> Self {
        Self {
            position: 0,
            size,
            flip_last: false,
        }
    }
}
fn byte_at(position: u64) -> u8 {
    (position % 251) as u8
}
impl Read for Pattern {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let n = (self.size - self.position).min(buffer.len() as u64) as usize;
        for (byte, position) in buffer[..n].iter_mut().zip(self.position..) {
            *byte = byte_at(position);
            if self.flip_last && position + 1 == self.size {
                *byte ^= 1;
            }
        }
        self.position += n as u64;
        Ok(n)
    }
}
async fn fixture() -> (tempfile::TempDir, Database, excalibur_store::BlobHash) {
    let temp = tempfile::tempdir().unwrap();
    let db = Database::open(
        temp.path().join("main.db"),
        DatabaseKind::Main,
        StorageConfig::default(),
    )
    .unwrap();
    let store = db.blobs();
    let prepared =
        tokio::task::spawn_blocking(move || store.prepare_reader(Pattern::new(SIZE)).unwrap())
            .await
            .unwrap();
    let hash = db
        .attach_blob(
            prepared,
            "test".into(),
            "twenty-megabyte-synthetic-output".into(),
            "full_output".into(),
        )
        .await
        .unwrap();
    (temp, db, hash)
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn twenty_megabyte_output_has_verified_bounded_start_middle_end_and_past_eof_windows() {
    let (_temp, db, hash) = fixture().await;
    let before = db.blobs().io_stats();
    for (offset, limit) in [
        (0, 65536usize),
        (9 * 65536 + 13, 12288),
        (SIZE - 11, 1024 * 1024),
        (SIZE, 128),
        (u64::MAX, 128),
    ] {
        let window = db
            .read_blob_window(hash.clone(), offset, limit)
            .await
            .unwrap();
        let expected_offset = offset.min(SIZE);
        let expected_len = (SIZE - expected_offset).min(limit as u64) as usize;
        assert_eq!(window.offset, expected_offset);
        assert_eq!(window.total_bytes, SIZE);
        assert_eq!(window.bytes.len(), expected_len);
        assert!(window.bytes.capacity() <= 1024 * 1024);
        assert!(
            window
                .bytes
                .iter()
                .enumerate()
                .all(|(i, b)| *b == byte_at(expected_offset + i as u64))
        );
    }
    let after = db.blobs().io_stats();
    assert_eq!(after.verified_reads - before.verified_reads, 5);
    assert_eq!(
        after.decoded_bytes - before.decoded_bytes,
        5 * SIZE,
        "even EOF windows verify complete content instead of trusting a prefix"
    );
    assert!(matches!(
        db.read_blob_window(hash.clone(), 0, 0).await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        db.read_blob_window(hash.clone(), 0, 1024 * 1024 + 1).await,
        Err(Error::Invalid(_))
    ));
    assert_eq!(
        db.blobs().io_stats().verified_reads,
        after.verified_reads,
        "caller bounds are checked before I/O"
    );
    assert!(
        matches!(
            db.read_blob(hash, 16 * 1024 * 1024).await,
            Err(Error::BlobTooLarge(_))
        ),
        "existing full-read bound retains its original semantics"
    );
    db.shutdown().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn corrupt_tail_is_detected_even_for_a_correct_prefix_window_and_incidence_is_recorded() {
    let (_temp, db, hash) = fixture().await;
    let path = db.blobs().path(&hash);
    let mut events = db.subscribe_events();
    // Keep the requested prefix correct and the compression frame valid, changing only
    // the last uncompressed byte. A prefix-only implementation would wrongly succeed.
    tokio::task::spawn_blocking(move || {
        let file = fs::File::create(path).unwrap();
        let mut encoder = zstd::stream::Encoder::new(file, 3).unwrap();
        let mut source = Pattern {
            position: 0,
            size: SIZE,
            flip_last: true,
        };
        io::copy(&mut source, &mut encoder).unwrap();
        encoder.finish().unwrap().sync_all().unwrap();
    })
    .await
    .unwrap();
    assert!(matches!(
        db.read_blob_window(hash.clone(), 0, 64).await,
        Err(Error::BlobCorrupt(_))
    ));
    let lookup = hash.as_str().to_string();
    let state: String = db
        .read(ReadLane::Interactive, move |c| {
            Ok(c.query_row(
                "SELECT state FROM blob_integrity WHERE hash=?1",
                [lookup],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(state, "corrupt");
    assert!(std::iter::from_fn(|| events.try_recv().ok()).any(
        |event| matches!(event,StoreEvent::BlobIntegrity(issue) if issue.state==BlobState::Corrupt)
    ));
    db.shutdown().await.unwrap();
}
#[tokio::test]
async fn window_uses_registered_size_and_preserves_missing_and_metadata_error_types() {
    let (_temp, db, hash) = fixture().await;
    let lookup = hash.as_str().to_string();
    db.write(Priority::Normal, move |c| {
        c.execute("UPDATE blobs SET size=size+1 WHERE hash=?1", [lookup])?;
        Ok(())
    })
    .await
    .unwrap();
    assert!(
        matches!(
            db.read_blob_window(hash.clone(), 0, 32).await,
            Err(Error::BlobCorrupt(_))
        ),
        "matching content hash cannot hide a wrong registered size"
    );
    fs::remove_file(db.blobs().path(&hash)).unwrap();
    assert!(matches!(
        db.read_blob_window(hash.clone(), 0, 32).await,
        Err(Error::BlobMissing(_))
    ));
    let unknown = "f".repeat(64).parse().unwrap();
    assert!(matches!(
        db.read_blob_window(unknown, 0, 32).await,
        Err(Error::BlobMetadataMissing(_))
    ));
    let missing: u64 = db
        .read(ReadLane::Interactive, |c| {
            Ok(c.query_row(
                "SELECT count(*) FROM blob_integrity WHERE state IN ('missing','metadata_missing')",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(missing, 2);
    db.shutdown().await.unwrap();
}
