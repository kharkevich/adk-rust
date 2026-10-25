//! One running language server: the initialize handshake, document versions, published
//! diagnostics and the progress that tells when a check finished.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lsp_types::{
    Diagnostic, DiagnosticSeverity, NumberOrString, ProgressParams, ProgressParamsValue,
    PublishDiagnosticsParams, ServerCapabilities, TextDocumentSyncCapability,
    TextDocumentSyncSaveOptions, Url, WorkDoneProgress,
};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::watch;

use super::rpc::{
    CONTENT_MODIFIED, CallError, Connection, METHOD_NOT_FOUND, RpcError, SERVER_CANCELLED,
    read_message,
};
use super::{LspError, language_id};

/// How fresh the diagnostics of an edited file are (spec 3.1 §7.28 "Freshness").
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DiagnosticsState {
    /// Published for the post-edit version, or the server reported its check finished
    /// after the edit, and it has gone quiet.
    Fresh,
    /// Published for the post-edit version, but the server was still checking when the
    /// wait ran out: more errors may follow.
    Checking,
    /// Nothing newer than the edit arrived in time; nothing is listed.
    Pending,
    /// No server answered for the file, with the reason.
    Unavailable(String),
}

/// Errors a server reported for one file after an edit.
#[derive(Clone, Debug, PartialEq)]
pub struct FileDiagnostics {
    /// The file.
    pub path: std::path::PathBuf,
    /// The server that reported them.
    pub server: String,
    /// How fresh they are; only [`Fresh`](DiagnosticsState::Fresh) and
    /// [`Checking`](DiagnosticsState::Checking) list errors.
    pub state: DiagnosticsState,
    /// Error-severity diagnostics, at most the pool's per-file cap.
    pub errors: Vec<Diagnostic>,
    /// Errors past the cap.
    pub omitted: usize,
    /// The file's text when it was synchronised, for rendering columns.
    pub text: Option<String>,
}

/// A published set of diagnostics and when it arrived.
#[derive(Clone, Debug)]
struct Published {
    version: Option<i32>,
    seq: u64,
    diagnostics: Vec<Diagnostic>,
}

/// What the reader task learns from the server.
#[derive(Debug, Default)]
struct Inner {
    /// Counts every event; "after the edit" means a larger number than the edit's.
    seq: u64,
    published: HashMap<Url, Published>,
    /// Progress tokens begun and not ended.
    active: HashMap<String, u64>,
    /// rust-analyzer's `experimental/serverStatus` quiescence, when it sends it.
    quiescent: Option<bool>,
    /// The event at which the server last went from busy to idle.
    idle_since: u64,
    /// Whether the server ever published a versioned set.
    versioned: bool,
    /// Why the connection ended.
    exited: Option<String>,
}

impl Inner {
    fn busy(&self) -> bool {
        !self.active.is_empty() || self.quiescent == Some(false)
    }
}

/// The shared state the reader task writes and waiters watch.
#[derive(Debug)]
pub(crate) struct ServerState {
    inner: Mutex<Inner>,
    tx: watch::Sender<u64>,
}

impl ServerState {
    fn new() -> Self {
        Self { inner: Mutex::new(Inner::default()), tx: watch::channel(0).0 }
    }

    /// Applies `change` as one event, and marks the moment the server goes idle.
    fn update(&self, change: impl FnOnce(&mut Inner)) {
        let seq = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let was_busy = inner.busy();
            inner.seq += 1;
            change(&mut inner);
            if was_busy && !inner.busy() {
                inner.idle_since = inner.seq;
            }
            inner.seq
        };
        self.tx.send_replace(seq);
    }

    fn seq(&self) -> u64 {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).seq
    }

    fn exited(&self) -> Option<String> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).exited.clone()
    }
}

/// An open document.
#[derive(Debug)]
struct Doc {
    version: i32,
    digest: u64,
    used: u64,
}

/// The documents a server has open, closed least recently used first.
#[derive(Debug, Default)]
struct Documents {
    open: HashMap<Url, Doc>,
    tick: u64,
    /// Versions increase across every document, so a reopened document never reuses a
    /// version an earlier set of diagnostics carries.
    next_version: i32,
}

