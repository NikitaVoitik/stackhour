//! `stackhour init server|agent`.
//!
//! ALL validations run BEFORE any write (validPort, host, machine name,
//! token, publicUrl via valid_url, --project-root realpath with the ORIGINAL
//! arg quoted in errors). init_server writes the `tokens` map — never the
//! legacy `token` key — and an `agent` section only-if-absent; the db path
//! comes from resolve_storage_paths honoring a forced STACKHOUR_CONFIG.
//! init_agent: --enrollment XOR explicit flags with the machine!==hostname
//! loophole quirk, STACKHOUR_TOKEN env fallback. Exact runInit stdout
//! including --install chaining and the 'Next:' hint lines. Secrets are
//! never printed.

use serde_json::Value;
use stackhour_core::Result;
use std::path::PathBuf;

/// Raw options for `init server` (values as they arrived from the CLI —
/// port stays a string until validPort, exactly like the JS).
#[derive(Debug, Clone, Default)]
pub struct InitServerOpts {
    pub config_path: PathBuf,
    pub force: bool,
    /// Default '0.0.0.0'.
    pub host: Option<String>,
    /// Default 4040; validated via validPort.
    pub port: Option<String>,
    /// Default `http://127.0.0.1:<port>`.
    pub public_url: Option<String>,
    /// Default os hostname.
    pub machine: Option<String>,
    /// None -> a fresh generate_token().
    pub token: Option<String>,
    pub project_roots: Vec<String>,
}

/// Raw options for `init agent`.
#[derive(Debug, Clone, Default)]
pub struct InitAgentOpts {
    pub config_path: PathBuf,
    pub force: bool,
    pub server_url: Option<String>,
    /// `--token` or the STACKHOUR_TOKEN env fallback (None with enrollment).
    pub token: Option<String>,
    /// Default os hostname (the machine!==hostname loophole check).
    pub machine: Option<String>,
    pub project_roots: Vec<String>,
    pub enrollment: Option<String>,
}

/// What an init produced (feeds the exact stdout lines; secrets stay out of
/// Debug output paths).
#[derive(Debug, Clone)]
pub struct InitResult {
    pub config_path: PathBuf,
    /// The written `server` section (init server only).
    pub server: Option<Value>,
    /// The written/preserved `agent` section.
    pub agent: Option<Value>,
}

/// The `stackhour init <server|agent> [options]` CLI.
pub fn run_init(args: &[String]) -> Result<()> {
    let _ = args;
    todo!()
}

/// Library form of init server (JS `initServer`).
pub fn init_server(opts: InitServerOpts) -> Result<InitResult> {
    let _ = opts;
    todo!()
}

/// Library form of init agent (JS `initAgent`).
pub fn init_agent(opts: InitAgentOpts) -> Result<InitResult> {
    let _ = opts;
    todo!()
}
