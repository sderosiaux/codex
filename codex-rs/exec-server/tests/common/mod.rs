use std::env;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;

use codex_exec_server::CODEX_ARG0_EXEC_HELPER_ARG1;
use codex_exec_server::ExecServerTelemetry;
use codex_exec_server::RequestDispatchMode;
use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
use codex_test_binary_support::TestBinaryDispatchGuard;
use codex_test_binary_support::TestBinaryDispatchMode;
use codex_test_binary_support::configure_test_binary_dispatch;
use ctor::ctor;

pub(crate) mod exec_server;

pub(crate) const TEST_BUILD_COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

pub(crate) const DELAYED_OUTPUT_AFTER_EXIT_PARENT_ARG: &str =
    "--codex-test-delayed-output-after-exit-parent";
pub(crate) const SYSTEM_PROXY_REQUEST_URL_ENV: &str =
    "CODEX_EXEC_SERVER_TEST_SYSTEM_PROXY_REQUEST_URL";
pub(crate) const SYSTEM_PROXY_URL_ENV: &str = "CODEX_EXEC_SERVER_TEST_SYSTEM_PROXY_URL";

const DELAYED_OUTPUT_AFTER_EXIT_CHILD_ARG: &str = "--codex-test-delayed-output-after-exit-child";

#[ctor]
pub static TEST_BINARY_DISPATCH_GUARD: Option<TestBinaryDispatchGuard> = {
    let guard = configure_test_binary_dispatch("codex-exec-server-tests", |_exe_name, argv1| {
        if argv1 == Some(CODEX_ARG0_EXEC_HELPER_ARG1) {
            return TestBinaryDispatchMode::DispatchArg0Only;
        }
        TestBinaryDispatchMode::InstallAliases
    });
    maybe_run_delayed_output_after_exit_from_test_binary();
    maybe_run_exec_server_from_test_binary();
    guard
};

pub(crate) fn current_test_binary_path() -> anyhow::Result<PathBuf> {
    Ok(env::current_exe()?)
}

fn maybe_run_delayed_output_after_exit_from_test_binary() {
    let mut args = env::args();
    let _program = args.next();
    let Some(command) = args.next() else {
        return;
    };
    match command.as_str() {
        DELAYED_OUTPUT_AFTER_EXIT_PARENT_ARG => {
            let release_path = next_release_path_arg(args);
            run_delayed_output_after_exit_parent(&release_path);
        }
        DELAYED_OUTPUT_AFTER_EXIT_CHILD_ARG => {
            let release_path = next_release_path_arg(args);
            run_delayed_output_after_exit_child(&release_path);
        }
        _ => {}
    }
}

fn next_release_path_arg(mut args: impl Iterator<Item = String>) -> PathBuf {
    let Some(release_path) = args.next() else {
        eprintln!("expected release path");
        std::process::exit(1);
    };
    if args.next().is_some() {
        eprintln!("unexpected extra arguments");
        std::process::exit(1);
    }
    PathBuf::from(release_path)
}

fn run_delayed_output_after_exit_parent(release_path: &Path) {
    let current_exe = match env::current_exe() {
        Ok(current_exe) => current_exe,
        Err(error) => {
            eprintln!("failed to resolve current test binary: {error}");
            std::process::exit(1);
        }
    };
    match Command::new(current_exe)
        .arg(DELAYED_OUTPUT_AFTER_EXIT_CHILD_ARG)
        .arg(release_path)
        .stdin(Stdio::null())
        .spawn()
    {
        Ok(_) => std::process::exit(0),
        Err(error) => {
            eprintln!("failed to spawn delayed output child: {error}");
            std::process::exit(1);
        }
    }
}

fn run_delayed_output_after_exit_child(release_path: &Path) {
    for _ in 0..1_000 {
        if release_path.exists() {
            let mut stdout = std::io::stdout().lock();
            if let Err(error) = writeln!(stdout, "late output after exit") {
                eprintln!("failed to write delayed output: {error}");
                std::process::exit(1);
            }
            if let Err(error) = stdout.flush() {
                eprintln!("failed to flush delayed output: {error}");
                std::process::exit(1);
            }
            std::process::exit(0);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    eprintln!(
        "timed out waiting for release path {}",
        release_path.display()
    );
    std::process::exit(1);
}

fn maybe_run_exec_server_from_test_binary() {
    let mut args = env::args();
    let _program = args.next();
    let Some(command) = args.next() else {
        return;
    };
    if command != "exec-server" {
        return;
    }
    // Enable requested child diagnostics so integration tests can observe background failures.
    if env::var_os("RUST_LOG").is_some()
        && let Err(error) = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_writer(std::io::stderr)
            .with_ansi(false)
            .try_init()
    {
        eprintln!("failed to initialize executor test logging: {error}");
        std::process::exit(1);
    }
    // Initialize in the executor child, just as the real CLI does at startup.
    codex_build_info::BuildInfo::initialize(TEST_BUILD_COMMIT);

    let Some(flag) = args.next() else {
        eprintln!("expected --listen");
        std::process::exit(1);
    };
    if flag != "--listen" {
        eprintln!("expected --listen, got `{flag}`");
        std::process::exit(1);
    }
    let Some(listen_url) = args.next() else {
        eprintln!("expected listen URL");
        std::process::exit(1);
    };
    let remaining_args = args.collect::<Vec<_>>();
    let request_dispatch_mode = match remaining_args.as_slice() {
        [] => RequestDispatchMode::Inline,
        [flag, value] if flag == "--concurrent-requests" => match value.parse() {
            Ok(mode) => mode,
            Err(error) => {
                eprintln!("invalid concurrent request count: {error}");
                std::process::exit(1);
            }
        },
        args => {
            eprintln!("unexpected exec-server arguments: {args:?}");
            std::process::exit(1);
        }
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("failed to build Tokio runtime: {error}");
            std::process::exit(1);
        }
    };
    let http_client_factory = match (
        env::var(SYSTEM_PROXY_REQUEST_URL_ENV),
        env::var(SYSTEM_PROXY_URL_ENV),
    ) {
        (Ok(request_url), Ok(proxy_url)) => {
            codex_http_client::cache_system_proxy_route_for_test(&request_url, proxy_url);
            HttpClientFactory::new(OutboundProxyPolicy::RespectSystemProxy)
        }
        (Err(env::VarError::NotPresent), Err(env::VarError::NotPresent)) => {
            HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault)
        }
        _ => {
            eprintln!("system proxy test configuration requires both request and proxy URLs");
            std::process::exit(1);
        }
    };
    let exit_code = match runtime.block_on(codex_exec_server::run_main_with_telemetry(
        &listen_url,
        ExecServerTelemetry::default(),
        http_client_factory,
        request_dispatch_mode,
        codex_websocket_auth::WebsocketAuthSettings::default(),
    )) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("exec-server failed: {error}");
            1
        }
    };
    std::process::exit(exit_code);
}
