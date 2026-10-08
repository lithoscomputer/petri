//! The run directory after a crash: a crash can cut a log's last line at
//! any byte. A write that fails inside a live process is covered by the
//! backend's own tests (`run_dir.rs`).

use std::fs;

use store::{Access, COORDINATOR_FILE, LogId, OwnerId, RunDirStore, RunKey, RunStore as _};
use testkit::RunDir;
use testkit::run_store::record;

/// Every cut of the last line, at every byte: a reader sees the records
/// before it, and a new writer truncates the cut and appends after them.
#[tokio::test]
async fn a_crash_may_cut_the_last_line_anywhere() {
    let key = RunKey::new("torn");
    let whole = {
        let dir = RunDir::new("store-torn-whole");
        let store = RunDirStore::new(dir.path());
        let logs = store
            .open(&key, Access::Create {
                owner: OwnerId::new("first"),
            })
            .await
            .expect("creates");
        for seq in 0..3 {
            logs.append(&LogId::Coordinator, &[record(seq, "event")])
                .await
                .expect("appends");
        }
        drop(logs);
        fs::read_to_string(dir.path().join(COORDINATOR_FILE)).expect("the log")
    };
    let last = whole[..whole.len() - 1].rfind('\n').expect("two lines") + 1;
    for cut in last..whole.len() {
        let dir = RunDir::new("store-torn-cut");
        let store = RunDirStore::new(dir.path());
        drop(
            store
                .open(&key, Access::Create {
                    owner: OwnerId::new("first"),
                })
                .await
                .expect("creates"),
        );
        let path = dir.path().join(COORDINATOR_FILE);
        fs::write(&path, &whole[..cut]).expect("writes the cut log");

        let reader = store.open(&key, Access::Read).await.expect("reads");
        assert_eq!(
            reader.read(&LogId::Coordinator).await.expect("reads"),
            vec![record(0, "event"), record(1, "event")],
            "cut at byte {cut}"
        );
        let writer = store
            .open(&key, Access::Write {
                owner: OwnerId::new("second"),
            })
            .await
            .expect("takes the run");
        writer
            .append(&LogId::Coordinator, &[record(2, "again")])
            .await
            .unwrap_or_else(|error| panic!("cut at byte {cut}: {error}"));
        assert_eq!(
            writer.read(&LogId::Coordinator).await.expect("reads"),
            vec![record(0, "event"), record(1, "event"), record(2, "again")],
            "cut at byte {cut}"
        );
    }
}
