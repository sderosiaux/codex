//! `codex`: the agent loop with nothing in front of it. Commands run directly on the host with
//! the user's privileges; there is no sandbox, approval prompt, or network policy.

mod config_override;
mod prompts;
mod render;

use std::io::IsTerminal;
use std::io::Read;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use anyhow::bail;
use clap::Parser;
use clap::Subcommand;
use codex_core_api::Arg0DispatchPaths;
use codex_core_api::AuthManager;
use codex_core_api::CodexHomeUserInstructionsProvider;
use codex_core_api::CodexThread;
use codex_core_api::Config;
use codex_core_api::ConfigBuilder;
use codex_core_api::ConfigOverrides;
use codex_core_api::DEFAULT_EXEC_SERVER_LISTEN_URL;
use codex_core_api::DynamicToolResponse;
use codex_core_api::EnvironmentManager;
use codex_core_api::Event;
use codex_core_api::EventMsg;
use codex_core_api::ExecutorSkillProvider;
use codex_core_api::ExtensionRegistryBuilder;
use codex_core_api::HostSkillProvider;
use codex_core_api::HttpClientFactory;
use codex_core_api::NewThread;
use codex_core_api::Op;
use codex_core_api::OutboundProxyPolicy;
use codex_core_api::SessionSource;
use codex_core_api::SkillProviders;
use codex_core_api::SkillsExtensionConfig;
use codex_core_api::StartIfIdleSubmission;
use codex_core_api::StartThreadOptions;
use codex_core_api::ThreadManager;
use codex_core_api::TurnInputRequest;
use codex_core_api::UserInput;
use codex_core_api::arg0_dispatch_or_else;
use codex_core_api::build_models_manager;
use codex_core_api::install_agent_message_board;
use codex_core_api::install_image_generation_extension;
use codex_core_api::install_skills_extension;
use codex_core_api::install_web_search_extension;
use codex_core_api::resolve_installation_id;
use codex_core_api::run_exec_server;
use codex_core_api::set_default_originator;
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

use crate::prompts::Prompter;
use crate::render::HumanRenderer;

#[derive(Debug, Parser)]
#[command(
    name = "codex",
    about = "Run the Codex agent loop. Commands execute directly on this machine, unsandboxed.",
    subcommand_negates_reqs = true,
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Model to use instead of the configured default.
    #[arg(long, short = 'm', value_name = "MODEL")]
    model: Option<String>,

    /// Working directory for the agent.
    #[arg(long = "cd", short = 'C', value_name = "DIR")]
    cwd: Option<PathBuf>,

    /// Override a `config.toml` value, e.g. `-c model_reasoning_effort=high`.
    /// The value is parsed as TOML, falling back to a plain string.
    #[arg(long = "config", short = 'c', value_name = "key=value")]
    config_overrides: Vec<String>,

    /// Print every event as a JSON line on stdout instead of human-readable output.
    #[arg(long)]
    json: bool,

    /// Show reasoning summaries on stderr.
    #[arg(long)]
    reasoning: bool,

    /// Prompt for a single turn. Without one, a piped stdin is the prompt and a terminal
    /// starts an interactive session.
    #[arg(value_name = "PROMPT", num_args = 0.., trailing_var_arg = true)]
    prompt: Vec<String>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Serve this machine as a remote execution environment for another Codex.
    ExecServer {
        /// `ws://IP:PORT` to listen on; the bound URL is printed on stdout.
        #[arg(long, value_name = "URL", default_value = DEFAULT_EXEC_SERVER_LISTEN_URL)]
        listen: String,
    },
}

/// What the run needs from the user after startup.
enum Mode {
    /// One turn, then exit. Requests for input are declined.
    OneShot(String),
    /// Read turns from the terminal until EOF or `/exit`.
    Interactive,
}

fn main() -> anyhow::Result<()> {
    arg0_dispatch_or_else(run_main)
}

