//! K1, K5 and K7 on `ScriptedLlm` with a real kernel (spec 3.2 §5, §11): the kernel's durable
//! workers, its scheduler, SQLite stores in `harness.db`, and its control plane on a socket
//! and loopback HTTP, composed as `adk-rust up` composes them, through `Harness::kernel`.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use adk_auth::signed_audit::{AuditExport, AuditLog, AuditSigner, Ed25519Signer, NoteVerifier};
use adk_core::audit_log::{AuditRecorder, InMemoryAuditRecorder};
use adk_core::{
    ApprovalAction, ApprovalPolicy, ApprovalService, ApproverSelector, Clock, DurableRunStore,
    ManualClock, MissedFirePolicy, NewRun, RunFilter, RunState, Schedule, ScheduleSpec,
    ScheduleStore, StoreApprovalService, SystemClock,
};
use adk_harness::{Harness, HarnessAgent, HarnessMode, InstructionsConfig};
use adk_kernel::control::api::{AttachRequest, ProcessesQuery, SpawnRequest};
use adk_kernel::control::{ControlClient, ControlPlaneConfig};
use adk_kernel::{Kernel, KernelConfig, ProcessFilter, QueueClass};
use adk_model::{ScriptedLlm, ScriptedResponse};
use adk_session::{
    SqliteApprovalStore, SqliteAuditLogStore, SqliteRunStore, SqliteScheduleStore,
    SqliteSessionService,
};
use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde_json::json;

use crate::Checks;

/// The app and target the harness runs under, as `adk-rust up` registers it.
const APP: &str = "adk-rust";

pub async fn run(name: &str) -> Result<Checks> {
    match name {
        "k1_leave_it_running" => k1_leave_it_running().await,
        "k5_one_job_many_windows" => k5_one_job_many_windows().await,
        "k7_partitions" => k7_partitions().await,
        other => bail!("unknown scenario {other}"),
    }
}

/// The stores of one kernel home, in `harness.db` as `adk-rust up` keeps them.
struct Stores {
    sessions: Arc<SqliteSessionService>,
    runs: Arc<SqliteRunStore>,
    approvals: Arc<SqliteApprovalStore>,
    schedules: Arc<SqliteScheduleStore>,
    processes: Arc<adk_kernel::SqliteProcessStore>,
    logs: Arc<SqliteAuditLogStore>,
}

/// The stores of `home`, the run store on `clock`, the clock the kernel reads too: the run
/// store decides leases and `not_before`, the kernel wake times and due schedules.
async fn stores(home: &Path, clock: Arc<dyn Clock>) -> Result<Stores> {
    std::fs::create_dir_all(home)?;
    let url = format!("sqlite://{}?mode=rwc", home.join("harness.db").display());
    let sessions = SqliteSessionService::new(&url).await?;
    sessions.migrate().await?;
    let pool = sessions.pool().clone();
    let runs = SqliteRunStore::from_pool(pool.clone()).with_clock(clock);
    runs.migrate().await?;
    let approvals = SqliteApprovalStore::from_pool(pool.clone());
    approvals.migrate().await?;
    let schedules = SqliteScheduleStore::from_pool(pool.clone());
    schedules.migrate().await?;
    let processes = adk_kernel::SqliteProcessStore::new(pool.clone());
    processes.migrate().await?;
    let logs = SqliteAuditLogStore::from_pool(pool);
    logs.migrate().await?;
    Ok(Stores {
        sessions: Arc::new(sessions),
        runs: Arc::new(runs),
        approvals: Arc::new(approvals),
        schedules: Arc::new(schedules),
        processes: Arc::new(processes),
        logs: Arc::new(logs),
    })
}

fn scripted(replies: Vec<ScriptedResponse>) -> Arc<ScriptedLlm> {
    Arc::new(ScriptedLlm::new("scripted", replies))
}

/// How one boot of the kernel runs.
struct Boot {
    model: Arc<ScriptedLlm>,
    mode: HarnessMode,
    control: ControlPlaneConfig,
    audit: Arc<dyn AuditRecorder>,
    clock: Arc<dyn Clock>,
}

