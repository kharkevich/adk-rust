//! [`LanguageServerPool`]: one server per configured language, started lazily through a
//! launcher the host supplies, restarted with backoff, and stopped together.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lsp_types::Url;
use serde_json::Value;

use super::server::{DiagnosticsState, FileDiagnostics, LanguageServer, ServerSettings, file_uri};
use super::{LspError, extension_of};

/// Placeholder in a server's environment values and initialization options that the pool
/// replaces with the server's scratch directory.
pub const SCRATCH_PLACEHOLDER: &str = "${scratch}";

/// The largest file the pool sends to a server.
const MAX_DOCUMENT_BYTES: u64 = 4 * 1024 * 1024;

/// A language server: the command that starts it and the files it serves.
///
/// # Example
///
/// ```rust
/// use adk_devtools::lsp::LanguageServerSpec;
///
/// let spec = LanguageServerSpec::new("zls", "zls").extensions(["zig"]);
/// assert!(spec.serves(std::path::Path::new("src/main.zig")));
/// assert!(LanguageServerSpec::builtin().iter().any(|s| s.name == "rust-analyzer"));
/// ```
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct LanguageServerSpec {
    /// The name the configuration and the audit records use.
    pub name: String,
    /// The program, found on `PATH` when it is a bare name; never downloaded.
    pub command: String,
    /// Its arguments.
    pub args: Vec<String>,
    /// File extensions it serves, without the dot.
    pub extensions: Vec<String>,
    /// `initializationOptions`, also the answer to `workspace/configuration`;
    /// [`SCRATCH_PLACEHOLDER`] in a string becomes the server's scratch directory.
    pub init_options: Option<Value>,
    /// Environment the server gets beyond the launcher's; values may use
    /// [`SCRATCH_PLACEHOLDER`].
    pub env: BTreeMap<String, String>,
    /// How long a quiet server that reported no check after an edit is waited for.
    pub settle: Duration,
}

impl LanguageServerSpec {
    /// A server named `name` started as `command`, serving no extension yet.
    pub fn new(name: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            command: command.into(),
            args: Vec::new(),
            extensions: Vec::new(),
            init_options: None,
            env: BTreeMap::new(),
            settle: Duration::from_millis(300),
        }
    }

    /// Sets the arguments.
    #[must_use]
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    /// Sets the extensions served, without the dot.
    #[must_use]
    pub fn extensions<I, S>(mut self, extensions: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.extensions = extensions
            .into_iter()
            .map(|ext| ext.into().trim_start_matches('.').to_ascii_lowercase())
            .collect();
        self
    }

    /// Sets the initialization options.
    #[must_use]
    pub fn init_options(mut self, options: Value) -> Self {
        self.init_options = Some(options);
        self
    }

    /// Adds an environment variable.
    #[must_use]
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Sets the settle time.
    #[must_use]
    pub fn settle(mut self, settle: Duration) -> Self {
        self.settle = settle;
        self
    }

    /// Whether this server serves `path`, by its extension.
    pub fn serves(&self, path: &Path) -> bool {
        extension_of(path).is_some_and(|ext| self.extensions.iter().any(|served| *served == ext))
    }

    /// The built-in table (spec 3.1 §7.28): `rust-analyzer` for `.rs`,
    /// `typescript-language-server` for `.ts`, `.tsx` and `.js`, `pyright-langserver` for
    /// `.py` and `gopls` for `.go`.
    ///
    /// rust-analyzer builds into its scratch directory (`cargo.targetDir`) with
    /// `CARGO_NET_OFFLINE=true`, so it never takes the workspace's `target` lock or the
    /// network, and waits up to a second after a save for the check it starts.
    pub fn builtin() -> Vec<Self> {
        vec![
            Self::new("rust-analyzer", "rust-analyzer")
                .extensions(["rs"])
                .init_options(serde_json::json!({
                    "cargo": { "targetDir": format!("{SCRATCH_PLACEHOLDER}/target") },
                    "checkOnSave": true,
                }))
                .env("CARGO_NET_OFFLINE", "true")
                .env("CARGO_TARGET_DIR", format!("{SCRATCH_PLACEHOLDER}/target"))
                .settle(Duration::from_secs(1)),
            Self::new("typescript-language-server", "typescript-language-server")
                .args(["--stdio"])
                .extensions(["ts", "tsx", "js"]),
            Self::new("pyright-langserver", "pyright-langserver")
                .args(["--stdio"])
                .extensions(["py"]),
            Self::new("gopls", "gopls").extensions(["go"]).env("GOFLAGS", "-mod=mod").env("GOPROXY", "off"),
        ]
    }

    /// The program on `path_var`: the command itself when it names a path, else the first
    /// executable of that name on `PATH`.
    pub fn resolve_program(&self, path_var: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
        let command = Path::new(&self.command);
        if command.components().count() > 1 {
            return command.is_file().then(|| command.to_path_buf());
        }
        std::env::split_paths(path_var?).filter(|dir| dir.is_absolute()).find_map(|dir| {
            let candidate = dir.join(&self.command);
            #[cfg(windows)]
            let candidate = if candidate.is_file() { candidate } else { candidate.with_extension("exe") };
            candidate.is_file().then_some(candidate)
        })
    }
}

