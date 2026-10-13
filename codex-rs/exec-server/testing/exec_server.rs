//! Minimal exec-server fixture for Bazel-only integration tests.
//!
//! Linking only exec-server avoids depending on the full Codex CLI binary
//! when a test only needs a WebSocket executor endpoint. It handles the arg0
//! helper mode because processes started with a custom arg0 re-exec this binary.

use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    #[cfg(target_os = "linux")]
    codex_utils_pty::init_spawn_helper(std::env::args_os());
    #[cfg(unix)]
    if std::env::args_os().nth(1).as_deref()
        == Some(std::ffi::OsStr::new(
            codex_exec_server::CODEX_ARG0_EXEC_HELPER_ARG1,
        ))
    {
        codex_exec_server::run_arg0_exec_helper_main();
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(codex_exec_server::run_main(
            "ws://127.0.0.1:0",
            HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
        ))
}
