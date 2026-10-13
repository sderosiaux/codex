//! ChatGPT cookies shared by HTTP and WebSocket transports. Only infrastructure cookies may be
//! stored globally; configured cookies remain scoped to their factory.

use std::sync::Arc;
use std::sync::LazyLock;

use http::HeaderMap;
use http::HeaderValue;
use http::Uri;
use http::header::SET_COOKIE;
use reqwest::cookie::CookieStore;
use reqwest::cookie::Jar;

use crate::HttpClientFactory;
use crate::chatgpt_hosts::is_allowed_chatgpt_host;

// WARNING: this HTTP cookie store is process-global and may be shared across auth contexts.
// It must only ever contain Cloudflare infrastructure cookies and the `__oailb`
// routing cookie. Never extend this store to persist ChatGPT account, session,
// auth, or other user-specific cookie data.
static SHARED_CHATGPT_CLOUDFLARE_COOKIE_STORE: LazyLock<Arc<ChatGptCloudflareCookieStore>> =
    LazyLock::new(|| Arc::new(ChatGptCloudflareCookieStore::default()));

#[derive(Debug, Default)]
struct ChatGptCloudflareCookieStore {
    jar: Jar,
}

pub(crate) struct ChatGptCookieStore {
    cloudflare: Arc<ChatGptCloudflareCookieStore>,
    cookies: Vec<HeaderValue>,
}

impl ChatGptCookieStore {
    pub(crate) fn new(cookies: Vec<HeaderValue>) -> Self {
        Self {
            cloudflare: Arc::clone(&SHARED_CHATGPT_CLOUDFLARE_COOKIE_STORE),
            cookies,
        }
    }

    pub(crate) fn configured_cookies(&self) -> &[HeaderValue] {
        &self.cookies
    }
}

impl CookieStore for ChatGptCloudflareCookieStore {
    fn set_cookies(
        &self,
        cookie_headers: &mut dyn Iterator<Item = &HeaderValue>,
        url: &reqwest::Url,
    ) {
        if !is_chatgpt_cookie_url(url) {
            return;
        }

        let mut cloudflare_cookie_headers =
            cookie_headers.filter(|header| is_allowed_cloudflare_set_cookie_header(header));
        self.jar.set_cookies(&mut cloudflare_cookie_headers, url);
    }

    fn cookies(&self, url: &reqwest::Url) -> Option<HeaderValue> {
        if is_chatgpt_cookie_url(url) {
            self.jar.cookies(url).and_then(only_cloudflare_cookies)
        } else {
            None
        }
    }
}

impl CookieStore for ChatGptCookieStore {
    fn set_cookies(
        &self,
        cookie_headers: &mut dyn Iterator<Item = &HeaderValue>,
        url: &reqwest::Url,
    ) {
        self.cloudflare.set_cookies(cookie_headers, url);
    }

    fn cookies(&self, url: &reqwest::Url) -> Option<HeaderValue> {
        if !is_chatgpt_cookie_url(url) {
            return self.cloudflare.cookies(url);
        }
        let cloudflare = self.cloudflare.cookies(url);
        let mut sensitive = cloudflare.as_ref().is_some_and(HeaderValue::is_sensitive);
        let mut cookies = cloudflare
            .map(|cloudflare| cloudflare.as_bytes().to_vec())
            .unwrap_or_default();
        for cookie in &self.cookies {
            sensitive |= cookie.is_sensitive();
            if !cookies.is_empty() {
                cookies.extend_from_slice(b"; ");
            }
            cookies.extend_from_slice(cookie.as_bytes());
        }
        if cookies.is_empty() {
            None
        } else {
            let mut header = HeaderValue::from_bytes(&cookies).ok()?;
            header.set_sensitive(sensitive);
            Some(header)
        }
    }
}

