//! Language servers over stdio JSON-RPC (spec 3.1 §7.28, §10 D11; feature `lsp`).
//!
//! [`LanguageServerPool`] keeps one server per configured language for a workspace. It
//! starts each lazily through a [`ServerLauncher`] the host supplies (the harness passes
//! its OS sandbox), restarts it with backoff, keeps at most 50 documents open per server
//! and stops every server on [`shutdown`](LanguageServerPool::shutdown).
//!
//! | Use | Method |
//! |---|---|
//! | Errors in files an edit wrote, fresh for the new content | [`LanguageServerPool::diagnostics_after_edit`] |
//! | A request (definition, references, hover, symbols, rename) | [`LanguageServerPool::ready_document`], then [`LanguageServer::request`] |
//! | Start a server before its first use | [`LanguageServerPool::warm`] |
//!
//! A diagnostic counts as fresh only when the server published it for the post-edit
//! document version, or after the server went idle following the edit (it reported its
//! check finished); otherwise the file's state is
//! [`Pending`](DiagnosticsState::Pending) and nothing is listed.

mod pool;
mod rpc;
mod server;
pub mod text;

use std::path::Path;

pub use lsp_types;
pub use pool::{
    EditDiagnostics, HostServerLauncher, LanguageServerPool, LanguageServerSpec, LaunchRequest,
    PoolConfig, PoolEvent, ProcessRegistry, RestartPolicy, SCRATCH_PLACEHOLDER, ServerLauncher,
    path_uri,
};
pub use server::{DiagnosticsState, FileDiagnostics, LanguageServer};

/// Why a language server could not answer.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LspError {
    /// No configured server serves the file's extension.
    #[error("no language server serves '{0}' files")]
    NoServer(String),

    /// The server's program is not on `PATH`.
    #[error(
        "language server '{server}' is not installed: '{command}' is not on PATH; install it, \
         or set its command under [lsp.servers.{server}]"
    )]
    NotInstalled {
        /// The server.
        server: String,
        /// The command looked for.
        command: String,
    },

    /// The server did not start or did not answer the handshake.
    #[error("language server '{server}' did not start: {reason}")]
    StartFailed {
        /// The server.
        server: String,
        /// Why.
        reason: String,
    },

    /// The server failed recently and is waiting to be started again.
    #[error("language server '{server}' failed and restarts in {secs}s: {reason}")]
    BackingOff {
        /// The server.
        server: String,
        /// Seconds until the next start.
        secs: u64,
        /// Why it failed.
        reason: String,
    },

    /// The server failed too often, or the pool was shut down.
    #[error("language server '{server}' is off: {reason}")]
    GaveUp {
        /// The server.
        server: String,
        /// Why.
        reason: String,
    },

    /// A request got no answer in time.
    #[error("language server '{server}' did not answer {method} within {secs}s")]
    Timeout {
        /// The server.
        server: String,
        /// The request.
        method: String,
        /// The timeout.
        secs: u64,
    },

    /// The server answered with an error.
    #[error("language server '{server}' answered {method} with an error ({code}): {message}")]
    Rpc {
        /// The server.
        server: String,
        /// The request.
        method: String,
        /// The JSON-RPC error code.
        code: i64,
        /// Its message.
        message: String,
    },

    /// The server's connection ended.
    #[error("language server '{server}' exited: {reason}")]
    Exited {
        /// The server.
        server: String,
        /// Why.
        reason: String,
    },

    /// The pool was shut down.
    #[error("the language servers are shut down")]
    Closed,

    /// A path the server cannot be sent.
    #[error("{0}")]
    Path(String),

    /// Reading a file failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl From<LspError> for adk_core::AdkError {
    fn from(error: LspError) -> Self {
        use adk_core::ErrorCategory::{
            Internal, InvalidInput, NotFound, Timeout, Unavailable, Unsupported,
        };
        let (category, code) = match &error {
            LspError::NoServer(_) => (Unsupported, "devtools.lsp.no_server"),
            LspError::NotInstalled { .. } => (Unavailable, "devtools.lsp.not_installed"),
            LspError::StartFailed { .. } => (Unavailable, "devtools.lsp.start_failed"),
            LspError::BackingOff { .. } => (Unavailable, "devtools.lsp.backing_off"),
            LspError::GaveUp { .. } | LspError::Closed => (Unavailable, "devtools.lsp.off"),
            LspError::Timeout { .. } => (Timeout, "devtools.lsp.timeout"),
            LspError::Rpc { .. } => (InvalidInput, "devtools.lsp.error"),
            LspError::Exited { .. } => (Unavailable, "devtools.lsp.exited"),
            LspError::Path(_) => (InvalidInput, "devtools.lsp.path"),
            LspError::Io(io) if io.kind() == std::io::ErrorKind::NotFound => {
                (NotFound, "devtools.not_found")
            }
            LspError::Io(_) => (Internal, "devtools.io"),
        };
        adk_core::AdkError::new(adk_core::ErrorComponent::Tool, category, code, error.to_string())
    }
}

/// The lowercase extension of `path`, without the dot.
pub(crate) fn extension_of(path: &Path) -> Option<String> {
    path.extension().map(|ext| ext.to_string_lossy().to_ascii_lowercase())
}

/// The LSP `languageId` of `path`, from its extension.
///
/// # Example
///
/// ```rust
/// use adk_devtools::lsp::language_id;
///
/// assert_eq!(language_id(std::path::Path::new("src/app.tsx")), "typescriptreact");
/// ```
pub fn language_id(path: &Path) -> &'static str {
    match extension_of(path).as_deref() {
        Some("rs") => "rust",
        Some("ts" | "mts" | "cts") => "typescript",
        Some("tsx") => "typescriptreact",
        Some("js" | "mjs" | "cjs") => "javascript",
        Some("jsx") => "javascriptreact",
        Some("py" | "pyi") => "python",
        Some("go") => "go",
        Some("c" | "h") => "c",
        Some("cc" | "cpp" | "cxx" | "hpp" | "hh") => "cpp",
        Some("java") => "java",
        Some("rb") => "ruby",
        Some("zig") => "zig",
        _ => "plaintext",
    }
}