async fn run_main(arg0_paths: Arg0DispatchPaths) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("error")),
        )
        .with_writer(std::io::stderr)
        .init();
    if let Err(err) = set_default_originator("codex_loop".to_string()) {
        tracing::warn!("failed to set originator: {err:?}");
    }

    let cli = Cli::parse();
    if let Some(Command::ExecServer { listen }) = cli.command {
        return run_exec_server(
            &listen,
            HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
        )
        .await
        .map_err(|err| anyhow::anyhow!("exec-server failed: {err}"));
    }

    let mode = if !cli.prompt.is_empty() {
        Mode::OneShot(cli.prompt.join(" "))
    } else if std::io::stdin().is_terminal() {
        Mode::Interactive
    } else {
        let mut prompt = String::new();
        std::io::stdin()
            .read_to_string(&mut prompt)
            .context("read prompt from stdin")?;
        if prompt.trim().is_empty() {
            bail!("no prompt provided via stdin");
        }
        Mode::OneShot(prompt)
    };

    let config = load_config(&cli, arg0_paths).await?;
    let host = Host::start(&config).await?;
    let NewThread {
        thread_id, thread, ..
    } = host
        .thread_manager
        .start_thread(StartThreadOptions::new(config))
        .await
        .context("start thread")?;
    eprintln!("thread {thread_id}");

    let mut output = if cli.json {
        Output::Json
    } else {
        Output::Human(HumanRenderer::new(cli.reasoning))
    };
    let result = match mode {
        Mode::OneShot(prompt) => {
            let mut prompter = Prompter::non_interactive();
            run_turn(&thread, prompt, &mut output, &mut prompter).await
        }
        Mode::Interactive => run_interactive(&thread, &mut output).await,
    };
    if let Output::Human(renderer) = &mut output {
        renderer.finish();
    }

    let shutdown = thread.shutdown_and_wait().await;
    host.thread_manager.remove_thread(&thread_id).await;
    result?;
    shutdown.context("shut down thread")?;
    Ok(())
}

async fn load_config(cli: &Cli, arg0_paths: Arg0DispatchPaths) -> anyhow::Result<Config> {
    let cli_overrides = config_override::parse_overrides(&cli.config_overrides)?;
    let harness_overrides = ConfigOverrides {
        model: cli.model.clone(),
        cwd: cli.cwd.clone(),
        codex_self_exe: arg0_paths.codex_self_exe,
        ..Default::default()
    };
    ConfigBuilder::default()
        .cli_overrides(cli_overrides)
        .harness_overrides(harness_overrides)
        .build()
        .await
        .context("load config")
}

/// Process-wide services shared by every thread of this run, sub-agents included.
struct Host {
    thread_manager: Arc<ThreadManager>,
}

impl Host {
    async fn start(config: &Config) -> anyhow::Result<Self> {
        let auth_manager =
            AuthManager::shared_from_config(config, /*enable_codex_api_key_env*/ true).await?;
        let environment_manager = Arc::new(
            EnvironmentManager::from_codex_home(
                config.codex_home.clone(),
                config.http_client_factory(),
            )
            .await?,
        );
        let installation_id = resolve_installation_id(&config.codex_home).await?;
        let user_instructions_provider = Arc::new(CodexHomeUserInstructionsProvider::new(
            config.codex_home.clone(),
        ));
        let session_source = SessionSource::Cli;

        let thread_manager = Arc::new_cyclic(|thread_manager| {
            let mut extensions = ExtensionRegistryBuilder::<Config>::new();
            install_agent_message_board(&mut extensions, thread_manager.clone());
            install_web_search_extension(&mut extensions, Arc::clone(&auth_manager));
            install_image_generation_extension(
                &mut extensions,
                Arc::clone(&auth_manager),
                |config: &Config| Some(config.codex_home.clone()),
            );
            install_skills_extension(
                &mut extensions,
                SkillProviders::new()
                    .with_executor_provider(Arc::new(
                        ExecutorSkillProvider::new_with_restriction_product(
                            Arc::clone(&environment_manager),
                            session_source.restriction_product(),
                        ),
                    ))
                    .with_host_provider(Arc::new(HostSkillProvider::new())),
                |config: &Config| SkillsExtensionConfig {
                    include_instructions: config.include_skill_instructions,
                    max_context_tokens: config.skill_max_context_tokens,
                    bundled_skills_enabled: config.bundled_skills_enabled(),
                    shadow_selection_enabled: false,
                },
            );
            ThreadManager::new(
                config,
                Arc::clone(&auth_manager),
                build_models_manager(config, Arc::clone(&auth_manager)),
                session_source,
                environment_manager,
                Arc::new(extensions.build()),
                user_instructions_provider,
                installation_id,
                /*attestation_provider*/ None,
                /*external_time_provider*/ None,
            )
        });
        Ok(Self { thread_manager })
    }
}