/// What a launcher starts: the resolved program, its arguments and environment, the
/// workspace root and the server's scratch directory.
#[derive(Debug)]
#[non_exhaustive]
pub struct LaunchRequest<'a> {
    /// The server.
    pub spec: &'a LanguageServerSpec,
    /// The program, an absolute path found on `PATH`.
    pub program: &'a Path,
    /// The workspace root, the server's working directory.
    pub root: &'a Path,
    /// A directory the server may write: its home, temporary files and build output.
    pub scratch: &'a Path,
    /// The spec's environment with the scratch directory substituted.
    pub env: BTreeMap<String, String>,
}

/// Builds the command that starts a language server (spec 3.1 §10 D11).
///
/// The pool owns everything around the process: standard streams, the process group, the
/// handshake, timeouts and the kill. A launcher decides what runs and under which
/// isolation, as [`CommandLauncher`](crate::CommandLauncher) does for `bash`.
///
/// # Example
///
/// ```rust
/// use adk_devtools::lsp::{HostServerLauncher, ServerLauncher};
///
/// assert_eq!(HostServerLauncher.isolation(), "host");
/// ```
pub trait ServerLauncher: Send + Sync {
    /// The command that starts the server `request` describes.
    ///
    /// # Errors
    ///
    /// A [`LspError`] when the command cannot be built, for example when a sandbox the
    /// launcher requires is unavailable; the server then does not start.
    fn command(&self, request: &LaunchRequest<'_>) -> Result<tokio::process::Command, LspError>;

    /// A short label for the isolation, such as `"host"` or `"seatbelt"`.
    fn isolation(&self) -> &str;
}

/// Starts servers on the host: the program in the workspace root with an environment
/// cleared to [`DEFAULT_ENV_ALLOWLIST`](crate::DEFAULT_ENV_ALLOWLIST) plus the spec's,
/// and `HOME` and `TMPDIR` moved to the scratch directory. Not an isolation boundary.
#[derive(Clone, Copy, Debug, Default)]
pub struct HostServerLauncher;

impl ServerLauncher for HostServerLauncher {
    fn command(&self, request: &LaunchRequest<'_>) -> Result<tokio::process::Command, LspError> {
        let mut command = tokio::process::Command::new(request.program);
        command.args(&request.spec.args).current_dir(request.root).env_clear();
        for key in crate::DEFAULT_ENV_ALLOWLIST {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        // Toolchain homes keep working with HOME moved.
        for key in ["CARGO_HOME", "RUSTUP_HOME"] {
            let value = std::env::var_os(key).or_else(|| {
                let home = std::env::var_os("HOME")?;
                let dir = Path::new(&home).join(if key == "CARGO_HOME" { ".cargo" } else { ".rustup" });
                dir.is_dir().then(|| dir.into_os_string())
            });
            if let Some(value) = value {
                command.env(key, value);
            }
        }
        command.env("HOME", request.scratch).env("TMPDIR", request.scratch);
        command.envs(&request.env);
        Ok(command)
    }

    fn isolation(&self) -> &str {
        "host"
    }
}

/// Records the processes the pool starts, so a later start can end what a crash left
/// running (spec 3.1 §7.28 "Supervision", shared with background shells, §7.29).
pub trait ProcessRegistry: Send + Sync {
    /// Records a started process.
    fn register(&self, pid: u32, label: &str);
    /// Forgets a process that ended or was ended.
    fn unregister(&self, pid: u32);
}

/// Restart with backoff, in the shape of the MCP server manager's `RestartPolicy`.
///
/// # Example
///
/// ```rust
/// use std::time::Duration;
/// use adk_devtools::lsp::RestartPolicy;
///
/// let policy = RestartPolicy::default();
/// assert_eq!(policy.delay(1), Duration::from_secs(1));
/// assert_eq!(policy.delay(3), Duration::from_secs(4));
/// assert_eq!(policy.delay(30), Duration::from_secs(30));
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct RestartPolicy {
    /// Delay before the first restart. Default: 1 s.
    pub initial_delay: Duration,
    /// Longest delay between restarts. Default: 30 s.
    pub max_delay: Duration,
    /// Multiplier applied after each failure. Default: 2.0.
    pub backoff_multiplier: f64,
    /// Consecutive failures before the pool gives up on a server. Default: 10.
    pub max_restart_attempts: u32,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
            backoff_multiplier: 2.0,
            max_restart_attempts: 10,
        }
    }
}

