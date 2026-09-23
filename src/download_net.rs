//! Network options + proxy/client plumbing: `DownloadOptions`,
//! proxy mode/type combos, system/manual proxy resolution, and the
//! pooled reqwest clients. Mid-level module (settings + net_types):
//! preferences, the engines and tests consume these through the
//! `download` facade.

use crate::net_types::ResolvedProxy;
use crate::runtime::lock_recover;
use gettextrs::gettext;
use gtk4::gio;
use gtk4::gio::prelude::*;
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone, Default)]
pub struct DownloadOptions {
    pub limit_rate: String,
    /// Parallel range connections for large downloads (1 = single stream).
    pub connections: i32,
    pub proxy_mode: String,
    pub proxy_type: String,
    pub proxy_host: String,
    pub proxy_port: i32,
    /// Raw browser-auth setting (`none` when off). The direct engine
    /// exports it to a cookie jar once per attempt (see cookies.rs).
    pub cookies_browser: String,
}

/// Default User-Agent for plain (non-yt-dlp) downloads: a common Chrome
/// string, since some hosts refuse requests with no (or a bot-like) UA.
/// Previously a preference; now fixed.
pub(crate) const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// Proxy modes for the `proxy-mode` setting.
pub const PROXY_MODE_SYSTEM: &str = "system";

pub const PROXY_MODE_MANUAL: &str = "manual";

pub const PROXY_MODE_DIRECT: &str = "direct";

pub const PROXY_MODE_VALUES: &[&str] = &[PROXY_MODE_SYSTEM, PROXY_MODE_MANUAL, PROXY_MODE_DIRECT];

/// Translated combo labels, index-aligned with [`PROXY_MODE_VALUES`].
pub fn proxy_mode_labels() -> Vec<String> {
    vec![gettext("System"), gettext("Manual"), gettext("Off")]
}

/// Combo index for a stored mode value. Unknown values fall back to
/// system (the default).
pub fn proxy_mode_index(value: &str) -> usize {
    crate::media_types::combo_index(PROXY_MODE_VALUES, value, 0)
}

/// Stored value for a combo index. Out-of-range indexes fall back to system.
pub fn proxy_mode_value(index: usize) -> &'static str {
    crate::media_types::combo_value(PROXY_MODE_VALUES, index, PROXY_MODE_SYSTEM)
}

pub const PROXY_TYPE_VALUES: &[&str] = &["http", "https", "socks5"];

/// Protocol names are left untranslated, like codec labels.
pub fn proxy_type_labels() -> Vec<String> {
    vec![
        "HTTP".to_string(),
        "HTTPS".to_string(),
        "SOCKS5".to_string(),
    ]
}

/// Combo index for a stored type value. Unknown values fall back to SOCKS5.
pub fn proxy_type_index(value: &str) -> usize {
    crate::media_types::combo_index(PROXY_TYPE_VALUES, value, 2)
}

/// Stored value for a combo index. Out-of-range indexes fall back to SOCKS5.
pub fn proxy_type_value(index: usize) -> &'static str {
    crate::media_types::combo_value(PROXY_TYPE_VALUES, index, "socks5")
}

/// Loopback bypass applied when no ignore list is configured: exits
/// cannot reach the user's own machine, so proxying localhost only
/// breaks local services.
const LOOPBACK_BYPASS: &str = "localhost,127.0.0.1,::1";

/// Host match for one ignore entry: exact or subdomain suffix, with a
/// leading `*.`/`.` tolerated. Ports and CIDR ranges are out of scope.
fn ignore_entry_normalized(pattern: &str) -> Option<String> {
    let p = pattern.trim().trim_end_matches('.').to_lowercase();
    let p = p.strip_prefix("*.").unwrap_or(&p);
    let p = p.strip_prefix('.').unwrap_or(p);
    if p.is_empty() {
        return None;
    }
    Some(p.to_string())
}

/// Normalize a GNOME ignore-hosts list into plain domains for
/// reqwest's NoProxy (suffix matching): `*.local` and `.local` both
/// become `local`. Unparseable entries are dropped, never passed on.
pub(crate) fn normalize_no_proxy(patterns: &[String]) -> String {
    patterns
        .iter()
        .filter_map(|p| ignore_entry_normalized(p))
        .collect::<Vec<_>>()
        .join(",")
}

fn system_proxy_settings() -> Option<gio::Settings> {
    let source = gio::SettingsSchemaSource::default()?;
    // Settings::new panics on a missing schema (non-GNOME systems, bare
    // CI images): probe first so system mode degrades to direct there.
    source.lookup("org.gnome.system.proxy", true)?;
    Some(gio::Settings::new("org.gnome.system.proxy"))
}

