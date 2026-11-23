use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;

use rho::agent::{Agent, AgentCommand, AgentEvent};
use rho::config::Config;
use rho::session;
use rho::templates::{self, template_dirs};
use rho::tools::MAX_BYTES;
use rho::tui::TuiOptions;

const USAGE: &str = "\
rho — a Rust TUI coding agent

usage: rho [options] [@file...] [prompt...]

options:
  -p, --print          run the prompt once, print the final answer, then exit
  -c, --continue       resume the most recent session for this directory
      --resume <path>  resume a specific session file
      --model <id>     override the configured model id
  -h, --help           show this help
  -V, --version        show version

with a prompt and redirected stdin/stdout (or --print), rho runs once and
exits; otherwise it opens the interactive TUI. @file arguments are expanded
into the prompt. Piped stdin is prepended to the prompt in print mode.";

#[derive(Debug, Default, PartialEq)]
pub struct Cli {
    pub print: bool,
    pub continue_session: bool,
    pub resume: Option<PathBuf>,
    pub model: Option<String>,
    pub help: bool,
    pub version: bool,
    pub prompts: Vec<String>,
}

pub fn parse_args(args: &[String]) -> Result<Cli, String> {
    let mut cli = Cli::default();
    let mut it = args.iter().peekable();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-p" | "--print" => cli.print = true,
            "-c" | "--continue" => cli.continue_session = true,
            "--model" => {
                let value = it.next().ok_or("--model requires a value")?;
                cli.model = Some(value.clone());
            }
            "--resume" => {
                let value = it.next().ok_or("--resume requires a value")?;
                cli.resume = Some(PathBuf::from(value));
            }
            "-h" | "--help" => cli.help = true,
            "-V" | "--version" => cli.version = true,
            "--" => {
                for rest in it.by_ref() {
                    cli.prompts.push(rest.clone());
                }
                break;
            }
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(format!("unknown option: {other}"));
            }
            other => cli.prompts.push(other.to_string()),
        }
    }
    Ok(cli)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cli = match parse_args(&args) {
        Ok(cli) => cli,
        Err(e) => {
            eprintln!("{e}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    if cli.help {
        println!("{USAGE}");
        return Ok(());
    }
    if cli.version {
        println!("rho {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let mut config = Config::load()?;
    if let Some(model) = &cli.model {
        config.model.id = model.clone();
        config.model.models.retain(|m| m != model);
        config.model.models.insert(0, config.model.id.clone());
    }
    let pwd = std::env::current_dir()?;

    let mut prompt = cli.prompts.join(" ");
    if prompt.is_empty() && !std::io::stdin().is_terminal() {
        let mut piped = String::new();
        std::io::stdin().read_to_string(&mut piped).unwrap_or(0);
        prompt = piped.trim().to_string();
    } else if !prompt.is_empty() && !std::io::stdin().is_terminal() {
        let mut piped = String::new();
        std::io::stdin().read_to_string(&mut piped).unwrap_or(0);
        let piped = piped.trim();
        if !piped.is_empty() {
            prompt = format!("{piped}\n\n{prompt}");
        }
    }
    let prompt = templates::expand_at_references(&pwd, &prompt, MAX_BYTES);

    let resumed = cli.resume.clone().or_else(|| {
        cli.continue_session
            .then(|| session::latest_session(&pwd))
            .flatten()
            .map(|s| s.path)
    });

    let print_mode = cli.print
        || (!prompt.is_empty()
            && (!std::io::stdout().is_terminal() || !std::io::stdin().is_terminal()));

    let (event_tx, event_rx) = tokio::sync::broadcast::channel(512);
    let (agent_tx, cancel) = Agent::spawn_resumed(
        config.clone(),
        pwd.clone(),
        event_tx.clone(),
        resumed.clone(),
    );
    let bridge = if config.discord.enabled {
        Some(rho::discord::spawn(
            config.clone(),
            agent_tx.clone(),
            event_tx.clone(),
            print_mode,
        ))
    } else {
        None
    };

    if print_mode {
        return run_print(agent_tx, event_rx.resubscribe(), prompt).await;
    }

    let user_dir = Config::config_path()
        .parent()
        .map(|p| p.join("prompts"))
        .unwrap_or_else(|| PathBuf::from("prompts"));
    let known = {
        let mut models = vec![config.model.id.clone()];
        models.extend(config.model.models.iter().cloned());
        models
    };
    let loaded_templates = templates::load_templates(&template_dirs(user_dir, &pwd));
    let tui = TuiOptions {
        pwd,
        model: config.model.id.clone(),
        known_models: known,
        templates: loaded_templates,
        context_window: config.context_window,
        theme: config.theme,
        session_path: resumed.as_ref().map(|p| p.display().to_string()),
        clipboard: config.clipboard,
        discord: bridge,
    };
    rho::tui::run(agent_tx, cancel, event_rx, tui).await
}

async fn run_print(
    agent_tx: tokio::sync::mpsc::UnboundedSender<AgentCommand>,
    mut event_rx: tokio::sync::broadcast::Receiver<AgentEvent>,
    prompt: String,
) -> anyhow::Result<()> {
    if prompt.is_empty() {
        eprintln!("no prompt given\n\n{USAGE}");
        std::process::exit(2);
    }
    agent_tx.send(AgentCommand::Prompt { text: prompt })?;
    let code;
    loop {
        match event_rx.recv().await {
            Ok(AgentEvent::AssistantDone { text }) => {
                println!("{text}");
                let _ = std::io::stdout().flush();
                code = 0;
                break;
            }
            Ok(AgentEvent::Error(e)) => {
                eprintln!("error: {e}");
                code = 1;
                break;
            }
            Ok(AgentEvent::Idle) => {
                eprintln!("error: agent finished without an answer");
                code = 1;
                break;
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                eprintln!("error: agent shut down");
                code = 1;
                break;
            }
        }
    }
    drop(agent_tx);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flags_and_prompts() {
        let cli = parse_args(&[
            "-p".into(),
            "--model".into(),
            "m2".into(),
            "explain".into(),
            "@src/main.rs".into(),
        ])
        .unwrap();
        assert!(cli.print);
        assert_eq!(cli.model.as_deref(), Some("m2"));
        assert_eq!(
            cli.prompts,
            vec!["explain".to_string(), "@src/main.rs".to_string()]
        );
    }

    #[test]
    fn parses_session_flags() {
        let cli = parse_args(&[
            "--continue".into(),
            "--resume".into(),
            "/tmp/s.jsonl".into(),
        ])
        .unwrap();
        assert!(cli.continue_session);
        assert_eq!(cli.resume, Some(PathBuf::from("/tmp/s.jsonl")));
    }

    #[test]
    fn double_dash_passes_prompts_through() {
        let cli = parse_args(&["--".into(), "-not-an-option".into()]).unwrap();
        assert_eq!(cli.prompts, vec!["-not-an-option".to_string()]);
    }

    #[test]
    fn unknown_option_is_an_error() {
        assert!(parse_args(&["--bogus".into()]).is_err());
        assert!(parse_args(&["--model".into()]).is_err());
    }
}