impl RestartPolicy {
    /// The delay after the `failures`-th consecutive failure.
    pub fn delay(&self, failures: u32) -> Duration {
        let exponent = i32::try_from(failures.saturating_sub(1)).unwrap_or(i32::MAX);
        let secs = self.initial_delay.as_secs_f64() * self.backoff_multiplier.powi(exponent);
        Duration::from_secs_f64(secs.min(self.max_delay.as_secs_f64()).max(0.0))
    }
}

/// Limits and timings of a [`LanguageServerPool`] (spec 3.1 §7.28).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct PoolConfig {
    /// A request without an answer after this fails. Default: 30 s.
    pub request_timeout: Duration,
    /// Open documents per server; the least recently used is closed first. Default: 50.
    pub max_open_documents: usize,
    /// Restart with backoff.
    pub restart: RestartPolicy,
    /// The longest wait for one edited file's diagnostics. Default: 3 s.
    pub diagnostics_per_file: Duration,
    /// The longest wait for one call's diagnostics. Default: 8 s.
    pub diagnostics_per_call: Duration,
    /// Errors listed per file. Default: 20.
    pub max_errors_per_file: usize,
    /// Other files whose new errors are listed after an edit. Default: 5.
    pub max_other_files: usize,
    /// Where each server's scratch directory goes (`<scratch>/<server>`); default a
    /// directory under the system temporary directory keyed by the workspace root.
    pub scratch: Option<PathBuf>,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(30),
            max_open_documents: 50,
            restart: RestartPolicy::default(),
            diagnostics_per_file: Duration::from_secs(3),
            diagnostics_per_call: Duration::from_secs(8),
            max_errors_per_file: 20,
            max_other_files: 5,
            scratch: None,
        }
    }
}

/// A server starting or stopping, for the host's audit records.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PoolEvent {
    /// A server started and answered the handshake.
    Started {
        /// The server's name.
        server: String,
        /// Its process id.
        pid: Option<u32>,
        /// The program that runs.
        program: PathBuf,
        /// The extensions it serves.
        extensions: Vec<String>,
        /// The launcher's isolation label.
        isolation: String,
    },
    /// A server stopped: shut down, exited, or failed to start.
    Stopped {
        /// The server's name.
        server: String,
        /// Its process id, when it had one.
        pid: Option<u32>,
        /// Why.
        reason: String,
    },
}

/// What a server's slot holds.
enum Slot {
    Idle,
    /// A started server, with the consecutive failures before it started.
    Running { server: Arc<LanguageServer>, failures: u32 },
    Backoff { failures: u32, retry_at: Instant, reason: String },
    GaveUp { reason: String },
}