/// One boot of the kernel over `stores`: the harness, held calls routed to the `operator`
/// group, built with `Harness::kernel` (spec 3.2 §10 HK1) as `adk-rust up` builds it.
fn boot(stores: &Stores, home: &Path, workspace: &Path, boot: Boot) -> Result<Kernel> {
    let routing = ApprovalPolicy::new([ApproverSelector::group("operator")]).allow_self_approval();
    let service: Arc<dyn ApprovalService> = Arc::new(
        StoreApprovalService::new(stores.approvals.clone())
            .with_router(routing)
            .with_run_store(stores.runs.clone()),
    );
    let agent = Arc::new(
        HarnessAgent::builder(APP)
            .model(boot.model)
            .workspace(adk_devtools::Workspace::new(workspace).allow_bash(true))
            .project_instructions(InstructionsConfig {
                user_file: None,
                ..InstructionsConfig::default()
            })
            .without_default_members()
            .mode(boot.mode)
            .approval_store(stores.approvals.clone())
            .approval_service(service)
            .audit_log(boot.audit)
            .build()?,
    );
    let runner = adk_runner::Runner::builder()
        .app_name(APP)
        .agent(agent.agent())
        .session_service(stores.sessions.clone())
        .approval_store(agent.runtime().approval_store().clone())
        .build()?;
    let harness = Harness::with_runner(agent, runner)?
        .with_run_store(stores.runs.clone())
        .with_goal_queue(QueueClass::BACKGROUND);
    let config = KernelConfig::default()
        .with_app_name(APP)
        .with_home(home)
        .with_scheduler_tick(Duration::from_millis(250))
        .with_control(boot.control)
        .with_clock(boot.clock);
    Ok(harness
        .kernel()
        .config(config)
        .process_store(stores.processes.clone())
        .schedule_store(stores.schedules.clone())
        .build()?)
}

