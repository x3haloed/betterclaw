//! Compressor CLI commands.

use std::sync::Arc;

use clap::{Args, Subcommand};

use crate::app::{AppBuilder, AppBuilderFlags};
use crate::cli::Cli;
use crate::compressor::complete_delta_v0;
use crate::config::Config;
use crate::db::Database;
use crate::ledger::NewLedgerEvent;
use crate::llm::ChatMessage;

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

fn format_events_for_prompt(events: &[crate::ledger::LedgerEvent]) -> String {
    let mut out = String::new();
    for e in events {
        out.push_str("- ");
        out.push_str(&format!(
            "{} {} {} {}\n",
            e.id,
            e.created_at.to_rfc3339(),
            e.kind,
            e.source
        ));
        if let Some(ref c) = e.content {
            out.push_str("  content: ");
            // Keep prompt bounded; this is not a dump.
            out.push_str(&crate::compressor::truncate_chars(c, 2_000));
            out.push('\n');
        }
    }
    out
}

async fn run_once(
    store: Arc<dyn Database>,
    compressor_llm: Arc<dyn crate::llm::LlmProvider>,
    args: RunOnceArgs,
) -> anyhow::Result<()> {
    // Local window (newest-first from DB); present oldest-first to the model.
    let mut local = store
        .list_recent_ledger_events(&args.user_id, args.window_events)
        .await?;
    local.reverse();

    let mut invariants = store
        .list_recent_ledger_events_by_kind_prefix(&args.user_id, "invariant.", args.anchor_invariants)
        .await?;
    invariants.reverse();

    let mut drift = store
        .list_recent_ledger_events_by_kind_prefix(&args.user_id, "drift.", args.drift_candidates)
        .await?;
    drift.reverse();

    // System prompt: sterile transformer role.
    let system = r#"
You are the BetterClaw compressor subsystem.
You are a transformer over evidence (ledger events). You do not have a persona.

Goal: produce a small, conservative delta of actions over invariants/isnads.

Hard rules:
- Never invent facts.
- Every action MUST include citations with valid event_id values from the provided ledger window or anchors.
- If you cannot cite evidence, do not create/update invariants; prefer flag_drift or do nothing.

Output constraints:
- Max 8 total actions.
- Max 2 create_invariant per scope.
- Prefer reweight/merge over rewriting text unless evidence is strong.
"#;

    let user = format!(
        "# Evidence Window (Local)\n{}\n\n# Anchor Invariants (Recent)\n{}\n\n# Drift/Contradiction Candidates (Recent)\n{}\n",
        format_events_for_prompt(&local),
        format_events_for_prompt(&invariants),
        format_events_for_prompt(&drift),
    );

    let messages = vec![ChatMessage::system(system.trim()), ChatMessage::user(user)];

    let delta = complete_delta_v0(compressor_llm.as_ref(), messages, None, 2048).await?;

    // Always print the delta to stdout for inspection.
    println!("{}", serde_json::to_string_pretty(&delta)?);

    if args.commit {
        let payload = serde_json::json!({
            "delta": delta,
            "window": {
                "local_event_ids": local.iter().map(|e| e.id.to_string()).collect::<Vec<_>>(),
                "anchor_invariant_ids": invariants.iter().map(|e| e.id.to_string()).collect::<Vec<_>>(),
                "drift_candidate_ids": drift.iter().map(|e| e.id.to_string()).collect::<Vec<_>>(),
            }
        });

        let ev = NewLedgerEvent {
            user_id: &args.user_id,
            episode_id: None,
            kind: "distill.micro",
            source: "compressor",
            content: None,
            payload: &payload,
        };

        let id = store.append_ledger_event(&ev).await?;
        eprintln!("Committed distill.micro event: {}", id);
    }

    Ok(())
}