impl HttpClientFactory {
    /// Returns cookies for a ChatGPT HTTPS or WSS request, using the same store as HTTP clients.
    /// Explicit request Cookie headers should take precedence over this value.
    pub fn chatgpt_cookie_header(&self, uri: &Uri) -> Option<HeaderValue> {
        let url = chatgpt_cookie_url(uri)?;
        let mut cookies = match self.chatgpt_cookie_store() {
            Some(store) => store.cookies(&url),
            None => SHARED_CHATGPT_CLOUDFLARE_COOKIE_STORE.cookies(&url),
        }?;
        cookies.set_sensitive(true);
        Some(cookies)
    }

    /// Retains only allowlisted infrastructure cookies from a ChatGPT HTTPS or WSS response.
    /// Account and session cookies are never added to the shared store.
    pub fn store_chatgpt_response_cookies(&self, uri: &Uri, headers: &HeaderMap) {
        if let Some(url) = chatgpt_cookie_url(uri) {
            SHARED_CHATGPT_CLOUDFLARE_COOKIE_STORE
                .set_cookies(&mut headers.get_all(SET_COOKIE).iter(), &url);
        }
    }
}

fn chatgpt_cookie_url(uri: &Uri) -> Option<reqwest::Url> {
    let mut url = reqwest::Url::parse(&uri.to_string()).ok()?;
    // A secure WebSocket handshake has the same cookie scope as HTTPS.
    if url.scheme() == "wss" {
        url.set_scheme("https").ok()?;
    }
    is_chatgpt_cookie_url(&url).then_some(url)
}

/// Adds the process-local ChatGPT infrastructure cookie jar used by Codex HTTP clients.
///
/// WARNING: this jar is global within the process. It is only acceptable because it hardcodes a
/// small allowlist of Cloudflare cookie names plus `__oailb` for routing, and refuses all other
/// ChatGPT cookies. Do not store ChatGPT account, session, auth, or other user-specific cookies
/// here. If a future caller needs those cookies, the store must be scoped to the auth/session
/// owner instead of shared globally.
pub fn with_chatgpt_cloudflare_cookie_store(
    builder: reqwest::ClientBuilder,
) -> reqwest::ClientBuilder {
    builder.cookie_provider(Arc::clone(&SHARED_CHATGPT_CLOUDFLARE_COOKIE_STORE))
}

fn is_chatgpt_cookie_url(url: &reqwest::Url) -> bool {
    match url.scheme() {
        "https" => {}
        _ => return false,
    }

    let Some(host) = url.host_str() else {
        return false;
    };

    is_allowed_chatgpt_host(host)
}

fn is_allowed_cloudflare_set_cookie_header(header: &HeaderValue) -> bool {
    header
        .to_str()
        .ok()
        .and_then(set_cookie_name)
        .is_some_and(is_allowed_cloudflare_cookie_name)
}

fn set_cookie_name(header: &str) -> Option<&str> {
    let (name, _) = header.split_once('=')?;
    let name = name.trim();
    (!name.is_empty()).then_some(name)
}

fn only_cloudflare_cookies(header: HeaderValue) -> Option<HeaderValue> {
    let header = header.to_str().ok()?;
    let cookies = header
        .split(';')
        .filter_map(|cookie| {
            let cookie = cookie.trim();
            let name = cookie.split_once('=')?.0.trim();
            is_allowed_cloudflare_cookie_name(name).then_some(cookie)
        })
        .collect::<Vec<_>>()
        .join("; ");

    if cookies.is_empty() {
        None
    } else {
        HeaderValue::from_str(&cookies).ok()
    }
}

fn is_allowed_cloudflare_cookie_name(name: &str) -> bool {
    // `__oailb` is an OpenAI infrastructure routing cookie, not an authentication cookie.
    // Keep this allowlist aligned with Cloudflare's documented service cookies:
    // https://developers.cloudflare.com/fundamentals/reference/policies-compliances/cloudflare-cookies/
    matches!(
        name,
        "__cf_bm"
            | "__cflb"
            | "__cfruid"
            | "__cfseq"
            | "__cfwaitingroom"
            | "__oailb"
            | "_cfuvid"
            | "cf_clearance"
            | "cf_ob_info"
            | "cf_use_ob"
    ) || name.starts_with("cf_chl_")
}
