//! CLI command handling.
//!
//! Provides subcommands for:
//! - Running the agent (`run`)
//! - Interactive onboarding wizard (`onboard`)
//! - Managing configuration (`config list`, `config get`, `config set`)
//! - Managing WASM tools (`tool install`, `tool list`, `tool remove`)
//! - Managing MCP servers (`mcp add`, `mcp auth`, `mcp list`, `mcp test`)
//! - Managing OS service (`service install`, `service start`, `service stop`)
//! - Active health diagnostics (`doctor`)
//! - Checking system health (`status`)

mod completion;
mod compressor;
mod config;
mod doctor;
mod mcp;
pub mod oauth_defaults;
mod pairing;
mod registry;
mod service;
pub mod status;
mod tool;

pub use completion::Completion;
pub use compressor::{CompressorCommand, run_compressor_command};
pub use config::{ConfigCommand, run_config_command};
pub use doctor::run_doctor_command;
pub use mcp::{McpCommand, run_mcp_command};
pub use pairing::{PairingCommand, run_pairing_command, run_pairing_command_with_store};
pub use registry::{RegistryCommand, run_registry_command};
pub use service::{ServiceCommand, run_service_command};
pub use status::run_status_command;
pub use tool::{ToolCommand, run_tool_command};

use clap::{ColorChoice, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "betterclaw")]
#[command(
    about = "Secure personal AI assistant that protects your data and expands its capabilities"
)]
#[command(
    long_about = "BetterClaw is a secure AI assistant. Use 'betterclaw <subcommand> --help' for details.\nExamples:\n  betterclaw run  # Start the agent\n  betterclaw config list  # List configs"
)]
#[command(version)]
#[command(color = ColorChoice::Auto)] // Enable auto-color for help (if the terminal supports it)
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Run in interactive CLI mode only (disable other channels)
    #[arg(long, global = true)]
    pub cli_only: bool,

    /// Skip database connection (for testing)
    #[arg(long, global = true)]
    pub no_db: bool,

    /// Single message mode - send one message and exit
    #[arg(short, long, global = true)]
    pub message: Option<String>,

    /// Configuration file path (optional, uses env vars by default)
    #[arg(short, long, global = true)]
    pub config: Option<std::path::PathBuf>,

    /// Skip first-run onboarding check
    #[arg(long, global = true)]
    pub no_onboard: bool,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Run the agent (default if no subcommand given)
    #[command(
        about = "Run the AI agent",
        long_about = "Starts the BetterClaw agent in default mode.\nExample: betterclaw run"
    )]
    Run,

    /// Interactive onboarding wizard
    #[command(
        about = "Run interactive setup wizard",
        long_about = "Guides through initial configuration.\nExamples:\n  betterclaw onboard --skip-auth  # Skip auth step\n  betterclaw onboard --channels-only  # Reconfigure channels"
    )]
    Onboard {
        /// Skip authentication (use existing session)
        #[arg(long)]
        skip_auth: bool,

        /// Reconfigure channels only
        #[arg(long)]
        channels_only: bool,
    },

    /// Manage configuration settings
    #[command(
        subcommand,
        about = "Manage app configs",
        long_about = "Commands for listing, getting, and setting configurations.\nExample: betterclaw config list"
    )]
    Config(ConfigCommand),

    /// Manage WASM tools
    #[command(
        subcommand,
        about = "Manage WASM tools",
        long_about = "Install, list, or remove WASM-based tools.\nExample: betterclaw tool install mytool.wasm"
    )]
    Tool(ToolCommand),

    /// Browse and install extensions from the registry
    #[command(
        subcommand,
        about = "Browse/install extensions",
        long_about = "Interact with extension registry.\nExample: betterclaw registry list"
    )]
    Registry(RegistryCommand),

    /// Manage MCP servers (hosted tool providers)
    #[command(
        subcommand,
        about = "Manage MCP servers",
        long_about = "Add, auth, list, or test MCP servers.\nExample: betterclaw mcp add notion https://mcp.notion.com"
    )]
    Mcp(McpCommand),

    /// DM pairing (approve inbound requests from unknown senders)
    #[command(
        subcommand,
        about = "Manage DM pairing",
        long_about = "Approve or manage pairing requests.\nExamples:\n  betterclaw pairing list telegram\n  betterclaw pairing approve telegram ABC12345"
    )]
    Pairing(PairingCommand),

    /// Manage OS service (launchd / systemd)
    #[command(
        subcommand,
        about = "Manage OS service",
        long_about = "Install, start, or stop service.\nExample: betterclaw service install"
    )]
    Service(ServiceCommand),

    /// Probe external dependencies and validate configuration
    #[command(
        about = "Run diagnostics",
        long_about = "Checks dependencies and config validity.\nExample: betterclaw doctor"
    )]
    Doctor,

    /// Show system health and diagnostics
    #[command(
        about = "Show system status",
        long_about = "Displays health and diagnostics info.\nExample: betterclaw status"
    )]
    Status,

    /// Generate shell completion scripts
    #[command(
        about = "Generate completions",
        long_about = "Generates shell completion scripts.\nExample: betterclaw completion --shell bash > betterclaw.bash"
    )]
    Completion(Completion),

    /// Compressor utilities (run-once distill, debugging)
    #[command(subcommand)]
    Compressor(CompressorCommand),

    /// Run as a sandboxed worker inside a Docker container (internal use).
    /// This is invoked automatically by the orchestrator, not by users directly.
    #[command(hide = true)]
    Worker {
        /// Job ID to execute.
        #[arg(long)]
        job_id: uuid::Uuid,

        /// URL of the orchestrator's internal API.
        #[arg(long, default_value = "http://host.docker.internal:50051")]
        orchestrator_url: String,

        /// Maximum iterations before stopping.
        #[arg(long, default_value = "50")]
        max_iterations: u32,
    },

    /// Run as a Claude Code bridge inside a Docker container (internal use).
    /// Spawns the `claude` CLI and streams output back to the orchestrator.
    #[command(hide = true)]
    ClaudeBridge {
        /// Job ID to execute.
        #[arg(long)]
        job_id: uuid::Uuid,

        /// URL of the orchestrator's internal API.
        #[arg(long, default_value = "http://host.docker.internal:50051")]
        orchestrator_url: String,

        /// Maximum agentic turns for Claude Code.
        #[arg(long, default_value = "50")]
        max_turns: u32,

        /// Claude model to use (e.g. "sonnet", "opus").
        #[arg(long, default_value = "sonnet")]
        model: String,
    },
}

impl Cli {
    /// Check if we should run the agent (default behavior or explicit `run` command).
    pub fn should_run_agent(&self) -> bool {
        matches!(self.command, None | Some(Command::Run))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use insta::assert_snapshot;

    #[test]
    fn test_version() {
        let cmd = Cli::command();
        assert_eq!(
            cmd.get_version().unwrap_or("unknown"),
            env!("CARGO_PKG_VERSION")
        );
    }

    #[test]
    fn test_help_output() {
        let mut cmd = Cli::command();
        let help = cmd.render_help().to_string();
        assert_snapshot!(help);
    }

    #[test]
    fn test_long_help_output() {
        let mut cmd = Cli::command();
        let help = cmd.render_long_help().to_string();
        assert_snapshot!(help);
    }
}