/// What one edit call's diagnostics came to: the edited files, then other files the edit
/// broke.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EditDiagnostics {
    /// One entry per edited file a server serves.
    pub files: Vec<FileDiagnostics>,
    /// Other files with new errors, at most [`PoolConfig::max_other_files`].
    pub others: Vec<FileDiagnostics>,
}

/// Language servers for one workspace (spec 3.1 §7.28, §10 D11).
///
/// Each configured server starts on the first file of an extension it serves, through the
/// [`ServerLauncher`] the host supplies, in a process group of its own. A server that
/// fails to start or exits is started again on its next use after a backoff
/// ([`RestartPolicy`]); after `max_restart_attempts` consecutive failures the pool gives
/// up on it. [`shutdown`](Self::shutdown) stops every server and refuses later starts.
///
/// # Example
///
/// ```rust,no_run
/// use std::sync::Arc;
/// use adk_devtools::lsp::{HostServerLauncher, LanguageServerPool, LanguageServerSpec};
///
/// # async fn run() -> Result<(), adk_devtools::lsp::LspError> {
/// let pool = LanguageServerPool::new("/work/app", LanguageServerSpec::builtin(), Arc::new(HostServerLauncher));
/// let report = pool.diagnostics_after_edit(&["/work/app/src/main.rs".into()]).await;
/// for file in &report.files {
///     println!("{}: {:?} {}", file.path.display(), file.state, file.errors.len());
/// }
/// pool.shutdown().await;
/// # Ok(())
/// # }
/// ```
pub struct LanguageServerPool {
    root: PathBuf,
    specs: Vec<LanguageServerSpec>,
    launcher: Arc<dyn ServerLauncher>,
    config: PoolConfig,
    registry: Option<Arc<dyn ProcessRegistry>>,
    slots: HashMap<String, tokio::sync::Mutex<Slot>>,
    events: Mutex<Vec<PoolEvent>>,
    closed: AtomicBool,
}

impl std::fmt::Debug for LanguageServerPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LanguageServerPool")
            .field("root", &self.root)
            .field("servers", &self.specs.iter().map(|s| &s.name).collect::<Vec<_>>())
            .field("isolation", &self.launcher.isolation())
            .finish_non_exhaustive()
    }
}

impl LanguageServerPool {
    /// A pool for the workspace at `root` with `specs`, started through `launcher`.
    /// A later spec with the same name as an earlier one replaces it.
    pub fn new(
        root: impl Into<PathBuf>,
        specs: Vec<LanguageServerSpec>,
        launcher: Arc<dyn ServerLauncher>,
    ) -> Self {
        let root = root.into();
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        let mut unique: Vec<LanguageServerSpec> = Vec::new();
        for spec in specs {
            unique.retain(|existing| existing.name != spec.name);
            unique.push(spec);
        }
        let slots =
            unique.iter().map(|spec| (spec.name.clone(), tokio::sync::Mutex::new(Slot::Idle))).collect();
        Self {
            root,
            specs: unique,
            launcher,
            config: PoolConfig::default(),
            registry: None,
            slots,
            events: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
        }
    }

    /// Sets the limits and timings.
    #[must_use]
    pub fn with_config(mut self, config: PoolConfig) -> Self {
        self.config = config;
        self
    }

    /// Records every started process in `registry`.
    #[must_use]
    pub fn with_process_registry(mut self, registry: Arc<dyn ProcessRegistry>) -> Self {
        self.registry = Some(registry);
        self
    }

    /// The workspace root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The configured servers.
    pub fn specs(&self) -> &[LanguageServerSpec] {
        &self.specs
    }

    /// The limits and timings.
    pub fn config(&self) -> &PoolConfig {
        &self.config
    }

    /// The launcher's isolation label.
    pub fn isolation(&self) -> &str {
        self.launcher.isolation()
    }

    /// The server that serves `path`, if one is configured.
    pub fn spec_for(&self, path: &Path) -> Option<&LanguageServerSpec> {
        self.specs.iter().find(|spec| spec.serves(path))
    }

