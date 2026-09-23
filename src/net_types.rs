//! Proxy identity shared by every engine: the resolved proxy travels
//! from settings into HTTP interceptors, yt-dlp spawns and the torrent
//! session. Leaf module (reqwest types only): the resolve/pooling impls
//! stay in `download.rs`, which is the only place that builds these.

/// Proxy resolved for one attempt: reqwest interceptors for the direct
/// engine, plus the CLI form for yt-dlp spawns.
#[derive(Clone, Debug)]
pub(crate) struct ResolvedProxy {
    pub(crate) proxies: Vec<reqwest::Proxy>,
    /// Single URL for `--proxy` (SOCKS always remote-resolving).
    pub cli_url: String,
    /// Comma list for NO_PROXY env on yt-dlp spawns.
    pub no_proxy_env: String,
    pub(crate) cache_key: String,
}

impl ResolvedProxy {
    /// SOCKS5 URL for the torrent engine: librqbit's `proxy_url` demands
    /// exactly the `socks5://` scheme, so the remote-resolving `socks5h://`
    /// form normalizes down. Peer addresses arrive as IPs (trackers, PEX —
    /// DHT is off under proxy), so no hostname resolution happens on the
    /// peer path at all. HTTP(S) proxies yield `None`: the engine has no
    /// HTTP-CONNECT peer path, so those torrents stay direct instead of
    /// failing.
    pub fn torrent_socks_url(&self) -> Option<String> {
        let rest = self
            .cli_url
            .strip_prefix("socks5h://")
            .or_else(|| self.cli_url.strip_prefix("socks5://"))?;
        Some(format!("socks5://{rest}"))
    }
}
