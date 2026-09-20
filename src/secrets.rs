//! System keyring access for credentials that must never live in
//! GSettings, logs, cache keys, or error strings.
//!
//! Backed by oo7: the Secret Service on the session bus, or the Secret
//! portal when sandboxed (the Flatpak already allows
//! `--talk-name=org.freedesktop.secrets`). Every operation is async; the
//! proxy path keeps an in-memory copy because proxy resolution is
//! synchronous — the cache is filled at startup and refreshed after every
//! edit, and the sync path only ever consults the snapshot.

use std::fmt;
use std::sync::{Mutex, OnceLock};

use gettextrs::gettext;

/// Which credential a keyring entry holds. One entry per kind; add new
/// kinds here (with their own attributes) rather than new modules.
#[derive(Clone, Copy, Debug)]
enum SecretKind {
    /// Manual proxy password, consumed by the synchronous proxy path.
    ProxyPassword,
}

impl SecretKind {
    fn label(&self) -> &'static str {
        match self {
            SecretKind::ProxyPassword => "Grab proxy password",
        }
    }

    /// Namespaced lookup attributes. Fixed per kind: the password belongs
    /// to the proxy configuration rather than to a username, so changing
    /// the username keeps working without re-entering the password.
    fn attributes(&self) -> [(&'static str, &'static str); 2] {
        match self {
            SecretKind::ProxyPassword => [
                ("application", "io.github.houssemko.Grab"),
                ("kind", "proxy-password"),
            ],
        }
    }
}

/// In-memory state of one cached credential. The keyring API is async but
/// proxy resolution is synchronous, so the password is loaded once at
/// startup (and refreshed after every edit); the sync path only ever sees
/// this snapshot.
#[derive(Clone, Default)]
pub enum CachedSecret {
    /// The keyring read has not finished yet.
    #[default]
    Unloaded,
    /// A secret is stored and cached.
    Present(String),
    /// The keyring was read and holds no entry for this kind.
    Absent,
    /// The keyring read failed; the payload is the secret-free error text.
    Failed(String),
}

// A derived Debug would print the secret bytes: redact them instead.
impl fmt::Debug for CachedSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CachedSecret::Unloaded => write!(f, "Unloaded"),
            CachedSecret::Present(_) => write!(f, "Present(<redacted>)"),
            CachedSecret::Absent => write!(f, "Absent"),
            CachedSecret::Failed(e) => write!(f, "Failed({e:?})"),
        }
    }
}

static PROXY_PASSWORD: OnceLock<Mutex<CachedSecret>> = OnceLock::new();

fn proxy_slot() -> &'static Mutex<CachedSecret> {
    PROXY_PASSWORD.get_or_init(|| Mutex::new(CachedSecret::Unloaded))
}

/// Snapshot the cached proxy password for the synchronous proxy path.
pub fn cached_proxy_password() -> CachedSecret {
    crate::download::lock_recover(proxy_slot()).clone()
}

fn set_cached(secret: CachedSecret) {
    *crate::download::lock_recover(proxy_slot()) = secret;
}

/// Read one credential from the keyring: `Ok(None)` when no entry exists.
/// Errors never carry secret bytes (oo7 only reports transport errors).
async fn keyring_load(kind: SecretKind) -> Result<Option<String>, String> {
    let keyring = oo7::Keyring::new().await.map_err(|e| e.to_string())?;
    let mut items = keyring
        .search_items(&kind.attributes())
        .await
        .map_err(|e| e.to_string())?;
    let Some(item) = items.pop() else {
        return Ok(None);
    };
    let secret = item.secret().await.map_err(|e| e.to_string())?;
    String::from_utf8(secret.as_bytes().to_vec())
        .map(Some)
        .map_err(|_| gettext("The stored credential is not valid UTF-8"))
}

/// Store one credential (replacing any existing entry), or delete the entry
/// when the new value is empty. Errors never carry secret bytes.
async fn keyring_store(kind: SecretKind, password: &str) -> Result<(), String> {
    let keyring = oo7::Keyring::new().await.map_err(|e| e.to_string())?;
    if password.is_empty() {
        // Deleting a missing entry is a no-op, never an error.
        keyring
            .delete(&kind.attributes())
            .await
            .map_err(|e| e.to_string())?;
    } else {
        keyring
            .create_item(kind.label(), &kind.attributes(), password.as_bytes(), true)
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// (Re)load the proxy password from the keyring into the in-memory cache.
/// Called at startup and after every edit; never blocks the sync path.
pub async fn refresh_proxy_password() {
    let next = match keyring_load(SecretKind::ProxyPassword).await {
        Ok(Some(password)) => CachedSecret::Present(password),
        Ok(None) => CachedSecret::Absent,
        Err(e) => CachedSecret::Failed(e),
    };
    set_cached(next);
}

/// Fire-and-forget keyring load for contexts that cannot await (startup,
/// preferences). The sync proxy path keeps failing loudly on `Unloaded`
/// until this completes. Concurrent calls are harmless: the load is an
/// idempotent read and every completion writes the same state.
pub fn ensure_proxy_password_loaded() {
    if !matches!(
        *crate::download::lock_recover(proxy_slot()),
        CachedSecret::Unloaded
    ) {
        return;
    }
    crate::download::tokio_rt().spawn(async {
        refresh_proxy_password().await;
    });
}

/// Store the proxy password from the preferences row (empty clears it),
/// then refresh the cache so the sync path sees the new value. The cache
/// is re-read even on failure so it reflects what is actually stored.
pub async fn store_proxy_password(password: &str) -> Result<(), String> {
    let result = keyring_store(SecretKind::ProxyPassword, password).await;
    refresh_proxy_password().await;
    result
}