    /// Takes the start and stop events since the last call, oldest first.
    pub fn take_events(&self) -> Vec<PoolEvent> {
        std::mem::take(&mut *self.events.lock().unwrap_or_else(|e| e.into_inner()))
    }

    fn event(&self, event: PoolEvent) {
        self.events.lock().unwrap_or_else(|e| e.into_inner()).push(event);
    }

    /// The scratch directory of `spec`'s server.
    fn scratch_for(&self, spec: &LanguageServerSpec) -> std::io::Result<PathBuf> {
        let base = match &self.config.scratch {
            Some(base) => base.clone(),
            None => {
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                std::hash::Hash::hash(&self.root, &mut hasher);
                let key = std::hash::Hasher::finish(&hasher);
                std::env::temp_dir().join("adk-devtools-lsp").join(format!("{key:016x}"))
            }
        };
        let dir = base.join(&spec.name);
        std::fs::create_dir_all(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            for path in [&base, &dir] {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
            }
        }
        std::fs::canonicalize(&dir)
    }

    /// The running server that serves `path`, started now if it is not running.
    ///
    /// # Errors
    ///
    /// [`LspError::NoServer`] when no configured server serves the file,
    /// [`LspError::NotInstalled`] when its program is not on `PATH`, and the start,
    /// backoff and give-up errors of the restart policy.
    pub async fn server_for(&self, path: &Path) -> Result<Arc<LanguageServer>, LspError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(LspError::Closed);
        }
        let spec = self.spec_for(path).ok_or_else(|| {
            LspError::NoServer(extension_of(path).unwrap_or_else(|| path.display().to_string()))
        })?;
        let Some(slot) = self.slots.get(&spec.name) else {
            return Err(LspError::NoServer(spec.name.clone()));
        };
        let mut slot = slot.lock().await;
        let mut failures = 0;
        if let Slot::Running { server, failures: before } = &*slot {
            let (server, before) = (server.clone(), *before);
            let Some(reason) = server.exited() else { return Ok(server) };
            if let (Some(pid), Some(registry)) = (server.pid(), &self.registry) {
                registry.unregister(pid);
            }
            self.event(PoolEvent::Stopped {
                server: spec.name.clone(),
                pid: server.pid(),
                reason: format!("exited: {reason}"),
            });
            // A crash after a long healthy run starts a new failure sequence.
            failures = if server.uptime() < self.config.restart.max_delay { before + 1 } else { 0 };
            *slot = if failures >= self.config.restart.max_restart_attempts {
                Slot::GaveUp { reason: format!("it exited {failures} times in a row: {reason}") }
            } else if failures > 0 {
                Slot::Backoff {
                    failures,
                    retry_at: Instant::now() + self.config.restart.delay(failures),
                    reason,
                }
            } else {
                Slot::Idle
            };
        }
        match &*slot {
            Slot::Backoff { failures: count, retry_at, reason } => {
                if Instant::now() < *retry_at {
                    return Err(LspError::BackingOff {
                        server: spec.name.clone(),
                        secs: retry_at.saturating_duration_since(Instant::now()).as_secs().max(1),
                        reason: reason.clone(),
                    });
                }
                failures = *count;
            }
            Slot::GaveUp { reason } => {
                return Err(LspError::GaveUp { server: spec.name.clone(), reason: reason.clone() });
            }
            Slot::Idle | Slot::Running { .. } => {}
        }
        match self.start(spec).await {
            Ok(server) => {
                let server = Arc::new(server);
                *slot = Slot::Running { server: server.clone(), failures };
                Ok(server)
            }
            Err(error @ LspError::NotInstalled { .. }) => {
                *slot = Slot::GaveUp { reason: error.to_string() };
                Err(error)
            }
            Err(error) => {
                let failures = failures + 1;
                let reason = error.to_string();
                self.event(PoolEvent::Stopped {
                    server: spec.name.clone(),
                    pid: None,
                    reason: format!("did not start: {reason}"),
                });
                *slot = if failures >= self.config.restart.max_restart_attempts {
                    Slot::GaveUp { reason }
                } else {
                    Slot::Backoff {
                        failures,
                        retry_at: Instant::now() + self.config.restart.delay(failures),
                        reason,
                    }
                };
                Err(error)
            }
        }
    }

    async fn start(&self, spec: &LanguageServerSpec) -> Result<LanguageServer, LspError> {
        let program = spec
            .resolve_program(std::env::var_os("PATH").as_deref())
            .ok_or_else(|| LspError::NotInstalled { server: spec.name.clone(), command: spec.command.clone() })?;
        let scratch = self.scratch_for(spec).map_err(|error| LspError::StartFailed {
            server: spec.name.clone(),
            reason: format!("its scratch directory could not be created: {error}"),
        })?;
        let scratch_text = scratch.display().to_string();
        let env = spec
            .env
            .iter()
            .map(|(key, value)| (key.clone(), value.replace(SCRATCH_PLACEHOLDER, &scratch_text)))
            .collect();
        let request =
            LaunchRequest { spec, program: &program, root: &self.root, scratch: &scratch, env };
        let mut command = self.launcher.command(&request)?;
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().map_err(|error| LspError::StartFailed {
            server: spec.name.clone(),
            reason: format!("{} did not start: {error}", program.display()),
        })?;
        let pid = child.id();
        if let (Some(pid), Some(registry)) = (pid, &self.registry) {
            registry.register(pid, &format!("lsp:{}", spec.name));
        }
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(LspError::StartFailed {
                server: spec.name.clone(),
                reason: "its standard streams were not piped".to_string(),
            });
        };
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(drain_stderr(spec.name.clone(), stderr));
        }
        let settings = ServerSettings {
            request_timeout: self.config.request_timeout,
            max_open_documents: self.config.max_open_documents,
            settle: spec.settle,
            init_options: spec.init_options.as_ref().map(|options| substitute(options, &scratch_text)),
        };
        let server =
            LanguageServer::connect(&spec.name, stdout, stdin, &self.root, settings, Some(child)).await;
        match server {
            Ok(server) => {
                tracing::info!(
                    lsp.server = %spec.name,
                    lsp.pid = pid.unwrap_or_default(),
                    lsp.isolation = %self.launcher.isolation(),
                    "language server started"
                );
                self.event(PoolEvent::Started {
                    server: spec.name.clone(),
                    pid,
                    program,
                    extensions: spec.extensions.clone(),
                    isolation: self.launcher.isolation().to_string(),
                });
                Ok(server)
            }
            Err(error) => {
                if let (Some(pid), Some(registry)) = (pid, &self.registry) {
                    registry.unregister(pid);
                }
                Err(error)
            }
        }
    }

    /// Starts the server for `path` and opens the file, so its first edit or query finds
    /// the workspace loaded. Errors are logged, not returned.
    pub async fn warm(&self, path: &Path) {
        if self.spec_for(path).is_none() {
            return;
        }
        if let Err(error) = self.document(path).await {
            tracing::debug!(path = %path.display(), error = %error, "language server warm-up failed");
        }
    }

    /// The server for `path` with the file open at its content on disk.
    ///
    /// # Errors
    ///
    /// As [`server_for`](Self::server_for), and an I/O error when the file cannot be read.
    pub async fn document(&self, path: &Path) -> Result<(Arc<LanguageServer>, Url), LspError> {
        let server = self.server_for(path).await?;
        let text = read_document(path)?;
        let (uri, _, _) = server.sync(path, &text, false).await?;
        Ok((server, uri))
    }

    /// Like [`document`](Self::document), then waits until the server has loaded the
    /// workspace, at most the request timeout, so a navigation request sees it whole.
    ///
    /// # Errors
    ///
    /// As [`document`](Self::document).
    pub async fn ready_document(&self, path: &Path) -> Result<(Arc<LanguageServer>, Url), LspError> {
        let (server, uri) = self.document(path).await?;
        server.wait_ready(Instant::now() + self.config.request_timeout).await;
        Ok((server, uri))
    }

    /// The diagnostics of `paths`, files an edit just wrote, as the servers report them for
    /// the new content (spec 3.1 §7.28 "Freshness", "Volume").
    ///
    /// Each file is sent to its server and saved; the wait is bounded by
    /// [`PoolConfig::diagnostics_per_file`] per file and
    /// [`PoolConfig::diagnostics_per_call`] in all. Files no server serves are left out.
    pub async fn diagnostics_after_edit(&self, paths: &[PathBuf]) -> EditDiagnostics {
        let call_deadline = Instant::now() + self.config.diagnostics_per_call;
        let mut report = EditDiagnostics::default();
        let mut edited: Vec<(Arc<LanguageServer>, Url, u64)> = Vec::new();
        for path in paths {
            let Some(spec) = self.spec_for(path) else { continue };
            let file_deadline = (Instant::now() + self.config.diagnostics_per_file).min(call_deadline);
            let mut entry = FileDiagnostics {
                path: path.clone(),
                server: spec.name.clone(),
                state: DiagnosticsState::Pending,
                errors: Vec::new(),
                omitted: 0,
                text: None,
            };
            let started = tokio::time::timeout_at(file_deadline.into(), self.server_for(path)).await;
            let server = match started {
                Ok(Ok(server)) => server,
                Ok(Err(error)) => {
                    entry.state = DiagnosticsState::Unavailable(error.to_string());
                    report.files.push(entry);
                    continue;
                }
                Err(_) => {
                    report.files.push(entry);
                    continue;
                }
            };
            let text = match read_document(path) {
                Ok(text) => text,
                Err(error) => {
                    entry.state = DiagnosticsState::Unavailable(error.to_string());
                    report.files.push(entry);
                    continue;
                }
            };
            let edited_at = Instant::now();
            match server.sync(path, &text, true).await {
                Ok((uri, version, seq)) => {
                    let (state, errors) =
                        server.wait_fresh(&uri, version, seq, edited_at, file_deadline).await;
                    entry.state = state;
                    entry.omitted = errors.len().saturating_sub(self.config.max_errors_per_file);
                    entry.errors = errors.into_iter().take(self.config.max_errors_per_file).collect();
                    edited.push((server.clone(), uri, seq));
                }
                Err(error) => entry.state = DiagnosticsState::Unavailable(error.to_string()),
            }
            entry.text = Some(text);
            report.files.push(entry);
        }
        let except: Vec<Url> = edited.iter().map(|(_, uri, _)| uri.clone()).collect();
        for (server, _, seq) in &edited {
            for (uri, errors) in server.fresh_others(*seq, &except) {
                if report.others.len() >= self.config.max_other_files {
                    break;
                }
                let Ok(path) = uri.to_file_path() else { continue };
                if report.others.iter().any(|other| other.path == path) {
                    continue;
                }
                let omitted = errors.len().saturating_sub(self.config.max_errors_per_file);
                report.others.push(FileDiagnostics {
                    // Files outside the root are named, never read.
                    text: if path.starts_with(&self.root) { read_document(&path).ok() } else { None },
                    path,
                    server: server.name().to_string(),
                    state: DiagnosticsState::Fresh,
                    errors: errors.into_iter().take(self.config.max_errors_per_file).collect(),
                    omitted,
                });
            }
        }
        report
    }

    /// Stops every running server and refuses later starts.
    pub async fn shutdown(&self) {
        self.closed.store(true, Ordering::Release);
        for (name, slot) in &self.slots {
            let mut slot = slot.lock().await;
            if let Slot::Running { server, .. } = std::mem::replace(&mut *slot, Slot::Idle) {
                server.shutdown().await;
                if let (Some(pid), Some(registry)) = (server.pid(), &self.registry) {
                    registry.unregister(pid);
                }
                self.event(PoolEvent::Stopped {
                    server: name.clone(),
                    pid: server.pid(),
                    reason: "shut down".to_string(),
                });
            }
            *slot = Slot::GaveUp { reason: "the language servers were shut down".to_string() };
        }
    }

    /// The running servers, by name.
    pub async fn running(&self) -> Vec<String> {
        let mut names = Vec::new();
        for (name, slot) in &self.slots {
            if matches!(&*slot.lock().await, Slot::Running { server, .. } if server.exited().is_none()) {
                names.push(name.clone());
            }
        }
        names.sort();
        names
    }
}

