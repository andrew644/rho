use rho::agent::Agent;
use rho::config::Config;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::load()?;
    let cwd = std::env::current_dir()?;
    let (event_tx, event_rx) = tokio::sync::broadcast::channel(512);
    let (agent_tx, cancel) = Agent::spawn(config.clone(), cwd, event_tx.clone());
    if config.discord.enabled {
        rho::discord::spawn(config.clone(), agent_tx.clone(), event_tx.clone());
    }
    rho::tui::run(agent_tx, cancel, event_rx).await
}
