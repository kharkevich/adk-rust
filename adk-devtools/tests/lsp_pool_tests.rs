//! `LanguageServerPool` against a scripted server process (spec 3.1 §7.28, §10 D11):
//! freshness after an edit, other files an edit broke, restart with backoff, the process
//! registry and the process-group kill on shutdown.
#![cfg(all(unix, feature = "lsp"))]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use adk_devtools::lsp::lsp_types::request::GotoDefinition;
use adk_devtools::lsp::lsp_types::{
    GotoDefinitionParams, GotoDefinitionResponse, Position, TextDocumentIdentifier,
    TextDocumentPositionParams,
};
use adk_devtools::lsp::{
    DiagnosticsState, HostServerLauncher, LanguageServerPool, LanguageServerSpec, LspError,
    PoolConfig, PoolEvent, ProcessRegistry, RestartPolicy,
};

fn python() -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join("python3"))
        .find(|candidate| candidate.is_file())
}

fn fixture() -> String {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_lsp.py").display().to_string()
}

fn spec(mode: &str, pids: Option<&Path>) -> Option<LanguageServerSpec> {
    let python = python()?;
    let mut args = vec!["-I".to_string(), fixture(), mode.to_string()];
    if let Some(pids) = pids {
        args.push(pids.display().to_string());
    }
    Some(LanguageServerSpec::new("fake", python.display().to_string()).args(args).extensions(["rs"]))
}

fn config(scratch: &Path) -> PoolConfig {
    let mut config = PoolConfig::default();
    config.request_timeout = Duration::from_secs(5);
    config.diagnostics_per_file = Duration::from_secs(3);
    config.restart = RestartPolicy {
        initial_delay: Duration::from_millis(100),
        max_delay: Duration::from_secs(1),
        backoff_multiplier: 2.0,
        max_restart_attempts: 3,
    };
    config.scratch = Some(scratch.to_path_buf());
    config
}

#[derive(Default)]
struct Registry(Mutex<Vec<(String, u32)>>);

impl ProcessRegistry for Registry {
    fn register(&self, pid: u32, label: &str) {
        self.0.lock().unwrap().push((format!("+{label}"), pid));
    }
    fn unregister(&self, pid: u32) {
        self.0.lock().unwrap().push(("-".to_string(), pid));
    }
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[tokio::test]
async fn an_edit_gets_fresh_errors_and_the_files_it_broke() {
    let dir = tempfile::tempdir().unwrap();
    let Some(spec) = spec("normal", None) else { return };
    let pool = LanguageServerPool::new(dir.path(), vec![spec], Arc::new(HostServerLauncher))
        .with_config(config(&dir.path().join(".scratch")));
    let file = pool.root().join("main.rs");
    std::fs::write(&file, "fn main() {}\n").unwrap();
    pool.warm(&file).await;
    assert_eq!(pool.running().await, vec!["fake".to_string()]);

    std::fs::write(&file, "fn main() {\n    ERROR; // WARN\n}\n// BREAK_OTHER\n").unwrap();
    let report = pool.diagnostics_after_edit(std::slice::from_ref(&file)).await;
    assert_eq!(report.files.len(), 1);
    let edited = &report.files[0];
    assert_eq!(edited.state, DiagnosticsState::Fresh);
    assert_eq!(edited.errors.len(), 1, "errors only: {:?}", edited.errors);
    assert_eq!(edited.errors[0].range.start, Position::new(1, 4));
    assert_eq!(report.others.len(), 1, "{:?}", report.others);
    assert!(report.others[0].path.ends_with("other.rs"));

    // Navigation goes through the same server.
    let (server, uri) = pool.ready_document(&file).await.unwrap();
    let answer = server
        .request::<GotoDefinition>(GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                position: Position::new(1, 4),
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        })
        .await
        .unwrap();
    assert!(matches!(answer, Some(GotoDefinitionResponse::Array(locations)) if locations[0].uri == uri));

    let events = pool.take_events();
    assert!(matches!(&events[0], PoolEvent::Started { server, isolation, .. } if server == "fake" && isolation == "host"));
    pool.shutdown().await;
    assert!(matches!(pool.server_for(&file).await, Err(LspError::Closed)));
}

#[tokio::test]
async fn shutdown_ends_the_server_and_its_children() {
    let dir = tempfile::tempdir().unwrap();
    let pids = dir.path().join("pids");
    let Some(spec) = spec("normal", Some(&pids)) else { return };
    let registry = Arc::new(Registry::default());
    let pool = LanguageServerPool::new(dir.path(), vec![spec], Arc::new(HostServerLauncher))
        .with_config(config(&dir.path().join(".scratch")))
        .with_process_registry(registry.clone());
    let file = pool.root().join("lib.rs");
    std::fs::write(&file, "pub fn f() {}\n").unwrap();
    let server = pool.server_for(&file).await.unwrap();
    let server_pid = server.pid().unwrap();
    let text = std::fs::read_to_string(&pids).unwrap();
    let mut parts = text.split_whitespace().map(|pid| pid.parse::<i32>().unwrap());
    let (leader, child) = (parts.next().unwrap(), parts.next().unwrap());
    assert!(alive(leader) && alive(child));
    drop(server);

    pool.shutdown().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!alive(child), "the server's child is in its process group and ends with it");
    let records = registry.0.lock().unwrap().clone();
    assert_eq!(records, vec![("+lsp:fake".to_string(), server_pid), ("-".to_string(), server_pid)]);
    assert!(pool.take_events().iter().any(|event| matches!(event, PoolEvent::Stopped { reason, .. } if reason == "shut down")));
}

#[tokio::test]
async fn a_crashing_server_restarts_with_backoff_then_gives_up() {
    let dir = tempfile::tempdir().unwrap();
    let Some(spec) = spec("crash-after-init", None) else { return };
    let pool = LanguageServerPool::new(dir.path(), vec![spec], Arc::new(HostServerLauncher))
        .with_config(config(&dir.path().join(".scratch")));
    let file = pool.root().join("x.rs");
    std::fs::write(&file, "").unwrap();
    let mut starts = 0;
    let mut backed_off = 0;
    for _ in 0..40 {
        match pool.server_for(&file).await {
            Ok(_) => starts += 1,
            Err(LspError::BackingOff { .. }) => backed_off += 1,
            Err(LspError::GaveUp { reason, .. }) => {
                assert!(reason.contains("exited 3 times"), "{reason}");
                assert_eq!(starts, 3, "the policy allows 3 consecutive failures");
                assert!(backed_off > 0);
                return;
            }
            Err(other) => panic!("unexpected: {other}"),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the pool never gave up: {starts} starts, {backed_off} backoffs");
}

#[tokio::test]
async fn a_server_that_never_answers_fails_its_start_within_the_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let Some(spec) = spec("never-answers", None) else { return };
    let mut config = config(&dir.path().join(".scratch"));
    config.request_timeout = Duration::from_millis(300);
    let pool =
        LanguageServerPool::new(dir.path(), vec![spec], Arc::new(HostServerLauncher)).with_config(config);
    let file = pool.root().join("y.rs");
    std::fs::write(&file, "").unwrap();
    let report = pool.diagnostics_after_edit(std::slice::from_ref(&file)).await;
    assert!(
        matches!(&report.files[0].state, DiagnosticsState::Unavailable(reason) if reason.contains("initialize")),
        "{:?}",
        report.files[0].state
    );
    assert!(matches!(pool.server_for(&file).await, Err(LspError::BackingOff { .. })));
}
