//! Writers on one SQLite run-store file wait for each other instead of failing.
//!
//! The single-statement writes (`heartbeat`, `set_checkpoint`, `wait`, `wake`, `finish`,
//! `request_cancel`) ran in autocommit mode: each read under a shared lock, then asked to
//! upgrade it while another connection was committing, which SQLite refuses at once with
//! `database is locked` rather than waiting — its busy handler cannot break that deadlock. A
//! worker whose park or heartbeat failed this way lost its lease, so a parked run woke only
//! after the lease expired. On Windows, whose file locks are slow enough to widen the window,
//! a two-replica round trip took five seconds and sometimes timed out. Each write now takes
//! the write lock first (`BEGIN IMMEDIATE`), which the busy handler waits for.

#![cfg(feature = "sqlite")]

use std::sync::Arc;
use std::time::Duration;

use adk_core::{DurableRunStore, NewRun, RunCheckpointRef};
use adk_session::SqliteRunStore;
use serde_json::json;

async fn open(url: &str) -> Arc<SqliteRunStore> {
    let store = SqliteRunStore::new(url).await.expect("SQLite opens");
    store.migrate().await.expect("run schema migrates");
    Arc::new(store)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_writers_on_two_pools_never_see_database_is_locked() {
    let path = std::env::temp_dir().join(format!("adk-run-contention-{}.db", uuid::Uuid::new_v4()));
    let url = format!("sqlite://{}?mode=rwc", path.display());
    // Two pools over one file, as two replicas (or processes) would open it.
    let stores = [open(&url).await, open(&url).await];

    let mut claimed = Vec::new();
    for index in 0..8 {
        let queue = format!("q{index}");
        stores[0]
            .enqueue(NewRun::new(&queue, "app", "user", "agent", json!({ "n": index })))
            .await
            .unwrap();
        let run = stores[0].claim(&queue, "w", Duration::from_secs(60)).await.unwrap().unwrap();
        claimed.push(run);
    }

    let mut tasks = Vec::new();
    for (index, run) in claimed.into_iter().enumerate() {
        let store = Arc::clone(&stores[index % 2]);
        tasks.push(tokio::spawn(async move {
            let mut failures = Vec::new();
            for round in 0..60 {
                let result = if round % 2 == 0 {
                    store
                        .heartbeat(&run.record.run_id, run.epoch, Duration::from_secs(60))
                        .await
                        .map(|_| ())
                } else {
                    let checkpoint = RunCheckpointRef::Session {
                        session_id: format!("s{index}"),
                        invocation_id: Some(format!("inv-{round}")),
                    };
                    store.set_checkpoint(&run.record.run_id, run.epoch, checkpoint).await
                };
                if let Err(error) = result {
                    failures.push(error.to_string());
                }
                store.wake(&run.record.run_id).await.err().into_iter().for_each(|error| {
                    failures.push(error.to_string());
                });
            }
            failures
        }));
    }
    let mut failures = Vec::new();
    for task in tasks {
        failures.extend(task.await.unwrap());
    }
    drop(stores);
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    assert!(failures.is_empty(), "{} writes failed, first: {:?}", failures.len(), failures.first());
}