/// Waits until `check` holds, polling every 100 ms, for at most `limit`.
async fn until<F, Fut>(limit: Duration, mut check: F) -> Result<bool>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<bool>>,
{
    let deadline = tokio::time::Instant::now() + limit;
    while tokio::time::Instant::now() < deadline {
        if check().await? {
            return Ok(true);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(false)
}

/// The runs of the nightly schedule.
async fn nightly(stores: &Stores) -> Result<Vec<adk_core::RunRecord>> {
    Ok(stores.runs.list(RunFilter::default().session_id("nightly")).await?)
}

/// K1 without service installation (spec 3.2 §5): a nightly audit is scheduled, the host
/// goes down before its time and comes back the next morning; the next boot runs it once,
/// the process table shows it `Completed`, and the signed audit log verifies. The night
/// passes on a manual clock the run store and the kernel share.
async fn k1_leave_it_running() -> Result<Checks> {
    let mut checks = Checks::default();
    let dir = tempfile::tempdir()?;
    let (home, workspace) = (dir.path().join("home"), dir.path().join("ws"));
    std::fs::create_dir_all(&workspace)?;
    let manual = Arc::new(ManualClock::new(Utc::now()));
    let clock: Arc<dyn Clock> = manual.clone();
    let stores = stores(&home, clock.clone()).await?;
    let signer = Arc::new(Ed25519Signer::generate("kernel-k1")?);
    let control = ControlPlaneConfig::default().without_socket();
    let fire_at = clock.now() + chrono::Duration::hours(2);

    // First boot, in the evening: the schedule is recorded, then the host goes down.
    {
        let log =
            AuditLog::builder(stores.logs.clone(), "kernel-k1", signer.clone()).open().await?;
        let first = Boot {
            model: scripted(Vec::new()),
            mode: HarnessMode::Auto,
            control: control.clone(),
            audit: Arc::new(log),
            clock: clock.clone(),
        };
        let kernel = boot(&stores, &home, &workspace, first)?;
        kernel.start().await?;
        let run =
            NewRun::new("background", APP, "ana", APP, json!({ "text": "audit the repository" }))
                .with_session_id("nightly");
        let schedule = Schedule::new(
            "nightly-audit",
            ScheduleSpec::At(fire_at),
            run,
            MissedFirePolicy::FireOnce,
        )
        .with_next_fire_at(fire_at);
        stores.schedules.upsert(schedule).await?;
        kernel.stop().await?;
    }
    // The host is down overnight, through the schedule's time.
    manual.advance(Duration::from_secs(8 * 60 * 60));
    checks.check("nothing ran while the host was down", nightly(&stores).await?.is_empty());

    // Second boot, the next morning: the missed firing runs once.
    let log =
        Arc::new(AuditLog::builder(stores.logs.clone(), "kernel-k1", signer.clone()).open().await?);
    let second = Boot {
        model: scripted(vec![
            ScriptedResponse::function_call(
                "write_file",
                json!({ "path": "audit.txt", "content": "audited" }),
            ),
            ScriptedResponse::text("audited"),
        ]),
        mode: HarnessMode::Auto,
        control,
        audit: log.clone(),
        clock,
    };
    let kernel = boot(&stores, &home, &workspace, second)?;
    kernel.start().await?;
    let completed = until(Duration::from_secs(30), || async {
        Ok(nightly(&stores).await?.iter().any(|run| run.state.is_terminal()))
    })
    .await?;
    // A second firing would show up within a few scheduler ticks.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let runs = nightly(&stores).await?;
    checks.check("the next boot ran the missed firing", completed);
    checks.check("it ran exactly once", runs.len() == 1);
    checks.check(
        "the run completed",
        runs.first().is_some_and(|run| run.state == RunState::Completed),
    );
    checks.check(
        "the audit was written once",
        std::fs::read_to_string(workspace.join("audit.txt")).unwrap_or_default() == "audited",
    );
    let processes = kernel.processes().ps(ProcessFilter::default()).await?;
    checks.check(
        "the process table shows it completed",
        runs.first().is_some_and(|run| {
            processes.iter().any(|process| {
                process.meta.pid == run.run_id && process.run.state == RunState::Completed
            })
        }),
    );
    kernel.stop().await?;
    // The replica signs a checkpoint over what it recorded, as it does every 256 records or
    // every minute.
    log.checkpoint().await?;
    let verifier: NoteVerifier = signer.verifier_key().parse().context("verifier key")?;
    let export = AuditExport::from_store(stores.logs.as_ref(), "kernel-k1").await?;
    let report = export.verify(&[verifier]);
    let records: Vec<serde_json::Value> = export
        .entries
        .iter()
        .filter_map(|entry| entry.record.as_deref())
        .filter_map(|record| serde_json::from_str(record).ok())
        .collect();
    checks.check("the signed audit log verifies across the restart", report.is_valid());
    checks.check(
        "the log records the write",
        records.iter().any(|record| record["action"] == "write_file"),
    );
    Ok(checks)
}

/// The calls `session` holds, or none while the kernel's worker has not created it yet.
async fn pending_in(
    client: &ControlClient,
    session: &str,
) -> Result<Vec<adk_core::ToolConfirmationRequest>> {
    match client.attach(session, &AttachRequest::default()).await {
        Ok(attached) => Ok(attached.pending),
        Err(error) if error.category() == adk_core::ErrorCategory::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

/// A free loopback port for the kernel's HTTP control plane.
fn free_port() -> Result<SocketAddr> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?)
}

/// K5 (spec 3.2 §5): a turn starts from a terminal on the socket and holds a `bash` call;
/// the terminal goes away; a phone on HTTP approves the call; the run resumes and the
/// command runs once without the terminal; a new shell attaches and sees the same process
/// and transcript.
async fn k5_one_job_many_windows() -> Result<Checks> {
    let mut checks = Checks::default();
    let dir = tempfile::tempdir()?;
    let (home, workspace) = (dir.path().join("home"), dir.path().join("ws"));
    std::fs::create_dir_all(&workspace)?;
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let stores = stores(&home, clock.clone()).await?;
    let addr = free_port()?;
    let recorder = InMemoryAuditRecorder::new();
    let setup = Boot {
        model: scripted(vec![
            ScriptedResponse::function_call(
                "bash",
                json!({ "command": "echo once >> marker.txt" }),
            ),
            ScriptedResponse::text("done"),
        ]),
        mode: HarnessMode::Ask,
        control: ControlPlaneConfig::default().with_http(addr),
        audit: Arc::new(recorder.clone()),
        clock,
    };
    let kernel = boot(&stores, &home, &workspace, setup)?;
    kernel.start().await?;

    // The terminal, on the socket.
    let terminal = ControlClient::connect_local(&home).await?.context("the kernel's socket")?;
    let request =
        SpawnRequest::new(APP, json!({ "text": "run the marker command" })).with_session_id("k5");
    let pid = terminal.spawn(&request).await?.meta.pid;
    let held = until(Duration::from_secs(20), || {
        let terminal = terminal.clone();
        async move { Ok(!pending_in(&terminal, "k5").await?.is_empty()) }
    })
    .await?;
    checks.check("the bash call is held for a decision", held);
    checks.check("nothing ran before the decision", !workspace.join("marker.txt").exists());
    // The laptop lid closes.
    drop(terminal);

    // The phone, on HTTP: another client, holding nothing.
    let phone = ControlClient::http(format!("http://{addr}"));
    let pending = phone.approvals().await?;
    let item = pending.iter().find(|item| item.scope.session_id == "k5");
    checks.check("the phone sees the held call in the inbox", item.is_some());
    if let Some(item) = item {
        phone.decide(&item.id, item.version, ApprovalAction::Approve, None).await?;
    }
    let finished = until(Duration::from_secs(20), || {
        let runs = stores.runs.clone();
        let pid = pid.clone();
        async move { Ok(runs.get(&pid).await?.is_some_and(|run| run.state.is_terminal())) }
    })
    .await?;
    let run = stores.runs.get(&pid).await?;
    checks.check(
        "the run resumed and completed with no terminal attached",
        finished && run.is_some_and(|run| run.state == RunState::Completed),
    );
    checks.check(
        "the command ran once",
        std::fs::read_to_string(workspace.join("marker.txt")).unwrap_or_default() == "once\n",
    );

    // A new shell attaches and sees the same process and transcript.
    let shell = ControlClient::connect_local(&home).await?.context("the kernel's socket")?;
    let attached = shell.attach("k5", &AttachRequest::default()).await?;
    let answered = attached.transcript.iter().any(|event| {
        event.llm_response.content.as_ref().is_some_and(|content| {
            content.parts.iter().any(|part| {
                matches!(part, adk_core::Part::FunctionResponse { function_response, .. } if function_response.name == "bash")
            })
        })
    });
    checks.check("the new shell sees the call's answer in the transcript", answered);
    checks.check("nothing is held any more", attached.pending.is_empty());
    let listed = shell.processes(&ProcessesQuery::default()).await?;
    checks.check(
        "the new shell sees the same process, completed",
        listed
            .iter()
            .any(|process| process.meta.pid == pid && process.run.state == RunState::Completed),
    );
    let controls: Vec<(String, String)> = recorder
        .records()
        .into_iter()
        .filter(|record| record.action == adk_kernel::audit::CONTROL)
        .map(|record| {
            let text = |key: &str| {
                record.ext.get(key).and_then(|value| value.as_str()).unwrap_or_default().to_string()
            };
            (text("action"), text("transport"))
        })
        .collect();
    checks.check(
        "the spawn and the attaches are recorded with their transport",
        controls.contains(&("spawn".into(), "socket".into()))
            && controls.contains(&("attach".into(), "socket".into())),
    );
    kernel.stop().await?;
    Ok(checks)
}

/// A partition's harness for K7, over the shared stores and fact store: memory scoped to the
/// partition, its class on every principal, its crossings under the partition's policy.
fn partition_harness(
    stores: &Stores,
    workspace: &Path,
    partition: adk_kernel::Partition,
    table: &adk_kernel::Partitions,
    coordinator: bool,
    replies: Vec<ScriptedResponse>,
    facts: Arc<adk_harness::SqliteFactStore>,
    service: Arc<dyn ApprovalService>,
) -> Result<Harness> {
    let mut builder = HarnessAgent::builder(APP)
        .model(scripted(replies))
        .workspace(adk_devtools::Workspace::new(workspace))
        .project_instructions(InstructionsConfig {
            user_file: None,
            ..InstructionsConfig::default()
        })
        .without_default_members()
        .approval_store(stores.approvals.clone())
        .approval_service(service)
        .long_term_memory(adk_harness::LongTermMemoryConfig {
            distill: false,
            ..adk_harness::LongTermMemoryConfig::default()
        })
        .fact_store(facts)
        .partition(partition.clone(), table.clone());
    if coordinator {
        builder = builder.partition_coordinator();
    }
    let agent = Arc::new(builder.build()?);
    let runner = adk_runner::Runner::builder()
        .app_name(APP)
        .agent(agent.agent())
        .session_service(stores.sessions.clone())
        .approval_store(agent.runtime().approval_store().clone())
        .identity_source(partition.identity_source(APP, APP))
        .build()?;
    Ok(Harness::with_runner(agent, runner)?.with_run_store(stores.runs.clone()))
}

/// K7 (spec 3.2 §5, §7.8): home and work, each run by its own harness on one kernel, over
/// one `harness.db`. A work agent asks for a home fact and is denied at the policy; the
/// coordinator in home asks for a work fact, its process parks until a person approves the
/// crossing in the inbox, and its identical read after the decision runs without asking;
/// the next process asks again; every crossing is audited, without the query.
async fn k7_partitions() -> Result<Checks> {
    use adk_core::audit_log::AuditTrail;
    use adk_harness::{
        FactKind, FactScope, FactSource, FactStatus, FactStore, MemoryFact, Provenance,
    };
    use adk_kernel::{
        CrossPartition, CrossingFilter, CrossingOutcome, CrossingRun, Partition, PartitionId,
        ProcessKind, ProcessSpec,
    };

    let mut checks = Checks::default();
    let dir = tempfile::tempdir()?;
    let (home, workspace) = (dir.path().join("home"), dir.path().join("ws"));
    std::fs::create_dir_all(&workspace)?;
    let stores = stores(&home, Arc::new(SystemClock)).await?;
    let pool = stores.sessions.pool().clone();
    let partition_store = adk_kernel::SqlitePartitionStore::new(pool.clone());
    partition_store.migrate().await?;
    let facts = adk_harness::SqliteFactStore::new(pool);
    facts.migrate().await?;
    let facts = Arc::new(facts);
    let recorder = InMemoryAuditRecorder::new();
    let table = adk_kernel::Partitions::new(Arc::new(partition_store))
        .with_audit(AuditTrail::new(Arc::new(recorder.clone())));
    for id in ["home", "work"] {
        let partition = Partition::new(id, format!("{id}-agents"))
            .with_cross_partition(CrossPartition::CoordinatorWithApproval);
        table.put(partition, adk_core::audit_log::AuditActor::default()).await?;
    }
    for (project, text) in
        [("home", "dentist appointment on tuesday"), ("work", "standup is at nine")]
    {
        let scope = FactScope::new(APP, "alice", Some(project.into()));
        let provenance = Provenance::new(FactSource::UserStated, "seed", "seed");
        facts
            .put(MemoryFact::new(scope, FactKind::ProjectFact, text, FactStatus::Known, provenance))
            .await?;
    }
    let routing = ApprovalPolicy::new([ApproverSelector::group("operator")]).allow_self_approval();
    let service: Arc<dyn ApprovalService> = Arc::new(
        StoreApprovalService::new(stores.approvals.clone())
            .with_router(routing)
            .with_run_store(stores.runs.clone()),
    );
    let search = |partition: &str, query: &str| {
        ScriptedResponse::function_call(
            adk_harness::PARTITION_MEMORY_SEARCH,
            json!({ "partition": partition, "query": query }),
        )
    };
    let get = |id: &str| table.get(&PartitionId::new(id));
    let home_partition = get("home").await?.context("home")?;
    let work_partition = get("work").await?.context("work")?;
    let coordinator = partition_harness(
        &stores,
        &workspace,
        home_partition,
        &table,
        true,
        vec![
            search("work", "standup"),
            search("work", "standup"),
            ScriptedResponse::text("Standup is at nine."),
            search("work", "standup"),
            ScriptedResponse::text("asked again"),
        ],
        facts.clone(),
        service.clone(),
    )?;
    let work = partition_harness(
        &stores,
        &workspace,
        work_partition,
        &table,
        false,
        vec![search("home", "dentist"), ScriptedResponse::text("not mine to see")],
        facts,
        service,
    )?;
    let kernel = work
        .host_partition(coordinator.kernel())
        .config(
            KernelConfig::default()
                .with_app_name(APP)
                .with_home(&home)
                .with_scheduler_tick(Duration::from_millis(250))
                .with_control(ControlPlaneConfig::default().without_socket()),
        )
        .process_store(stores.processes.clone())
        .build()?;
    kernel.start().await?;
    let spawn = |target: &str, session: &str| {
        let run =
            NewRun::new(QueueClass::INTERACTIVE, APP, "alice", target, json!({ "text": "?" }))
                .with_session_id(session)
                .with_authenticated_user();
        kernel.processes().spawn(run, ProcessSpec::new(ProcessKind::Interactive))
    };
    let state = |pid: String| {
        let runs = stores.runs.clone();
        async move { Ok::<_, anyhow::Error>(runs.get(&pid).await?.map(|run| run.state)) }
    };
    let reaches = |pid: String, wanted: RunState| {
        let state = state.clone();
        async move {
            until(Duration::from_secs(20), || {
                let (pid, state) = (pid.clone(), state.clone());
                async move { Ok(state(pid).await? == Some(wanted)) }
            })
            .await
        }
    };

    let asked = spawn("adk-rust@work", "w1").await?;
    checks.check("the work agent's process runs in work", asked.meta.partition.as_str() == "work");
    checks.check("it completes", reaches(asked.meta.pid.clone(), RunState::Completed).await?);
    let denied = table.crossings(&CrossingFilter::default().from("work")).await?;
    checks.check(
        "its read of a home fact was denied at the policy",
        denied.len() == 1 && denied[0].outcome == CrossingOutcome::Denied,
    );

    let parked = spawn("adk-rust@home", "h1").await?;
    let pid = parked.meta.pid.clone();
    checks.check(
        "the coordinator's crossing parks its process",
        reaches(pid.clone(), RunState::Waiting).await?,
    );
    let alice = adk_core::UserId::new("alice")?;
    let h1 = adk_core::SessionId::new("h1")?;
    let pending = coordinator.pending_approvals(&alice, &h1).await?;
    checks.check("one crossing waits in the inbox", pending.len() == 1);
    if let Some(item) = pending.first() {
        coordinator
            .decide(&alice, &h1, &item.fingerprint, adk_harness::HarnessDecision::Approve)
            .await?;
    }
    checks
        .check("the approved process completes", reaches(pid.clone(), RunState::Completed).await?);
    let ledger: Vec<CrossingOutcome> = table
        .crossings(&CrossingFilter::default().run(&pid))
        .await?
        .into_iter()
        .map(|record| record.outcome)
        .collect();
    checks.check(
        "its identical second read ran without asking",
        ledger == [CrossingOutcome::Held, CrossingOutcome::Approved, CrossingOutcome::RunGrant],
    );
    let released = until(Duration::from_secs(5), || {
        let table = table.clone();
        let run = CrossingRun::Durable(pid.clone());
        async move { Ok(table.grants(&run).await?.is_empty()) }
    })
    .await?;
    checks.check("the finished run holds no grant", released);

    let next = spawn("adk-rust@home", "h2").await?;
    checks.check(
        "the next process asks again",
        reaches(next.meta.pid.clone(), RunState::Waiting).await?,
    );
    let records: Vec<_> = recorder
        .records()
        .into_iter()
        .filter(|draft| draft.action == adk_kernel::audit::PARTITION_CROSSING)
        .collect();
    checks.check("every crossing is audited", records.len() == 5);
    checks.check(
        "without the query",
        records.iter().all(|draft| {
            !serde_json::to_string(&draft.ext).unwrap_or_default().contains("standup")
        }),
    );
    kernel.processes().kill(&next.meta.pid, "the scenario is over").await?;
    kernel.stop().await?;
    Ok(checks)
}
