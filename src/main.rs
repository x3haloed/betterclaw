use anyhow::Result;
use betterclaw::logging;
use betterclaw::runtime::Runtime;

fn main() -> Result<()> {
    logging::init()?;

    let runtime = Runtime::new();

    tracing::info!(
        agent_count = runtime.agents().len(),
        thread_count = runtime.threads().len(),
        tool_count = runtime.tools().len(),
        channel_count = runtime.channels().len(),
        "BetterClaw runtime initialized"
    );

    Ok(())
}
