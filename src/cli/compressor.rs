//! Compressor CLI commands.

use std::sync::Arc;

use clap::{Args, Subcommand};

use crate::app::{AppBuilder, AppBuilderFlags};
use crate::cli::Cli;
use crate::compressor::{MicroDistillParams, run_micro_distill_pass};
use crate::config::Config;
use crate::db::Database;

#[derive(Subcommand, Debug, Clone)]
pub enum CompressorCommand {
    /// Run a single compressor pass over a bounded ledger window.
    RunOnce(RunOnceArgs),
}

#[derive(Args, Debug, Clone)]
pub struct RunOnceArgs {
    /// User id namespace for the ledger.
    #[arg(long, default_value = "default")]
    pub user_id: String,

    /// Number of most-recent ledger events to include in the local window.
    #[arg(long, default_value_t = 200)]
    pub window_events: i64,

    /// Anchor invariants to include (kind prefix `invariant.`).
    #[arg(long, default_value_t = 30)]
    pub anchor_invariants: i64,

    /// Drift/contradiction candidates to include (kind prefix `drift.`).
    #[arg(long, default_value_t = 30)]
    pub drift_candidates: i64,

    /// Commit the resulting delta to the ledger as a derived event (`distill.micro`).
    #[arg(long)]
    pub commit: bool,
}

pub async fn run_compressor_command(cli: &Cli, cmd: CompressorCommand) -> anyhow::Result<()> {
    let cfg = Config::from_env_with_toml(cli.config.as_deref()).await?;

    let flags = AppBuilderFlags { no_db: cli.no_db };
    let log_broadcaster = Arc::new(crate::channels::web::log_layer::LogBroadcaster::new());
    let builder = AppBuilder::new(cfg, flags, cli.config.clone(), log_broadcaster);
    let components = builder.build_all().await?;

    let store: Arc<dyn Database> = components
        .db
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("No database available (use without --no-db)"))?
        .clone();

    match cmd {
        CompressorCommand::RunOnce(args) => {
            run_once(store, components.compressor_llm, args).await?;
        }
    }

    Ok(())
}

async fn run_once(
    store: Arc<dyn Database>,
    compressor_llm: Arc<dyn crate::llm::LlmProvider>,
    args: RunOnceArgs,
) -> anyhow::Result<()> {
    let params = MicroDistillParams {
        window_events: args.window_events,
        anchor_invariants: args.anchor_invariants,
        drift_candidates: args.drift_candidates,
        max_tokens: 2048,
    };

    let res = run_micro_distill_pass(
        store.as_ref(),
        compressor_llm.as_ref(),
        &args.user_id,
        params,
        args.commit,
    )
    .await?;
    let delta = res.delta;

    // Always print the delta to stdout for inspection.
    println!("{}", serde_json::to_string_pretty(&delta)?);
    eprintln!("\n--- wake_pack.v0 (content) ---\n{}\n", delta.wake_pack.content);
    if args.commit {
        if let Some(id) = res.wake_pack_event_id {
            eprintln!("Committed wake_pack.v0 event: {}", id);
        }
        if let Some(id) = res.distill_event_id {
            eprintln!("Committed distill.micro event: {}", id);
        }
    }

    Ok(())
}