enum Output {
    Json,
    Human(HumanRenderer),
}

impl Output {
    fn emit(&mut self, event: &Event) -> anyhow::Result<()> {
        match self {
            Output::Json => {
                let mut stdout = std::io::stdout().lock();
                serde_json::to_writer(&mut stdout, event).context("serialize event")?;
                stdout.write_all(b"\n").context("write event")?;
                stdout.flush().context("flush event")?;
            }
            Output::Human(renderer) => renderer.render(&event.msg),
        }
        Ok(())
    }
}

async fn run_interactive(thread: &CodexThread, output: &mut Output) -> anyhow::Result<()> {
    let mut prompter = Prompter::interactive(spawn_stdin_lines());
    loop {
        eprint!("› ");
        let line = tokio::select! {
            line = prompter.read_line() => line,
            _ = tokio::signal::ctrl_c() => None,
        };
        let Some(line) = line else {
            eprintln!();
            return Ok(());
        };
        let prompt = line.trim();
        match prompt {
            "" => continue,
            "/exit" | "/quit" => return Ok(()),
            _ => {}
        }
        if let Err(err) = run_turn(thread, prompt.to_string(), output, &mut prompter).await {
            eprintln!("{err:#}");
        }
    }
}

/// Reads terminal lines on a detached thread: a blocking read must never keep the runtime alive.
fn spawn_stdin_lines() -> mpsc::UnboundedReceiver<String> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut line = String::new();
        loop {
            line.clear();
            match stdin.read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {
                    if tx
                        .send(line.trim_end_matches(['\r', '\n']).to_string())
                        .is_err()
                    {
                        return;
                    }
                }
            }
        }
    });
    rx
}

/// Runs one user turn to completion. Ctrl-C interrupts the turn instead of the process.
async fn run_turn(
    thread: &CodexThread,
    prompt: String,
    output: &mut Output,
    prompter: &mut Prompter,
) -> anyhow::Result<()> {
    let submission = thread
        .start_turn_if_idle(TurnInputRequest::user_input(vec![UserInput::Text {
            text: prompt,
            text_elements: Vec::new(),
        }]))
        .await
        .context("submit turn")?;
    if let StartIfIdleSubmission::NotSubmitted { reason } = submission {
        bail!("turn was not started: {reason:?}");
    }

    let mut failed = None;
    loop {
        let event = tokio::select! {
            event = thread.next_event() => event.context("read event")?,
            _ = tokio::signal::ctrl_c() => {
                thread.submit(Op::Interrupt).await.context("interrupt turn")?;
                continue;
            }
        };
        output.emit(&event)?;
        match event.msg {
            EventMsg::TurnComplete(complete) => {
                return match complete.error.map(|error| error.message).or(failed) {
                    Some(message) => Err(anyhow::anyhow!(message)),
                    None => Ok(()),
                };
            }
            EventMsg::TurnAborted(aborted) => bail!("turn aborted: {:?}", aborted.reason),
            EventMsg::ShutdownComplete => bail!("thread shut down during the turn"),
            EventMsg::Error(error) => failed = Some(error.message),
            EventMsg::ElicitationRequest(request) => {
                let op = prompter.resolve_elicitation(request).await;
                thread.submit(op).await.context("answer MCP elicitation")?;
            }
            EventMsg::RequestUserInput(request) => {
                let op = prompter.answer_user_input(request).await;
                thread.submit(op).await.context("answer user input")?;
            }
            EventMsg::DynamicToolCallRequest(request) => {
                // No dynamic tools are registered by this host; answer so the turn can proceed.
                thread
                    .submit(Op::DynamicToolResponse {
                        id: request.call_id,
                        response: DynamicToolResponse {
                            content_items: Vec::new(),
                            success: false,
                        },
                    })
                    .await
                    .context("answer dynamic tool call")?;
            }
            _ => {}
        }
    }
}
