//! Language Server Protocol support for the guest agent.
//!
//! Provides semantic code intelligence (`lsp_hover`, `lsp_definition`,
//! `lsp_references`, `lsp_diagnostics`, `lsp_rename`) by managing language
//! server subprocesses inside the VM. Servers start lazily, are reused for the
//! session, recover from crashes, and shut down with the connection.
//!
//! - [`config`] loads the per-image `[lsp]` server table.
//! - [`language`] maps file extensions to a language and LSP `languageId`.
//! - [`jsonrpc`] is the `Content-Length`-framed JSON-RPC stdio client.
//! - [`manager`] owns the server processes and dispatches tool requests.

pub mod config;
pub mod jsonrpc;
pub mod language;
pub mod manager;

pub use config::{LspConfig, LspConfigError};
pub use manager::{LspManager, LspOutcome};