fn digest(text: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// Settings a running server keeps.
#[derive(Clone, Debug)]
pub(crate) struct ServerSettings {
    pub(crate) request_timeout: Duration,
    pub(crate) max_open_documents: usize,
    /// How long after an edit a quiet server with a versioned publish counts as settled
    /// when it reported no check.
    pub(crate) settle: Duration,
    pub(crate) init_options: Option<Value>,
}

/// One running language server over stdio JSON-RPC (spec 3.1 §10 D11).
///
/// Started by [`LanguageServerPool`](super::LanguageServerPool), which owns its lifecycle;
/// the methods here answer requests, keep documents in step with the disk and wait for
/// fresh diagnostics.
pub struct LanguageServer {
    name: String,
    conn: Arc<Connection>,
    state: Arc<ServerState>,
    capabilities: ServerCapabilities,
    docs: tokio::sync::Mutex<Documents>,
    settings: ServerSettings,
    child: Mutex<Option<tokio::process::Child>>,
    pid: Option<u32>,
    started: Instant,
    reader: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for LanguageServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LanguageServer")
            .field("name", &self.name)
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

impl LanguageServer {
    /// Connects to a server over `reader` and `writer` and runs the initialize handshake
    /// for the workspace at `root`.
    pub(crate) async fn connect(
        name: &str,
        reader: impl AsyncRead + Send + Unpin + 'static,
        writer: impl AsyncWrite + Send + Unpin + 'static,
        root: &Path,
        settings: ServerSettings,
        child: Option<tokio::process::Child>,
    ) -> Result<Self, LspError> {
        let pid = child.as_ref().and_then(tokio::process::Child::id);
        let conn = Arc::new(Connection::new(Box::new(writer)));
        let state = Arc::new(ServerState::new());
        let reader = tokio::spawn(read_loop(
            name.to_string(),
            reader,
            conn.clone(),
            state.clone(),
            settings.init_options.clone(),
        ));
        let mut server = Self {
            name: name.to_string(),
            conn,
            state,
            capabilities: ServerCapabilities::default(),
            docs: tokio::sync::Mutex::new(Documents::default()),
            settings,
            child: Mutex::new(child),
            pid,
            started: Instant::now(),
            reader,
        };
        let root_uri = Url::from_directory_path(root).map_err(|()| LspError::StartFailed {
            server: name.to_string(),
            reason: format!("{} is not an absolute path", root.display()),
        })?;
        let params = initialize_params(&root_uri, root, server.settings.init_options.clone());
        let answer = server
            .conn
            .request("initialize", params, server.settings.request_timeout)
            .await
            .map_err(|error| LspError::StartFailed {
                server: name.to_string(),
                reason: describe(&error, "initialize", server.settings.request_timeout),
            })?;
        server.capabilities = answer
            .get("capabilities")
            .cloned()
            .and_then(|caps| serde_json::from_value(caps).ok())
            .unwrap_or_default();
        server.conn.notify("initialized", json!({})).await.map_err(|error| {
            LspError::StartFailed {
                server: name.to_string(),
                reason: describe(&error, "initialized", server.settings.request_timeout),
            }
        })?;
        Ok(server)
    }

    /// The server's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The process id, when the server runs as a process.
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// What the server said it can do.
    pub fn capabilities(&self) -> &ServerCapabilities {
        &self.capabilities
    }

    /// How long the server has run.
    pub fn uptime(&self) -> Duration {
        self.started.elapsed()
    }

    /// Why the server's connection ended, or `None` while it runs.
    pub fn exited(&self) -> Option<String> {
        self.state.exited()
    }

    /// Opens `path` with `text`, or brings the open document up to `text`, closing the
    /// least recently used document past the cap. With `save`, also tells the server the
    /// file was saved, which starts rust-analyzer's check.
    ///
    /// Returns the document version the server now has and the event number before the
    /// change was sent: anything with a larger number arrived after it.
    pub(crate) async fn sync(
        &self,
        path: &Path,
        text: &str,
        save: bool,
    ) -> Result<(Url, i32, u64), LspError> {
        let uri = file_uri(path)?;
        let hash = digest(text);
        let mut docs = self.docs.lock().await;
        docs.tick += 1;
        let tick = docs.tick;
        let before = self.state.seq();
        let version = match docs.open.get_mut(&uri) {
            Some(doc) if doc.digest == hash && !save => {
                doc.used = tick;
                return Ok((uri, doc.version, before));
            }
            Some(doc) => {
                doc.used = tick;
                doc.digest = hash;
                docs.next_version += 1;
                let version = docs.next_version;
                if let Some(doc) = docs.open.get_mut(&uri) {
                    doc.version = version;
                }
                self.notify(
                    "textDocument/didChange",
                    json!({
                        "textDocument": { "uri": uri, "version": version },
                        "contentChanges": [{ "text": text }],
                    }),
                )
                .await?;
                version
            }
            None => {
                while docs.open.len() >= self.settings.max_open_documents.max(1) {
                    let Some(oldest) =
                        docs.open.iter().min_by_key(|(_, doc)| doc.used).map(|(uri, _)| uri.clone())
                    else {
                        break;
                    };
                    docs.open.remove(&oldest);
                    self.notify(
                        "textDocument/didClose",
                        json!({ "textDocument": { "uri": oldest } }),
                    )
                    .await?;
                }
                docs.next_version += 1;
                let version = docs.next_version;
                docs.open.insert(uri.clone(), Doc { version, digest: hash, used: tick });
                self.notify(
                    "textDocument/didOpen",
                    json!({
                        "textDocument": {
                            "uri": uri,
                            "languageId": language_id(path),
                            "version": version,
                            "text": text,
                        }
                    }),
                )
                .await?;
                version
            }
        };
        if save && self.wants_save() {
            self.notify("textDocument/didSave", json!({ "textDocument": { "uri": uri } })).await?;
        }
        Ok((uri, version, before))
    }

    /// Whether the server asked for `didSave`.
    fn wants_save(&self) -> bool {
        match &self.capabilities.text_document_sync {
            Some(TextDocumentSyncCapability::Options(options)) => match &options.save {
                Some(TextDocumentSyncSaveOptions::Supported(supported)) => *supported,
                Some(TextDocumentSyncSaveOptions::SaveOptions(_)) => true,
                None => false,
            },
            Some(TextDocumentSyncCapability::Kind(_)) | None => false,
        }
    }

    /// The number of open documents.
    pub(crate) async fn open_documents(&self) -> usize {
        self.docs.lock().await.open.len()
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), LspError> {
        self.conn.notify(method, params).await.map_err(|error| self.error(error, method))
    }

    fn error(&self, error: CallError, method: &str) -> LspError {
        match error {
            CallError::Rpc(RpcError { code, message }) => {
                LspError::Rpc { server: self.name.clone(), method: method.to_string(), code, message }
            }
            CallError::Timeout => LspError::Timeout {
                server: self.name.clone(),
                method: method.to_string(),
                secs: self.settings.request_timeout.as_secs(),
            },
            CallError::Closed(reason) => {
                LspError::Exited { server: self.name.clone(), reason: self.exited().unwrap_or(reason) }
            }
        }
    }

    /// Sends a request and waits for its answer within the request timeout, retrying
    /// while the server answers `ContentModified` or `ServerCancelled`.
    ///
    /// # Errors
    ///
    /// [`LspError::Timeout`] after the request timeout, [`LspError::Rpc`] for another
    /// error answer, [`LspError::Exited`] when the server is gone.
    pub async fn request<R>(&self, params: R::Params) -> Result<R::Result, LspError>
    where
        R: lsp_types::request::Request,
    {
        let deadline = Instant::now() + self.settings.request_timeout;
        let params = serde_json::to_value(params).map_err(|error| LspError::Rpc {
            server: self.name.clone(),
            method: R::METHOD.to_string(),
            code: 0,
            message: format!("the request could not be encoded: {error}"),
        })?;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(self.error(CallError::Timeout, R::METHOD));
            }
            match self.conn.request(R::METHOD, params.clone(), remaining).await {
                Ok(value) => {
                    return serde_json::from_value(value).map_err(|error| LspError::Rpc {
                        server: self.name.clone(),
                        method: R::METHOD.to_string(),
                        code: 0,
                        message: format!("the answer did not decode: {error}"),
                    });
                }
                Err(CallError::Rpc(error))
                    if error.code == CONTENT_MODIFIED || error.code == SERVER_CANCELLED =>
                {
                    tokio::time::sleep(Duration::from_millis(200).min(remaining)).await;
                }
                Err(error) => return Err(self.error(error, R::METHOD)),
            }
        }
    }

    /// Waits until the server has finished loading the workspace (rust-analyzer's
    /// quiescence), at most until `deadline`, so a navigation request does not see a
    /// half-loaded project. Servers that do not report it are ready at once.
    pub(crate) async fn wait_ready(&self, deadline: Instant) {
        let mut rx = self.state.tx.subscribe();
        loop {
            {
                let inner = self.state.inner.lock().unwrap_or_else(|e| e.into_inner());
                if inner.quiescent != Some(false) || inner.exited.is_some() {
                    return;
                }
            }
            if tokio::time::timeout_at(deadline.into(), rx.changed()).await.is_err() {
                return;
            }
        }
    }

    /// Waits until the diagnostics of `uri` are fresh for `version`, the document version
    /// the edit sent at event `edit_seq`, at most until `deadline` (spec 3.1 §7.28
    /// "Freshness"):
    ///
    /// - fresh: published with a version at least `version`, or the server went idle after
    ///   the edit (it reported its check finished);
    /// - settled: the server is idle, and either reported a check after the edit or stayed
    ///   quiet for the settle time.
    ///
    /// Returns the state with the file's error diagnostics, uncapped.
    pub(crate) async fn wait_fresh(
        &self,
        uri: &Url,
        version: i32,
        edit_seq: u64,
        edited_at: Instant,
        deadline: Instant,
    ) -> (DiagnosticsState, Vec<Diagnostic>) {
        let mut rx = self.state.tx.subscribe();
        let settle_at = edited_at + self.settings.settle;
        loop {
            let now = Instant::now();
            let (fresh, settled, errors, exited, versioned) = {
                let inner = self.state.inner.lock().unwrap_or_else(|e| e.into_inner());
                let published = inner.published.get(uri);
                let busy = inner.busy();
                let check_finished = !busy && inner.idle_since > edit_seq;
                let versioned_fresh = published
                    .is_some_and(|p| p.version.is_some_and(|published| published >= version));
                let fresh = versioned_fresh || check_finished;
                let settled = !busy && (check_finished || now >= settle_at);
                let errors = published.map(|p| errors_only(&p.diagnostics)).unwrap_or_default();
                (fresh, settled, errors, inner.exited.clone(), inner.versioned)
            };
            if let Some(reason) = exited {
                return (DiagnosticsState::Unavailable(format!("the server exited: {reason}")), Vec::new());
            }
            if fresh && settled {
                return (DiagnosticsState::Fresh, errors);
            }
            // A server that has never versioned a publish and is quiet after the settle
            // time will not make this edit's diagnostics fresh by waiting longer.
            let hopeless = !fresh && !versioned && settled;
            if now >= deadline || hopeless {
                return if fresh && !errors.is_empty() {
                    (DiagnosticsState::Checking, errors)
                } else {
                    (DiagnosticsState::Pending, Vec::new())
                };
            }
            let wake = if now < settle_at { settle_at.min(deadline) } else { deadline };
            let _ = tokio::time::timeout_at(wake.into(), rx.changed()).await;
        }
    }

    /// Error diagnostics of files other than `except` that the server published after
    /// event `edit_seq` and that are fresh by the same rule, files with errors only.
    pub(crate) fn fresh_others(&self, edit_seq: u64, except: &[Url]) -> Vec<(Url, Vec<Diagnostic>)> {
        let inner = self.state.inner.lock().unwrap_or_else(|e| e.into_inner());
        let check_finished = !inner.busy() && inner.idle_since > edit_seq;
        let mut others: Vec<(u64, Url, Vec<Diagnostic>)> = inner
            .published
            .iter()
            .filter(|(uri, published)| {
                !except.contains(uri)
                    && published.seq > edit_seq
                    && (check_finished || published.version.is_some())
            })
            .map(|(uri, published)| (published.seq, uri.clone(), errors_only(&published.diagnostics)))
            .filter(|(_, _, errors)| !errors.is_empty())
            .collect();
        others.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.as_str().cmp(b.1.as_str())));
        others.into_iter().map(|(_, uri, errors)| (uri, errors)).collect()
    }

    /// Asks the server to shut down, then ends its process group.
    pub(crate) async fn shutdown(&self) {
        if self.exited().is_none() {
            let _ = self.conn.request("shutdown", Value::Null, Duration::from_secs(2)).await;
            let _ = self.conn.notify("exit", Value::Null).await;
        }
        self.conn.close("the server was shut down");
        let child = self.child.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(mut child) = child {
            if tokio::time::timeout(Duration::from_secs(1), child.wait()).await.is_err() {
                kill_group(self.pid);
                let _ = child.kill().await;
            }
            // The server's children (cargo, proc-macro servers) are in its group.
            kill_group(self.pid);
        }
        self.reader.abort();
    }
}

