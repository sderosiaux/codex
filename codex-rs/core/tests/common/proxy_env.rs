//! Environment for tests that run a subprocess against a mock HTTP proxy.

/// Variables that can route or bypass outbound HTTP traffic; clear them all before setting the
/// ones a proxy test needs so the developer's own proxy settings cannot leak in.
pub const PROXY_ENV_KEYS: &[&str] = &[
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
    "WS_PROXY",
    "WSS_PROXY",
    "ws_proxy",
    "wss_proxy",
];

/// Keeps loopback and private-network mock servers off the proxy while other hosts go through it.
pub const DEFAULT_NO_PROXY_VALUE: &str =
    "localhost,127.0.0.1,::1,10.0.0.0/8,172.16.0.0/12,192.168.0.0/16";