impl Drop for LanguageServerPool {
    fn drop(&mut self) {
        // Each server's own Drop ends its process group; the registry forgets them here.
        if let Some(registry) = &self.registry {
            for slot in self.slots.values_mut() {
                if let Slot::Running { server, .. } = slot.get_mut()
                    && let Some(pid) = server.pid()
                {
                    registry.unregister(pid);
                }
            }
        }
    }
}

/// Reads a document to send to a server.
fn read_document(path: &Path) -> Result<String, LspError> {
    let metadata = std::fs::metadata(path)?;
    if metadata.len() > MAX_DOCUMENT_BYTES {
        return Err(LspError::Path(format!(
            "{} is over the 4 MiB a language server is sent",
            path.display()
        )));
    }
    let bytes = std::fs::read(path)?;
    String::from_utf8(bytes).map_err(|_| LspError::Path(format!("{} is not UTF-8 text", path.display())))
}

/// `value` with [`SCRATCH_PLACEHOLDER`] in every string replaced by `scratch`.
fn substitute(value: &Value, scratch: &str) -> Value {
    match value {
        Value::String(text) => Value::String(text.replace(SCRATCH_PLACEHOLDER, scratch)),
        Value::Array(items) => Value::Array(items.iter().map(|item| substitute(item, scratch)).collect()),
        Value::Object(map) => {
            Value::Object(map.iter().map(|(key, item)| (key.clone(), substitute(item, scratch))).collect())
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => value.clone(),
    }
}

/// Logs a server's standard error at debug level, so a full pipe never stalls it.
async fn drain_stderr(name: String, stderr: tokio::process::ChildStderr) {
    use tokio::io::AsyncBufReadExt as _;
    let mut lines = tokio::io::BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        tracing::debug!(lsp.server = %name, line = %line, "language server stderr");
    }
}