impl Drop for LanguageServer {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.get_mut().unwrap_or_else(|e| e.into_inner()).take() {
            kill_group(self.pid);
            let _ = child.start_kill();
        }
        self.reader.abort();
    }
}

/// Ends the process group `pid` leads, which the pool starts every server in.
fn kill_group(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid.and_then(|pid| i32::try_from(pid).ok())
        && pid > 0
    {
        // SAFETY: killpg only sends a signal; a group that no longer exists is ESRCH.
        unsafe {
            libc::killpg(pid, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

fn errors_only(diagnostics: &[Diagnostic]) -> Vec<Diagnostic> {
    diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::ERROR))
        .cloned()
        .collect()
}

/// The `file://` URI of `path`.
pub(crate) fn file_uri(path: &Path) -> Result<Url, LspError> {
    Url::from_file_path(path)
        .map_err(|()| LspError::Path(format!("{} is not an absolute path", path.display())))
}

fn describe(error: &CallError, method: &str, timeout: Duration) -> String {
    match error {
        CallError::Rpc(RpcError { code, message }) => format!("{method} failed ({code}): {message}"),
        CallError::Timeout => format!("no answer to {method} within {}s", timeout.as_secs()),
        CallError::Closed(reason) => reason.clone(),
    }
}

/// What the client tells the server it can do: read-only use, so no edits are ever applied
/// for it.
fn initialize_params(root_uri: &Url, root: &Path, init_options: Option<Value>) -> Value {
    let name = root.file_name().map_or_else(|| "workspace".to_string(), |n| n.to_string_lossy().into_owned());
    json!({
        "processId": std::process::id(),
        "clientInfo": { "name": "adk-devtools", "version": env!("CARGO_PKG_VERSION") },
        "rootUri": root_uri,
        "rootPath": root.display().to_string(),
        "workspaceFolders": [{ "uri": root_uri, "name": name }],
        "initializationOptions": init_options,
        "capabilities": {
            "general": { "positionEncodings": ["utf-16"] },
            "window": { "workDoneProgress": true },
            "workspace": {
                "workspaceFolders": true,
                "symbol": { "dynamicRegistration": false },
                "workspaceEdit": {
                    "documentChanges": true,
                    "resourceOperations": ["create", "rename", "delete"],
                },
            },
            "textDocument": {
                "synchronization": { "didSave": true, "dynamicRegistration": false },
                "publishDiagnostics": { "versionSupport": true, "relatedInformation": false },
                "hover": { "contentFormat": ["markdown", "plaintext"] },
                "definition": { "linkSupport": false },
                "implementation": { "linkSupport": false },
                "references": {},
                "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
                "rename": { "prepareSupport": false },
            },
            "experimental": { "serverStatusNotification": true },
        },
    })
}

/// Reads the server's messages until its output ends: answers go to their requests,
/// diagnostics and progress to the state, and the server's own requests are answered.
async fn read_loop(
    name: String,
    reader: impl AsyncRead + Send + Unpin,
    conn: Arc<Connection>,
    state: Arc<ServerState>,
    settings: Option<Value>,
) {
    let mut reader = tokio::io::BufReader::new(reader);
    let reason = loop {
        let message = match read_message(&mut reader).await {
            Ok(Some(message)) => message,
            Ok(None) => break "it closed its output".to_string(),
            Err(error) => break format!("it sent an unreadable message: {error}"),
        };
        let method = message.get("method").and_then(Value::as_str).map(str::to_string);
        match (method, message.get("id").cloned()) {
            (None, Some(id)) => {
                let Some(id) = id.as_i64() else { continue };
                let result = match message.get("error") {
                    Some(error) => Err(RpcError {
                        code: error.get("code").and_then(Value::as_i64).unwrap_or_default(),
                        message: error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    }),
                    None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                };
                conn.complete(id, result);
            }
            (Some(method), Some(id)) => {
                let answer = answer_request(&method, message.get("params"), settings.as_ref());
                conn.respond(id, answer).await;
            }
            (Some(method), None) => notification(&name, &method, message.get("params"), &state),
            (None, None) => {}
        }
    };
    tracing::debug!(lsp.server = %name, reason = %reason, "language server connection ended");
    conn.close(&reason);
    state.update(|inner| {
        inner.exited.get_or_insert(reason);
        inner.active.clear();
    });
}

/// The answer to a request the server makes of the client.
fn answer_request(
    method: &str,
    params: Option<&Value>,
    settings: Option<&Value>,
) -> Result<Value, RpcError> {
    match method {
        "window/workDoneProgress/create"
        | "client/registerCapability"
        | "client/unregisterCapability"
        | "workspace/diagnostic/refresh"
        | "workspace/semanticTokens/refresh"
        | "workspace/inlayHint/refresh"
        | "workspace/codeLens/refresh"
        | "window/showMessageRequest" => Ok(Value::Null),
        // Each item's section of the initialization options, rust-analyzer's included.
        "workspace/configuration" => {
            let items = params
                .and_then(|params| params.get("items"))
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            Ok(Value::Array(vec![settings.cloned().unwrap_or(Value::Null); items]))
        }
        "workspace/workspaceFolders" => Ok(Value::Null),
        // Language servers here never write: `lsp_rename` returns the edit as text.
        "workspace/applyEdit" => Ok(json!({
            "applied": false,
            "failureReason": "this client applies no edits",
        })),
        _ => Err(RpcError { code: METHOD_NOT_FOUND, message: format!("{method} is not supported") }),
    }
}

fn token(token: &NumberOrString) -> String {
    match token {
        NumberOrString::Number(number) => number.to_string(),
        NumberOrString::String(text) => text.clone(),
    }
}

fn notification(name: &str, method: &str, params: Option<&Value>, state: &ServerState) {
    let params = params.cloned().unwrap_or(Value::Null);
    match method {
        "textDocument/publishDiagnostics" => {
            let Ok(published) = serde_json::from_value::<PublishDiagnosticsParams>(params) else {
                return;
            };
            state.update(|inner| {
                let seq = inner.seq;
                inner.versioned |= published.version.is_some();
                inner.published.insert(
                    published.uri,
                    Published { version: published.version, seq, diagnostics: published.diagnostics },
                );
            });
        }
        "$/progress" => {
            let Ok(progress) = serde_json::from_value::<ProgressParams>(params) else { return };
            let key = token(&progress.token);
            let ProgressParamsValue::WorkDone(work) = progress.value;
            match work {
                WorkDoneProgress::Begin(_) => state.update(|inner| {
                    let seq = inner.seq;
                    inner.active.insert(key, seq);
                }),
                WorkDoneProgress::End(_) => state.update(|inner| {
                    inner.active.remove(&key);
                }),
                WorkDoneProgress::Report(_) => {}
            }
        }
        "experimental/serverStatus" => {
            let quiescent = params.get("quiescent").and_then(Value::as_bool);
            if quiescent.is_some() {
                state.update(|inner| inner.quiescent = quiescent);
            }
        }
        "window/logMessage" | "window/showMessage" => {
            let text = params.get("message").and_then(Value::as_str).unwrap_or_default();
            tracing::debug!(lsp.server = %name, message = %text, "language server message");
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::rpc::frame;
    use tokio::io::{AsyncWriteExt, BufReader, DuplexStream};

    /// A server on the other end of a duplex pipe, scripted by the test.
    struct Peer {
        reader: BufReader<tokio::io::ReadHalf<DuplexStream>>,
        writer: tokio::io::WriteHalf<DuplexStream>,
    }

    impl Peer {
        async fn recv(&mut self) -> Value {
            read_message(&mut self.reader).await.unwrap().unwrap()
        }
        async fn send(&mut self, message: Value) {
            self.writer.write_all(&frame(&message)).await.unwrap();
        }
        async fn publish(&mut self, uri: &Url, version: Option<i32>, errors: &[&str]) {
            let diagnostics: Vec<Value> = errors
                .iter()
                .map(|message| json!({
                    "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } },
                    "severity": 1,
                    "message": message,
                }))
                .collect();
            let mut params = json!({ "uri": uri, "diagnostics": diagnostics });
            if let Some(version) = version {
                params["version"] = json!(version);
            }
            self.send(json!({ "jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": params }))
                .await;
        }
        async fn progress(&mut self, kind: &str) {
            let value = if kind == "begin" { json!({ "kind": "begin", "title": "cargo check" }) } else { json!({ "kind": kind }) };
            self.send(json!({ "jsonrpc": "2.0", "method": "$/progress", "params": { "token": "flycheck", "value": value } }))
                .await;
        }
    }

    async fn connected(capabilities: Value, settle: Duration) -> (LanguageServer, Peer, std::path::PathBuf) {
        let (client, server) = tokio::io::duplex(1 << 20);
        let (client_read, client_write) = tokio::io::split(client);
        let (server_read, server_write) = tokio::io::split(server);
        let mut peer = Peer { reader: BufReader::new(server_read), writer: server_write };
        let root = std::env::temp_dir().join("adk-devtools-lsp-unit");
        let settings = ServerSettings {
            request_timeout: Duration::from_secs(5),
            max_open_documents: 2,
            settle,
            init_options: Some(json!({ "check": true })),
        };
        let root_for_task = root.clone();
        let connecting = tokio::spawn(async move {
            LanguageServer::connect("fake", client_read, client_write, &root_for_task, settings, None).await
        });
        let init = peer.recv().await;
        assert_eq!(init["method"], "initialize");
        assert_eq!(init["params"]["initializationOptions"], json!({ "check": true }));
        peer.send(json!({ "jsonrpc": "2.0", "id": init["id"], "result": { "capabilities": capabilities } })).await;
        assert_eq!(peer.recv().await["method"], "initialized");
        (connecting.await.unwrap().unwrap(), peer, root)
    }

    #[tokio::test]
    async fn a_versioned_publish_for_the_edit_is_fresh_and_an_older_one_is_not() {
        let (server, mut peer, root) = connected(json!({ "textDocumentSync": 1 }), Duration::from_millis(50)).await;
        let path = root.join("a.rs");
        let (uri, v1, _) = server.sync(&path, "fn a() {}", true).await.unwrap();
        assert_eq!(peer.recv().await["method"], "textDocument/didOpen");
        let (_, v2, seq) = server.sync(&path, "fn a() { x }", true).await.unwrap();
        assert!(v2 > v1);
        assert_eq!(peer.recv().await["params"]["textDocument"]["version"], v2);
        // An old publish arrives after the edit: not fresh.
        peer.publish(&uri, Some(v1), &["old"]).await;
        let deadline = Instant::now() + Duration::from_millis(300);
        let (state, errors) = server.wait_fresh(&uri, v2, seq, Instant::now(), deadline).await;
        assert_eq!((state, errors.len()), (DiagnosticsState::Pending, 0));
        peer.publish(&uri, Some(v2), &["cannot find value `x`"]).await;
        let deadline = Instant::now() + Duration::from_secs(2);
        let (state, errors) = server.wait_fresh(&uri, v2, seq, Instant::now(), deadline).await;
        assert_eq!(state, DiagnosticsState::Fresh);
        assert_eq!(errors[0].message, "cannot find value `x`");
    }

    #[tokio::test]
    async fn a_check_that_finishes_after_the_edit_makes_the_current_set_fresh() {
        let caps = json!({ "textDocumentSync": { "openClose": true, "change": 2, "save": {} } });
        let (server, mut peer, root) = connected(caps, Duration::from_secs(10)).await;
        let path = root.join("b.rs");
        let (uri, version, seq) = server.sync(&path, "fn b() {}", true).await.unwrap();
        assert_eq!(peer.recv().await["method"], "textDocument/didOpen");
        assert_eq!(peer.recv().await["method"], "textDocument/didSave", "a save starts the check");
        let waiting = {
            let uri = uri.clone();
            let deadline = Instant::now() + Duration::from_secs(3);
            async move { server.wait_fresh(&uri, version, seq, Instant::now(), deadline).await }
        };
        let script = async {
            peer.progress("begin").await;
            // Unversioned diagnostics from the check, then the check ends.
            peer.publish(&uri, None, &["mismatched types"]).await;
            peer.progress("end").await;
        };
        let (outcome, ()) = tokio::join!(waiting, script);
        assert_eq!(outcome.0, DiagnosticsState::Fresh);
        assert_eq!(outcome.1[0].message, "mismatched types");
    }

    #[tokio::test]
    async fn an_unversioned_server_without_progress_stays_pending() {
        let (server, mut peer, root) = connected(json!({ "textDocumentSync": 2 }), Duration::from_millis(50)).await;
        let path = root.join("c.ts");
        let (uri, version, seq) = server.sync(&path, "let c = 1", true).await.unwrap();
        let open = peer.recv().await;
        assert_eq!(open["params"]["textDocument"]["languageId"], "typescript");
        peer.publish(&uri, None, &["Cannot find name"]).await;
        let started = Instant::now();
        let deadline = started + Duration::from_secs(3);
        let (state, errors) = server.wait_fresh(&uri, version, seq, started, deadline).await;
        assert_eq!((state, errors.len()), (DiagnosticsState::Pending, 0));
        assert!(started.elapsed() < Duration::from_secs(2), "a hopeless wait ends at the settle time");
    }

    #[tokio::test]
    async fn the_least_recently_used_document_is_closed_past_the_cap() {
        let (server, mut peer, root) = connected(json!({ "textDocumentSync": 1 }), Duration::from_millis(50)).await;
        for name in ["1.rs", "2.rs"] {
            server.sync(&root.join(name), name, false).await.unwrap();
            assert_eq!(peer.recv().await["method"], "textDocument/didOpen");
        }
        // Touch 1.rs so 2.rs is the least recently used.
        server.sync(&root.join("1.rs"), "1.rs", false).await.unwrap();
        server.sync(&root.join("3.rs"), "3.rs", false).await.unwrap();
        let close = peer.recv().await;
        assert_eq!(close["method"], "textDocument/didClose");
        assert!(close["params"]["textDocument"]["uri"].as_str().unwrap().ends_with("2.rs"));
        assert_eq!(peer.recv().await["method"], "textDocument/didOpen");
        assert_eq!(server.open_documents().await, 2);
    }

    #[tokio::test]
    async fn server_requests_are_answered_and_edits_refused() {
        let (server, mut peer, _root) = connected(json!({}), Duration::from_millis(50)).await;
        peer.send(json!({ "jsonrpc": "2.0", "id": "a", "method": "workspace/applyEdit", "params": {} })).await;
        let answer = peer.recv().await;
        assert_eq!(answer["id"], "a");
        assert_eq!(answer["result"]["applied"], false);
        peer.send(json!({ "jsonrpc": "2.0", "id": 7, "method": "workspace/configuration", "params": { "items": [{}, {}] } })).await;
        assert_eq!(peer.recv().await["result"], json!([{ "check": true }, { "check": true }]));
        peer.send(json!({ "jsonrpc": "2.0", "id": 8, "method": "x/unknown" })).await;
        assert_eq!(peer.recv().await["error"]["code"], METHOD_NOT_FOUND);
        drop(peer);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(server.exited().is_some(), "the end of the stream is the end of the server");
    }
}
