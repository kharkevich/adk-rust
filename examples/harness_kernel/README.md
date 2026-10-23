# harness_kernel

Scenarios for the agent kernel (`adk-kernel`, spec 3.2 §5, §11). Every pass condition is
asserted on stores, processes, files and audit records, never on model prose.

## Scripted scenarios

No keys needed. Each scenario boots a real kernel — a durable worker per queue, the
scheduler, SQLite stores in `harness.db` and the control plane — over a harness on a
scripted model, built with `Harness::kernel` as `adk-rust up` builds it, and prints a
main-versus-branch table. `main` has no kernel, so its cells read `not run, expected: …`, stating what `main`
does instead.

```bash
cargo run                          # every scripted scenario
cargo run -- k5_one_job_many_windows
```

| Scenario | What passes |
|---|---|
| `k1_leave_it_running` | K1 without service installation, on a manual clock the run store and the kernel share: a nightly audit is scheduled two hours ahead, the kernel stops, the clock moves eight hours on while it is down, and the kernel boots again; nothing ran while it was down, the next boot runs the missed firing exactly once, the run completes, the file is written once, `ps` shows the process `Completed`, and the replica's signed audit log verifies across the restart once it signs a checkpoint |
| `k5_one_job_many_windows` | K5: a turn spawned from a terminal on the socket holds its `bash` call; the terminal goes away; a phone on loopback HTTP approves the call from the inbox; the run resumes and the command runs once with no terminal attached; a new shell attaches and sees the same process, completed, and the call's answer in the transcript; the spawn and the attach are recorded as `kernel.control` with their transport |
| `k7_partitions` | K7: home and work, each run by its own harness on one kernel over one `harness.db`, with partitions and facts in SQLite. The work agent's read of a home fact is denied at the policy and audited; the coordinator's read of a work fact parks its process until it is approved in the inbox, its identical second read runs without asking, the finished run holds no grant, and the next process asks again; every crossing is a `kernel.partition.crossing` record without the query |

## Live scenarios

The live scenarios run the `adk-rust up` binary in a private home and reach it over its
control plane, as any surface does.

```bash
cargo build -p adk-cli --bin adk-rust          # from the workspace root
cp .env.example .env                           # add the keys you have
ADK_RUST_BIN=../../target/debug/adk-rust cargo run -- --live
cargo run -- --live --provider anthropic --bin ../../target/debug/adk-rust
```

`HARNESS_ENV_FILE=/path/to/.env` loads keys from another file. The providers are
`anthropic` (`claude-sonnet-5-5`) and `gemini` (`gemini-3.5-flash-lite`); one without a key
is skipped.

| Scenario | What passes |
|---|---|
| `live_k5_approve_from_another_process` | `up --mode ask`: the model's `bash` call is held; the client that started the turn disconnects; `adk-rust approvals --server … approve` in another process approves it over HTTP; the turn completes, the command ran exactly once, and a new client attaching sees the call in the transcript |
| `live_k3_file_change_wakes` | `up --mode ask`: a process spawned asleep on a `notes.md` file wake makes no model call; saving the file wakes it within two seconds; the turn reads the file with `read_file` and answers with its codeword |
| `live_k7_partitions` | `up` with `[[kernel.partitions]]` for home (the coordinator) and work: each partition's agent remembers a fact in its own scope; the work agent asks for the home fact and is denied at the policy; the coordinator asks for the work fact, its process parks until the crossing is approved in the inbox, it answers from work's memory, and the partition ledger shows the crossing held, approved, then let through for the rest of the run |
