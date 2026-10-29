//! Subprocess interruption tests: the parent terminates the worker at explicit barriers.
use excalibur_store::{Database, DatabaseKind, Priority, ReadLane, StorageConfig};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

fn ready(root: &Path) {
    fs::write(root.join("ready"), b"ready").unwrap();
}
fn park() -> ! {
    loop {
        std::thread::park();
    }
}
struct InterruptedInput {
    root: PathBuf,
    first: bool,
}
impl Read for InterruptedInput {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.first {
            self.first = false;
            buf.fill(37);
            Ok(buf.len())
        } else {
            ready(&self.root);
            park()
        }
    }
}
#[test]
fn crash_worker() {
    let Ok(root) = std::env::var("EXCALIBUR_CRASH_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let phase = std::env::var("EXCALIBUR_CRASH_PHASE").unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async{
        let db=Database::open(root.join("excalibur.db"),DatabaseKind::Main,StorageConfig::default()).unwrap();
        db.write(Priority::Interactive,|c|{c.execute("INSERT INTO messages(id,text) VALUES('confirmed','survives')",[])?;Ok(())}).await.unwrap();
        let prepared=db.blobs().prepare(b"confirmed bytes").unwrap();
        db.attach_blob(prepared,"message".into(),"confirmed".into(),"output".into()).await.unwrap();
        match phase.as_str(){
            "group"=>{db.write(Priority::Normal,move|c|{
                c.execute("INSERT INTO messages(id,text) VALUES('uncommitted','must rollback')",[])?;
                ready(&root);park()
            }).await.unwrap_or(());},
            "commit"=>{
                db.write(Priority::Normal,move|c|{
                    c.commit_hook(Some(move||{ready(&root);park()}));
                    c.execute("INSERT INTO messages(id,text) VALUES('uncommitted','at commit barrier')",[])?;
                    Ok(())
                }).await.unwrap();
            },
            "blob"=>{db.blobs().prepare_reader(InterruptedInput{root,first:true}).unwrap();},
            "published"=>{
                let staged=db.blobs().prepare(b"published but uncommitted orphan").unwrap();
                db.write_with_blob(staged,Priority::Normal,move|c,hash|{
                    c.execute("INSERT INTO blob_refs(owner_type,owner_id,slot,hash) VALUES('test','uncommitted','output',?1)",[hash.as_str()])?;
                    ready(&root);park()
                }).await.unwrap_or(());
            },
            _=>panic!("invalid test phase"),
        }
    });
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn process_kill_during_group_staging_and_publication_preserves_references() {
    for phase in ["group", "commit", "blob", "published"] {
        let dir = tempfile::tempdir().unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "crash_worker", "--nocapture"])
            .env("EXCALIBUR_CRASH_ROOT", dir.path())
            .env("EXCALIBUR_CRASH_PHASE", phase)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let start = Instant::now();
        while !dir.path().join("ready").exists() {
            if let Some(status) = child.try_wait().unwrap() {
                panic!("worker exited before {phase} barrier: {status}");
            }
            if start.elapsed() > Duration::from_secs(15) {
                child.kill().unwrap();
                panic!("worker did not reach {phase} barrier");
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        child.kill().unwrap();
        child.wait().unwrap();
        let db = Database::open(
            dir.path().join("excalibur.db"),
            DatabaseKind::Main,
            StorageConfig::default(),
        )
        .unwrap();
        let rows = db
            .read(ReadLane::Interactive, |c| {
                assert_eq!(
                    c.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0))?,
                    "ok"
                );
                assert_eq!(
                    c.query_row(
                        "SELECT count(*) FROM messages WHERE id='confirmed'",
                        [],
                        |r| r.get::<_, i64>(0)
                    )?,
                    1
                );
                assert_eq!(
                    c.query_row(
                        "SELECT count(*) FROM messages WHERE id='uncommitted'",
                        [],
                        |r| r.get::<_, i64>(0)
                    )?,
                    0
                );
                let mut s = c.prepare(
                    "SELECT b.hash,b.size FROM blob_refs r JOIN blobs b ON b.hash=r.hash",
                )?;
                Ok(
                    s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u64>(1)?)))?
                        .collect::<std::result::Result<Vec<_>, _>>()?,
                )
            })
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "{phase}: uncommitted refs must not survive");
        for (hash, size) in rows {
            assert_eq!(
                db.blobs().read(&hash.parse().unwrap(), size).unwrap(),
                b"confirmed bytes"
            );
        }
        db.shutdown().await.unwrap();
        println!(
            "crash phase={phase}: confirmed records persisted, quick_check=ok, dangling_refs=0"
        );
    }
}