/// http:// + https:// reqwest proxies for one URL, with the bypass
/// list applied. Used by both system and manual resolution; callers
/// differ only in how they surface construction failure.
fn http_proxies(url: &str, no_proxy_env: &str) -> Result<Vec<reqwest::Proxy>, reqwest::Error> {
    [reqwest::Proxy::http(url), reqwest::Proxy::https(url)]
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .map(|proxies| {
            proxies
                .into_iter()
                .map(|p| p.no_proxy(reqwest::NoProxy::from_string(no_proxy_env)))
                .collect()
        })
}

/// Proxy from the desktop settings (`org.gnome.system.proxy`, manual
/// mode). PAC (`auto`) is unsupported by design — executing remote
/// proxy scripts is out of scope — as is a missing schema.
fn system_proxy() -> Option<ResolvedProxy> {
    use gtk4::gio::prelude::SettingsExt as _;
    let s = system_proxy_settings()?;
    if s.string("mode").as_str() != "manual" {
        return None;
    }
    let ignore: Vec<String> = s
        .strv("ignore-hosts")
        .iter()
        .map(|v| v.to_string())
        .collect();
    let bypassed = normalize_no_proxy(&ignore);
    let no_proxy_env = if bypassed.is_empty() {
        LOOPBACK_BYPASS.to_string()
    } else {
        bypassed
    };
    let no_proxy = reqwest::NoProxy::from_string(&no_proxy_env);
    let host = |key: &str| s.string(key).trim().to_string();
    let port = |key: &str| s.int(key);
    // Same host-safe rule as manual_proxy: dconf free text feeds URL
    // construction, so anything outside host chars fails this leg (the
    // lenient-system rule degrades to direct, never to a mangled URL).
    let valid = |h: &str, p: i32| {
        !h.is_empty()
            && (1..=65535).contains(&p)
            && h.chars().all(|c| {
                c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']')
            })
    };
    // SOCKS first: one remote-resolving tunnel covers every scheme,
    // which is also the Tor shape (system SOCKS host + port).
    let socks = host("socks-host");
    if valid(&socks, port("socks-port")) {
        let url = format!("socks5h://{}:{}", socks, port("socks-port"));
        let proxy = reqwest::Proxy::all(url.clone()).ok()?.no_proxy(no_proxy);
        return Some(ResolvedProxy {
            proxies: vec![proxy],
            cache_key: format!("{url}|{no_proxy_env}"),
            cli_url: url,
            no_proxy_env,
        });
    }
    let http = host("http-host");
    let https = host("https-host");
    if s.boolean("use-same-proxy") && valid(&http, port("http-port")) {
        let url = format!("http://{}:{}", http, port("http-port"));
        let proxies = http_proxies(&url, &no_proxy_env).ok()?;
        return Some(ResolvedProxy {
            proxies,
            cache_key: format!("{url}|{no_proxy_env}"),
            cli_url: url,
            no_proxy_env,
        });
    }
    // Split HTTP/HTTPS proxies: cover each scheme present. CLI gets the
    // secure leg (the sensitive half); plain-HTTP direct fallback there
    // is documented on the mode row.
    let mut proxies = Vec::new();
    if valid(&http, port("http-port")) {
        proxies.push(
            reqwest::Proxy::http(format!("http://{}:{}", http, port("http-port")))
                .ok()?
                .no_proxy(reqwest::NoProxy::from_string(&no_proxy_env)),
        );
    }
    if valid(&https, port("https-port")) {
        proxies.push(
            reqwest::Proxy::https(format!("http://{}:{}", https, port("https-port")))
                .ok()?
                .no_proxy(reqwest::NoProxy::from_string(&no_proxy_env)),
        );
    }
    if proxies.is_empty() {
        return None;
    }
    let cli_url = if valid(&https, port("https-port")) {
        format!("http://{}:{}", https, port("https-port"))
    } else {
        format!("http://{}:{}", http, port("http-port"))
    };
    Some(ResolvedProxy {
        proxies,
        cache_key: format!("{cli_url}|{no_proxy_env}"),
        cli_url,
        no_proxy_env,
    })
}