/// The `file://` URI of `path`.
///
/// # Errors
///
/// [`LspError::Path`] when `path` is not absolute.
pub fn path_uri(path: &Path) -> Result<Url, LspError> {
    file_uri(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scratch_placeholder_is_replaced_everywhere() {
        let value = serde_json::json!({ "cargo": { "targetDir": "${scratch}/target" }, "n": [1, "${scratch}"] });
        assert_eq!(
            substitute(&value, "/s"),
            serde_json::json!({ "cargo": { "targetDir": "/s/target" }, "n": [1, "/s"] })
        );
    }

    #[test]
    fn a_later_spec_replaces_one_of_the_same_name() {
        let pool = LanguageServerPool::new(
            "/nowhere",
            vec![
                LanguageServerSpec::new("rust-analyzer", "rust-analyzer").extensions(["rs"]),
                LanguageServerSpec::new("rust-analyzer", "/opt/ra").extensions([".RS"]),
            ],
            Arc::new(HostServerLauncher),
        );
        assert_eq!(pool.specs().len(), 1);
        assert_eq!(pool.specs()[0].command, "/opt/ra");
        assert!(pool.spec_for(Path::new("/nowhere/src/lib.rs")).is_some());
        assert!(pool.spec_for(Path::new("/nowhere/README.md")).is_none());
    }

    #[tokio::test]
    async fn a_missing_program_gives_up_at_once() {
        let pool = LanguageServerPool::new(
            std::env::temp_dir(),
            vec![LanguageServerSpec::new("nope", "adk-no-such-language-server").extensions(["nope"])],
            Arc::new(HostServerLauncher),
        );
        let path = std::env::temp_dir().join("x.nope");
        let first = pool.server_for(&path).await.unwrap_err();
        assert!(matches!(first, LspError::NotInstalled { .. }), "{first}");
        let second = pool.server_for(&path).await.unwrap_err();
        assert!(matches!(second, LspError::GaveUp { .. }), "{second}");
        assert!(matches!(pool.server_for(&path.with_extension("md")).await, Err(LspError::NoServer(_))));
    }
}