/// Proxy from the manual settings. Unlike system resolution this is an
/// explicit user demand: invalid values fail loudly instead of
/// silently leaking direct, because a typo must never look like
/// privacy. Proxies are unauthenticated: authentication was dropped
/// because the password had to travel in cleartext process argv.
fn manual_proxy(o: &DownloadOptions) -> Result<Option<ResolvedProxy>, String> {
    let host = o.proxy_host.trim();
    if host.is_empty() {
        return Err(gettext("Proxy host is empty"));
    }
    if !(1..=65535).contains(&o.proxy_port) {
        return Err(gettext("Proxy port is out of range (1–65535)"));
    }
    // Free-text host feeds URL construction below (`http://{host}:{port}`):
    // anything outside host-safe chars (`@`, `/`, `?`, `#`, whitespace…)
    // would smuggle userinfo, paths or log splits into the proxy URL.
    if !host
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']'))
    {
        return Err(gettext("Proxy host contains invalid characters"));
    }
    let no_proxy_env = LOOPBACK_BYPASS.to_string();
    let apply_bypass = |p: reqwest::Proxy| p.no_proxy(reqwest::NoProxy::from_string(&no_proxy_env));
    let (proxies, cli_url) = match o.proxy_type.as_str() {
        "http" | "https" => {
            let url = format!("http://{host}:{}", o.proxy_port);
            let proxies = http_proxies(&url, &no_proxy_env).map_err(|e| e.to_string())?;
            (proxies, url)
        }
        // SOCKS5 always remote-resolving: local DNS would leak every
        // hostname around the tunnel.
        "socks5" => {
            let url = format!("socks5h://{host}:{}", o.proxy_port);
            let proxy = apply_bypass(reqwest::Proxy::all(url.clone()).map_err(|e| e.to_string())?);
            (vec![proxy], url)
        }
        other => {
            return Err(gettext("Unknown proxy type: {t}").replace("{t}", other));
        }
    };
    Ok(Some(ResolvedProxy {
        proxies,
        cache_key: format!("{cli_url}|{no_proxy_env}"),
        cli_url,
        no_proxy_env,
    }))
}

impl DownloadOptions {
    /// Snapshot the network-related GSettings keys.
    pub fn from_settings(s: &crate::settings::AppSettings) -> Self {
        Self {
            limit_rate: s.speed_limit(),
            connections: s.connections(),
            proxy_mode: s.proxy_mode(),
            proxy_type: s.proxy_type(),
            proxy_host: s.proxy_host(),
            proxy_port: s.proxy_port(),
            cookies_browser: s.cookies_browser(),
        }
    }

    /// Proxy for this attempt. Manual misconfiguration fails loudly;
    /// system resolution degrades to direct (missing schema, PAC mode,
    /// empty hosts) since it is opportunistic, not demanded.
    pub fn proxy_config(&self) -> Result<Option<ResolvedProxy>, String> {
        match self.proxy_mode.as_str() {
            PROXY_MODE_DIRECT => Ok(None),
            PROXY_MODE_MANUAL => manual_proxy(self),
            // System default and unknown values resolve opportunistically.
            _ => Ok(system_proxy()),
        }
    }
}

pub(crate) fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| client_builder().build().expect("http client"))
}

/// reqwest client honoring this attempt's proxy. Proxied configs share
/// one pooled client per proxy config; direct attempts keep the static
/// client, so enabling a proxy never perturbs existing connection pools.
pub(crate) fn http_client_for(proxy: Option<&ResolvedProxy>) -> reqwest::Client {
    let Some(proxy) = proxy else {
        return http_client().clone();
    };
    let cache = PROXIED.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    if let Some(client) = lock_recover(cache).get(&proxy.cache_key) {
        return client.clone();
    }
    let mut builder = client_builder();
    for p in &proxy.proxies {
        builder = builder.proxy(p.clone());
    }
    let client = builder.build().expect("proxied http client");
    lock_recover(cache).insert(proxy.cache_key.clone(), client.clone());
    client
}

static PROXIED: OnceLock<Mutex<std::collections::HashMap<String, reqwest::Client>>> =
    OnceLock::new();

/// Test hook: how many unauthenticated proxied clients are pooled.
#[cfg(test)]
pub(crate) fn proxied_pool_len() -> usize {
    lock_recover(PROXIED.get_or_init(|| Mutex::new(std::collections::HashMap::new()))).len()
}

/// Shared builder: bounded hops, no downgrades (see [`http_client`]).
fn client_builder() -> reqwest::ClientBuilder {
    // Bounded hops: a malicious server must not bounce the client
    // around without limit. Downgrades are refused outright: no
    // cookie or auth store is enabled, but a https→http bounce would
    // still let a network attacker substitute the downloaded bytes.
    // Only the URL + UA cross origins.
    //
    // No ambient proxy either: reqwest would otherwise route through
    // HTTP_PROXY-style env vars behind Direct mode's back. Proxying
    // is always explicit here (settings or nothing).
    // Automatic Referer is off for the same reason: cross-origin
    // redirects must not leak full URLs (tokens included) as Referer.
    reqwest::Client::builder()
        .no_proxy()
        .referer(false)
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            let downgrade = attempt
                .previous()
                .last()
                .is_some_and(|u| u.scheme() == "https")
                && attempt.url().scheme() == "http";
            if attempt.previous().len() > 5 || downgrade {
                attempt.stop()
            } else {
                attempt.follow()
            }
        }))
}
