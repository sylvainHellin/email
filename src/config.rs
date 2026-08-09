use anyhow::{Context, Result};
use colored::*;
use log::debug;
use serde::Deserialize;
use simplelog::{format_description, CombinedLogger, ConfigBuilder, LevelFilter, WriteLogger};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::Utc;

use crate::types::MailboxRole;

// ---------------------------------------------------------------------------
// Global config (loaded from ~/.config/mailypoppins/config.toml)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default, Clone)]
pub struct GlobalConfig {
    /// TUI color theme name. Built-ins: "catppuccin-mocha" (the default,
    /// today's exact appearance), "catppuccin-latte", "tokyo-night",
    /// "terminal" (adapts to the terminal's own ANSI palette).
    /// Unknown names warn and fall back to the default.
    #[serde(default)]
    pub theme: String,
    /// Desktop notifications for new mail while the TUI is running
    /// (macOS: `osascript`, Linux: `notify-send`; missing tools degrade
    /// silently). Opt-in: defaults to off. See src/notify.rs (#0009).
    #[serde(default)]
    pub notifications: bool,
    #[serde(default)]
    pub email: EmailSettings,
    /// Global retention defaults for the local cache. Every field is optional
    /// and falls back to the constants documented on [`RetentionPolicy`].
    #[serde(default)]
    pub retention: RetentionConfig,
    #[serde(default)]
    pub accounts: Vec<AccountConfig>,
    /// Where to store SMTP/IMAP passwords and OAuth2 token caches.
    /// Default: "encrypted-file" (machine-bound). Opt-in: "keyring".
    #[serde(default)]
    pub secrets_backend: crate::secrets::SecretsBackendKind,
}

/// Authentication method for an account.
#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum AuthMethod {
    Password,
    #[serde(rename = "oauth2")]
    OAuth2,
    #[serde(rename = "graph")]
    Graph,
}

impl Default for AuthMethod {
    fn default() -> Self {
        AuthMethod::Password
    }
}

/// OAuth2 settings stored in config (client_id + tenant_id for Azure Entra ID).
#[derive(Debug, Deserialize, Default, Clone)]
pub struct OAuth2Settings {
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub tenant_id: String,
}

/// Per-account configuration (one entry in `[[accounts]]`).
#[derive(Debug, Deserialize, Default, Clone)]
pub struct AccountConfig {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub default_from: String,
    #[serde(default)]
    pub auth_method: AuthMethod,
    #[serde(default)]
    pub oauth2: Option<OAuth2Settings>,
    #[serde(default)]
    pub smtp: SmtpSettings,
    #[serde(default)]
    pub imap: ImapSettings,
    #[serde(default)]
    pub mailboxes: MailboxesConfig,
    #[serde(default)]
    pub signatures: SignaturesConfig,
    /// Per-account retention overrides. Unset fields inherit the global
    /// `[retention]` table, which itself falls back to the defaults.
    #[serde(default)]
    pub retention: RetentionConfig,
    /// Whether a sent message is copied into the server's Sent mailbox by this
    /// client. See [`SaveToSent`]; `auto` is almost always right.
    #[serde(default)]
    pub save_to_sent: SaveToSent,
}

impl AccountConfig {
    /// Whether this account has no remote source at all: no IMAP host (nor the
    /// SMTP host [`ImapConfig::load`] falls back to) and no Graph.
    ///
    /// Such an account is legitimate: drafts are local files, so a config can
    /// hold an account that only ever writes them. The TUI already supports it
    /// (startup auto-fetch skips accounts with neither transport); `mp sync
    /// --all-accounts` used to count it as a failure and exit 1 forever.
    ///
    /// Configured-but-unusable is deliberately *not* local-only: an account
    /// with a host and a missing password must still be reported as a failure,
    /// which is why this reads the config rather than asking whether
    /// [`ImapConfig::load`] succeeds.
    pub fn is_local_only(&self) -> bool {
        self.auth_method != AuthMethod::Graph
            && self.imap.host.trim().is_empty()
            && self.smtp.host.trim().is_empty()
    }
}

/// Whether the client APPENDs its own copy of a sent message to the Sent
/// mailbox (#0037 item 5).
///
/// Getting this wrong costs duplicate Sent items, the Thunderbird bug 1427619
/// failure mode, so the default detects the account type instead of guessing a
/// fixed answer: Gmail, Microsoft Graph and Proton save the message themselves
/// on submission, generic IMAP does not.
#[derive(Debug, Deserialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SaveToSent {
    /// Skip the APPEND for accounts whose server already saves the copy, keep
    /// it for everything else. See [`server_saves_to_sent`].
    #[default]
    Auto,
    /// Always APPEND, even when the server may also save a copy.
    Always,
    /// Never APPEND. The Sent mailbox is then entirely the server's business.
    Never,
}

#[derive(Debug, Deserialize, Clone)]
pub struct EmailSettings {
    #[serde(default = "default_font_family")]
    pub font_family: String,
    #[serde(default = "default_font_size")]
    pub font_size: String,
    #[serde(default = "default_true")]
    pub include_signature: bool,
}

impl Default for EmailSettings {
    fn default() -> Self {
        Self {
            font_family: default_font_family(),
            font_size: default_font_size(),
            include_signature: true,
        }
    }
}

fn default_font_family() -> String {
    "Helvetica, Arial, sans-serif".to_string()
}

fn default_font_size() -> String {
    "12pt".to_string()
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct SmtpSettings {
    #[serde(default)]
    pub host: String,
    #[serde(default = "default_smtp_port")]
    pub port: u16,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub accept_invalid_certs: bool,
}

fn default_smtp_port() -> u16 {
    465
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct ImapSettings {
    #[serde(default)]
    pub host: String,
    #[serde(default = "default_imap_port")]
    pub port: u16,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub accept_invalid_certs: bool,
    /// How many mailboxes a single sync fetches in parallel, each on its own
    /// IMAP connection (#0005). A sync SELECTs one mailbox per connection, so
    /// N mailboxes on one session cost N round-trips serially; N connections
    /// overlap the latency. Kept conservative because servers throttle
    /// concurrent connections, Gmail especially. Clamped to [1, 8] at load;
    /// 1 restores the old serial ordering (one session per mailbox, opened in turn).
    #[serde(default = "default_fetch_concurrency")]
    pub fetch_concurrency: usize,
}

fn default_imap_port() -> u16 {
    993
}

/// The default per-account fetch fan-out. Four covers the usual inbox +
/// archive + sent (+ one extra) without opening more connections than a
/// throttling server tolerates.
fn default_fetch_concurrency() -> usize {
    4
}

#[derive(Debug, Deserialize, Clone)]
pub struct MailboxMapping {
    pub server: String,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct MailboxesConfig {
    #[serde(default)]
    pub inbox: Option<MailboxMapping>,
    #[serde(default)]
    pub archive: Option<MailboxMapping>,
    #[serde(default)]
    pub sent: Option<MailboxMapping>,
    #[serde(default)]
    pub extra: Option<Vec<MailboxMapping>>,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct SignaturesConfig {
    #[serde(default)]
    pub default: Option<String>,
    #[serde(flatten)]
    pub entries: HashMap<String, SignatureEntry>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SignatureEntry {
    #[serde(default)]
    pub name: Option<String>,
    pub path: String,
}

// ---------------------------------------------------------------------------
// Retention and disk budget (see docs/plans/data-access-layer.md)
// ---------------------------------------------------------------------------
//
// The local store is a cache in front of the server, so evicting a body or an
// attachment is not deletion: the envelope row stays and the bytes are
// re-fetched on open. That is what makes retention a safe, user-facing knob.
//
// This is the parsing and defaults surface only. Nothing here evicts anything;
// the pruning pass lands with the eviction work.

/// Default metadata horizon: keep every envelope row, forever, so the message
/// list and search always render the full history. Envelopes are cheap.
pub const DEFAULT_METADATA_HORIZON_DAYS: u32 = 0;

/// Default body horizon: one year of full message bodies.
pub const DEFAULT_BODY_HORIZON_DAYS: u32 = 365;

/// Default attachment horizon: 90 days. Shorter than the body horizon because
/// attachments dominate disk use.
pub const DEFAULT_ATTACHMENT_HORIZON_DAYS: u32 = 90;

/// Default disk budget per account: 5 GB, a conservative cap that overrides
/// the horizons when the two disagree.
pub const DEFAULT_MAX_DISK_BYTES: u64 = 5_000_000_000;

/// Upper bound on any horizon: 36500 days (100 years). Larger values are a
/// typo, and `0` already means keep-all.
pub const MAX_HORIZON_DAYS: u32 = 36_500;

/// Lower bound on the disk budget: 10 MB. Below this the cache cannot hold a
/// useful working set and the evictor would thrash.
pub const MIN_MAX_DISK_BYTES: u64 = 10_000_000;

/// Upper bound on the disk budget: 1 TB.
pub const MAX_MAX_DISK_BYTES: u64 = 1_000_000_000_000;

/// Retention settings as written in the config file, global or per account.
///
/// Every field is optional so that an account can override one horizon without
/// restating the others; [`RetentionPolicy::resolve`] fills the gaps.
#[derive(Debug, Deserialize, Default, Clone, PartialEq, Eq)]
pub struct RetentionConfig {
    /// How far back to keep envelope rows, in days. `0` means keep all.
    #[serde(default)]
    pub metadata_horizon_days: Option<u32>,
    /// How far back to keep message bodies, in days. `0` means keep all.
    #[serde(default)]
    pub body_horizon_days: Option<u32>,
    /// How far back to keep attachment blobs, in days. `0` means keep all.
    #[serde(default)]
    pub attachment_horizon_days: Option<u32>,
    /// Disk budget for this account's store and blobs, in bytes.
    #[serde(default)]
    pub max_disk_bytes: Option<u64>,
}

/// A fully resolved retention policy: no optionals, every value validated.
///
/// The horizons express intent ("keep bodies for a year") and
/// `max_disk_bytes` is the hard cap that overrides them when the two disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    pub metadata_horizon_days: u32,
    pub body_horizon_days: u32,
    pub attachment_horizon_days: u32,
    pub max_disk_bytes: u64,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            metadata_horizon_days: DEFAULT_METADATA_HORIZON_DAYS,
            body_horizon_days: DEFAULT_BODY_HORIZON_DAYS,
            attachment_horizon_days: DEFAULT_ATTACHMENT_HORIZON_DAYS,
            max_disk_bytes: DEFAULT_MAX_DISK_BYTES,
        }
    }
}

impl RetentionPolicy {
    /// Layer `account` over `global` over the defaults, field by field, then
    /// validate. An account that sets only `attachment_horizon_days` keeps the
    /// global (or default) value for everything else.
    pub fn resolve(global: &RetentionConfig, account: &RetentionConfig) -> Result<Self> {
        let defaults = Self::default();
        let policy = Self {
            metadata_horizon_days: account
                .metadata_horizon_days
                .or(global.metadata_horizon_days)
                .unwrap_or(defaults.metadata_horizon_days),
            body_horizon_days: account
                .body_horizon_days
                .or(global.body_horizon_days)
                .unwrap_or(defaults.body_horizon_days),
            attachment_horizon_days: account
                .attachment_horizon_days
                .or(global.attachment_horizon_days)
                .unwrap_or(defaults.attachment_horizon_days),
            max_disk_bytes: account
                .max_disk_bytes
                .or(global.max_disk_bytes)
                .unwrap_or(defaults.max_disk_bytes),
        };
        policy.validate()?;
        Ok(policy)
    }

    /// Reject values that cannot mean what the user typed, naming the field,
    /// the offending value and the allowed range.
    pub fn validate(&self) -> Result<()> {
        for (field, days) in [
            ("metadata_horizon_days", self.metadata_horizon_days),
            ("body_horizon_days", self.body_horizon_days),
            ("attachment_horizon_days", self.attachment_horizon_days),
        ] {
            if days > MAX_HORIZON_DAYS {
                return Err(anyhow::anyhow!(
                    "[retention] {field} = {days} is out of range: expected 0 to {MAX_HORIZON_DAYS} days \
                     (0 means keep everything)."
                ));
            }
        }
        if self.max_disk_bytes < MIN_MAX_DISK_BYTES || self.max_disk_bytes > MAX_MAX_DISK_BYTES {
            return Err(anyhow::anyhow!(
                "[retention] max_disk_bytes = {} is out of range: expected {} to {} bytes \
                 (10 MB to 1 TB).",
                self.max_disk_bytes,
                MIN_MAX_DISK_BYTES,
                MAX_MAX_DISK_BYTES
            ));
        }
        Ok(())
    }
}

/// The retention policy in force for one account: its own overrides layered
/// over the global `[retention]` table.
pub fn retention_for(config: &GlobalConfig, account: &AccountConfig) -> Result<RetentionPolicy> {
    RetentionPolicy::resolve(&config.retention, &account.retention)
        .with_context(|| format!("invalid retention config for account '{}'", account.name))
}

// ---------------------------------------------------------------------------
// Resolved runtime configs (include secrets from the secrets backend)
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub default_from: String,
    pub accept_invalid_certs: bool,
    pub auth_method: AuthMethod,
}

#[derive(Clone)]
pub struct ImapConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub accept_invalid_certs: bool,
    pub auth_method: AuthMethod,
    /// Parallel mailbox fetches per sync, clamped to [1, 8]. See
    /// [`ImapSettings::fetch_concurrency`].
    pub fetch_concurrency: usize,
}

/// Runtime config for Graph API accounts (no SMTP/IMAP needed).
#[derive(Clone)]
pub struct GraphConfig {
    pub client_id: String,
    pub tenant_id: String,
    pub username: String,
    pub account_name: String,
}

impl GraphConfig {
    pub fn load(account: &AccountConfig) -> Result<Self> {
        let oauth2_settings = account
            .oauth2
            .as_ref()
            .context("Graph auth_method requires [accounts.oauth2] config with client_id and tenant_id")?;
        if oauth2_settings.client_id.is_empty() || oauth2_settings.tenant_id.is_empty() {
            return Err(anyhow::anyhow!(
                "Graph auth_method requires non-empty client_id and tenant_id in [accounts.oauth2]"
            ));
        }
        let username = if !account.smtp.username.is_empty() {
            account.smtp.username.clone()
        } else {
            account.default_from.clone()
        };
        Ok(Self {
            client_id: oauth2_settings.client_id.clone(),
            tenant_id: oauth2_settings.tenant_id.clone(),
            username,
            account_name: account.name.clone(),
        })
    }
}

// ---------------------------------------------------------------------------
// Secrets backend (file-encrypted or keyring) -- see src/secrets.rs.
// ---------------------------------------------------------------------------
//
// `get_secret` / `set_secret` go through the process-wide backend selected
// by `GlobalConfig::secrets_backend`. Callers must call
// `init_secrets_backend()` once at startup.

// ---------------------------------------------------------------------------
// TLS certificate-validation opt-out guard
// ---------------------------------------------------------------------------

/// True if `host` is a loopback destination: `localhost`, 127.0.0.0/8 or ::1.
pub fn is_loopback_host(host: &str) -> bool {
    let host = host.trim().trim_start_matches('[').trim_end_matches(']');
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

/// Refuse to disable TLS certificate validation for non-loopback hosts.
///
/// `accept_invalid_certs` exists for Proton Mail Bridge, which always listens
/// on loopback with a self-signed cert. For any remote host, skipping cert
/// validation hands credentials to an active man-in-the-middle, so we refuse
/// instead of connecting insecurely.
pub fn ensure_invalid_certs_allowed(host: &str) -> Result<()> {
    if is_loopback_host(host) {
        return Ok(());
    }
    Err(anyhow::anyhow!(
        "accept_invalid_certs is enabled for non-loopback host '{}'. \
         Disabling TLS certificate validation for a remote server exposes your \
         credentials to man-in-the-middle attacks, so this is only allowed for \
         loopback hosts (localhost / 127.x.x.x / ::1), e.g. Proton Mail Bridge. \
         Remove `accept_invalid_certs = true` from this account in config.toml, \
         or reach the server through a loopback tunnel.",
        host
    ))
}

pub fn get_secret(key: &str) -> Result<String> {
    crate::secrets::get(key)
}

pub fn set_secret(key: &str, value: &str) -> Result<()> {
    crate::secrets::set(key, value)
}

pub fn delete_secret(key: &str) -> Result<()> {
    crate::secrets::delete(key)
}

/// Initialize the process-wide secrets backend from the loaded config.
/// Idempotent. Logs (does not fail) if config is unreadable -- callers can
/// still operate on a default `EncryptedFile` backend.
pub fn init_secrets_backend(config: &GlobalConfig) -> std::result::Result<(), crate::secrets::SecretsError> {
    crate::secrets::init(config.secrets_backend)
}

// ---------------------------------------------------------------------------
// Config loading
// ---------------------------------------------------------------------------

/// Environment variable overriding the config directory.
///
/// Mirrors `MAILYPOPPINS_DATA_DIR`, and is what test harnesses point at a
/// tempdir instead of writing into the real `$HOME`. Setting it also disables
/// [`migrate_legacy_config_dir`]: an explicit override names a location the
/// caller chose, and must never trigger a migration side effect.
pub const CONFIG_DIR_ENV: &str = "MAILYPOPPINS_CONFIG_DIR";

fn home_dir() -> PathBuf {
    #[cfg(test)]
    if let Some(p) = test_env::home() {
        return p;
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".to_string()))
}

/// The pre-#0022 config directory, `~/.config/email`.
///
/// Only [`migrate_legacy_config_dir`] reads this. Nothing else may fall back to
/// it: a config that failed to move must fail loudly, not be quietly read from
/// the old place forever.
fn legacy_config_dir() -> PathBuf {
    home_dir().join(".config").join("email")
}

/// Return the config directory: `$MAILYPOPPINS_CONFIG_DIR`, else
/// `~/.config/mailypoppins`.
pub fn config_dir() -> PathBuf {
    if let Some(p) = config_dir_env() {
        if !p.is_empty() {
            return PathBuf::from(shellexpand::tilde(&p).into_owned());
        }
    }
    home_dir().join(".config").join("mailypoppins")
}

/// The value of `$MAILYPOPPINS_CONFIG_DIR`, through the test seam.
fn config_dir_env() -> Option<String> {
    #[cfg(test)]
    if let Some(v) = test_env::config_dir() {
        return v;
    }
    std::env::var(CONFIG_DIR_ENV).ok()
}

/// Return the path to the global config file: ~/.config/mailypoppins/config.toml
pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

/// Move a pre-#0022 `~/.config/email` directory to `~/.config/mailypoppins`,
/// once, at startup.
///
/// The "no migration paths until v1.0" invariant is scoped to data formats,
/// secret storage and wire protocols. This is a location change: not one byte
/// inside the directory is read or rewritten. A hard cut would instead cost the
/// user every stored SMTP/IMAP password and OAuth2 client id, which is a real
/// price for a cosmetic rename.
///
/// `fs::rename` is the whole operation, which is what makes it idempotent and
/// safe under two concurrent `mp` invocations: the second process either finds
/// the new directory already there and does nothing, or loses the rename race
/// and gets `ENOENT` because the old directory is already gone. Old-absent plus
/// new-present is success in both cases.
///
/// No copy fallback. Both paths sit under `~/.config`, so they are on one
/// filesystem in practice; a rename that fails anyway names both paths and the
/// exact `mv` to run.
pub fn migrate_legacy_config_dir() -> Result<()> {
    if config_dir_env().is_some() {
        return Ok(());
    }
    let old = legacy_config_dir();
    let new = config_dir();
    if new.exists() || !old.exists() {
        return Ok(());
    }
    if let Some(parent) = new.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!("Failed to create config parent directory: {}", parent.display())
        })?;
    }
    match fs::rename(&old, &new) {
        Ok(()) => {
            eprintln!(
                "{} Moved config directory {} -> {} (#0022)",
                "ℹ".blue(),
                old.display(),
                new.display(),
            );
            log::info!(
                "Moved legacy config directory {} to {}",
                old.display(),
                new.display()
            );
            warn_about_self_references(&new.join("config.toml"), &old, &new);
            Ok(())
        }
        // Lost the race with a concurrent `mp`: the other process did the move.
        Err(_) if new.is_dir() && !old.exists() => Ok(()),
        Err(e) => Err(anyhow::anyhow!(
            "Could not move the config directory {} to {}: {e}.\n\
             mailypoppins reads config and secrets from {} only. Move it by hand and re-run:\n\
             \x20 mv {} {}",
            old.display(),
            new.display(),
            new.display(),
            old.display(),
            new.display(),
        )),
    }
}

/// Point out config values that name the directory the move just emptied.
///
/// `fs::rename` moves the file but not the strings inside it, and a config may
/// well reference its own directory: a signature at
/// `~/.config/email/signatures/robin.html` resolves to nothing afterwards, and
/// [`load_signature`] answers a missing signature file with one stderr line and
/// an unsigned message. From the TUI that line goes nowhere, so the break is
/// silent, which is the only reason this warning is worth its lines.
///
/// It warns and rewrites nothing. `config.toml` is user-edited, and editing it
/// would turn a location change into a content migration of a file the user is
/// entitled to own. Warned once, here, at move time: the steady-state signal
/// stays [`load_signature`]'s own missing-file message.
fn warn_about_self_references(config_file: &Path, old_dir: &Path, new_dir: &Path) {
    let Ok(content) = fs::read_to_string(config_file) else {
        return;
    };
    // Both spellings a user could plausibly have written: the tilde form and
    // the expanded home path. Matched as a directory prefix, so an unrelated
    // string containing the words cannot trip it.
    let home = home_dir();
    let tilde = |dir: &Path| match dir.strip_prefix(&home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => dir.display().to_string(),
    };
    let prefixes = vec![
        (tilde(old_dir), tilde(new_dir)),
        (old_dir.display().to_string(), new_dir.display().to_string()),
    ];

    let hits = self_referencing_values(&content, &prefixes);
    if hits.is_empty() {
        return;
    }
    eprintln!(
        "{} {} still names the old config directory. Nothing inside it was rewritten, so these need one manual edit:",
        "⚠".yellow(),
        config_file.display(),
    );
    for (key, old_value, new_value) in &hits {
        eprintln!("    {key} = \"{old_value}\"");
        eprintln!("      -> \"{new_value}\"");
    }
    log::warn!(
        "{} references the pre-#0022 config directory in {} value(s)",
        config_file.display(),
        hits.len()
    );
}

/// Find `key = "value"` lines whose value starts with one of `prefixes`,
/// returning the dotted key path, the old value and its replacement.
///
/// Split out from [`warn_about_self_references`] so the TOML walk is testable
/// without a filesystem.
fn self_referencing_values(
    content: &str,
    prefixes: &[(String, String)],
) -> Vec<(String, String, String)> {
    let mut table = String::new();
    let mut hits = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if let Some(header) = line.strip_prefix("[[").and_then(|l| l.strip_suffix("]]")) {
            table = header.trim().to_string();
            continue;
        }
        if let Some(header) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            table = header.trim().to_string();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"');
        for (old, new) in prefixes {
            // Directory prefix, not substring: `<old>/...` or `<old>` exactly.
            let is_prefix = value == old
                || value
                    .strip_prefix(old.as_str())
                    .is_some_and(|rest| rest.starts_with('/'));
            if is_prefix {
                let key = key.trim();
                let dotted = if table.is_empty() {
                    key.to_string()
                } else {
                    format!("{table}.{key}")
                };
                hits.push((dotted, value.to_string(), value.replacen(old, new, 1)));
                break;
            }
        }
    }
    hits
}

/// Load the global config from ~/.config/mailypoppins/config.toml
pub fn load_global_config() -> Result<GlobalConfig> {
    let path = config_path();
    if !path.exists() {
        return Err(anyhow::anyhow!(
            "Config file not found at {}. Run `mp config init` to create it.",
            path.display()
        ));
    }
    let content = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read config file: {}", path.display()))?;
    reject_legacy_keys(&content, &path)?;
    let config: GlobalConfig = toml::from_str(&content)
        .with_context(|| format!("Failed to parse config file: {}", path.display()))?;
    validate_retention(&config)
        .with_context(|| format!("Invalid config file: {}", path.display()))?;
    debug!("Loaded global config from {}", path.display());
    Ok(config)
}

/// Reject out-of-range retention values at load time rather than at the first
/// eviction pass. The global table is checked on its own so a config with no
/// accounts still fails loudly.
fn validate_retention(config: &GlobalConfig) -> Result<()> {
    RetentionPolicy::resolve(&config.retention, &RetentionConfig::default())?;
    for account in &config.accounts {
        retention_for(config, account)?;
    }
    Ok(())
}

/// Refuse to parse legacy configs containing `[accounts.directories]` or
/// per-mailbox `local = "..."` keys. Per the v1.0 "no migrations" invariant,
/// fail loud and instruct the user to re-run `mp config init`.
fn reject_legacy_keys(content: &str, path: &Path) -> Result<()> {
    let mut hits: Vec<&'static str> = Vec::new();
    if content.contains("[accounts.directories]")
        || content.contains("[directories]")
    {
        hits.push("[accounts.directories] / [directories]");
    }
    // Per-mailbox `local = "..."`. Be conservative: only flag when both
    // `server = ` and `local = ` appear in the file, to avoid false positives
    // on unrelated keys.
    let local_count = content.matches("local = ").count();
    let server_count = content.matches("server = ").count();
    if local_count > 0 && server_count > 0 {
        hits.push("per-mailbox `local = \"...\"`");
    }
    if !hits.is_empty() {
        return Err(anyhow::anyhow!(
            "Config at {} uses removed keys: {}.\n\
             Mail data now lives under `mailypoppins_data_dir()` (e.g. ~/Library/Application Support/mailypoppins on macOS).\n\
             Per the v1.0 \"no migrations\" policy, please re-run `mp config init` to regenerate the config.",
            path.display(),
            hits.join(", "),
        ));
    }
    Ok(())
}

/// Build SmtpConfig from AccountConfig + secrets-backend password (or OAuth2 token)
impl SmtpConfig {
    pub fn load(account: &AccountConfig) -> Result<Self> {
        if account.auth_method == AuthMethod::Graph {
            return Err(anyhow::anyhow!(
                "Graph accounts use Microsoft Graph API, not SMTP"
            ));
        }
        let password = match account.auth_method {
            AuthMethod::OAuth2 => {
                let oauth2_settings = account.oauth2.as_ref()
                    .context("OAuth2 auth_method requires [accounts.oauth2] config")?;
                crate::oauth2::load_or_refresh_token_blocking(
                    &account.name,
                    &oauth2_settings.client_id,
                    &oauth2_settings.tenant_id,
                    crate::oauth2::IMAP_SMTP_SCOPES,
                )?
            }
            AuthMethod::Password => {
                let key = format!("smtp-password-{}", account.name);
                get_secret(&key)?
            }
            AuthMethod::Graph => unreachable!(),
        };
        Ok(Self {
            host: account.smtp.host.clone(),
            port: account.smtp.port,
            username: account.smtp.username.clone(),
            password,
            default_from: account.default_from.clone(),
            accept_invalid_certs: account.smtp.accept_invalid_certs,
            auth_method: account.auth_method.clone(),
        })
    }
}

/// Build ImapConfig from AccountConfig + secrets-backend password (or OAuth2 token, with SMTP fallback)
impl ImapConfig {
    pub fn load(account: &AccountConfig) -> Result<Self> {
        if account.auth_method == AuthMethod::Graph {
            return Err(anyhow::anyhow!(
                "Graph accounts use Microsoft Graph API, not IMAP"
            ));
        }

        // Username falls back to SMTP username if empty
        let username = if account.imap.username.is_empty() {
            account.smtp.username.clone()
        } else {
            account.imap.username.clone()
        };

        let password = match account.auth_method {
            AuthMethod::OAuth2 => {
                let oauth2_settings = account.oauth2.as_ref()
                    .context("OAuth2 auth_method requires [accounts.oauth2] config")?;
                crate::oauth2::load_or_refresh_token_blocking(
                    &account.name,
                    &oauth2_settings.client_id,
                    &oauth2_settings.tenant_id,
                    crate::oauth2::IMAP_SMTP_SCOPES,
                )?
            }
            AuthMethod::Password => {
                // Password: try imap-password first, fall back to smtp-password
                let imap_key = format!("imap-password-{}", account.name);
                let smtp_key = format!("smtp-password-{}", account.name);
                get_secret(&imap_key).or_else(|_| get_secret(&smtp_key))?
            }
            AuthMethod::Graph => unreachable!(),
        };

        // Host falls back to SMTP host if empty
        let host = if account.imap.host.is_empty() {
            account.smtp.host.clone()
        } else {
            account.imap.host.clone()
        };

        Ok(Self {
            host,
            port: account.imap.port,
            username,
            password,
            accept_invalid_certs: account.imap.accept_invalid_certs,
            auth_method: account.auth_method.clone(),
            fetch_concurrency: account.imap.fetch_concurrency.clamp(1, 8),
        })
    }
}

// ---------------------------------------------------------------------------
// App-managed data directory (replaces user-configurable [accounts.directories])
// ---------------------------------------------------------------------------
//
// Layout under <data_dir>:
//
//   accounts/<account>/{inbox,archive,sent,drafts,<extra_slug>}/
//   accounts/<account>/contacts-cache.json
//   tokens/<account>.enc
//   logs/mailypoppins-YYYY-MM-DD.log
//
// `<data_dir>` defaults to the OS-conventional app data directory:
//   - macOS:  ~/Library/Application Support/mailypoppins
//   - Linux:  $XDG_DATA_HOME/mailypoppins  (default ~/.local/share/mailypoppins)
//
// Override with the `MAILYPOPPINS_DATA_DIR` env var (for tests, or for power
// users running mailypoppins from a portable location).

/// Thread-local overrides for the process-global inputs the path resolvers
/// read (#0077).
///
/// `std::env::set_var` mutates the process environment, which every other test
/// thread is concurrently reading through `getenv` (`tempfile::tempdir()` reads
/// `$TMPDIR`, `dirs::data_dir()` reads `$HOME`, every `config::` path helper
/// read `$MAILYPOPPINS_DATA_DIR`). That is a data race on `environ` in a
/// multi-threaded process -- unsound, not merely unsynchronised -- and it is
/// what produced the one-off failures in #0077. A crate-wide mutex only
/// serialises the *writers*; the readers never took it.
///
/// A thread-local override removes the shared state instead of guarding it:
/// libtest runs each test on its own thread, so a fixture's value is invisible
/// to every other test, no lock is needed, and tests stay parallel.
#[cfg(test)]
pub(crate) mod test_env {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};

    thread_local! {
        static DATA_DIR: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
        static HOME: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
        /// `None` = no override (read the real env); `Some(v)` = override,
        /// where `v` is `None` for "explicitly unset".
        static CONFIG_DIR: RefCell<Option<Option<String>>> = const { RefCell::new(None) };
    }

    pub(crate) fn data_dir() -> Option<PathBuf> {
        DATA_DIR.with(|c| c.borrow().clone())
    }

    pub(crate) fn home() -> Option<PathBuf> {
        HOME.with(|c| c.borrow().clone())
    }

    pub(crate) fn config_dir() -> Option<Option<String>> {
        CONFIG_DIR.with(|c| c.borrow().clone())
    }

    /// Points the data dir at `path` for this thread, restoring the previous
    /// override on drop.
    pub(crate) struct DataDirOverride {
        previous: Option<PathBuf>,
    }

    impl DataDirOverride {
        pub(crate) fn set(path: impl Into<PathBuf>) -> Self {
            let previous = DATA_DIR.with(|c| c.borrow_mut().replace(path.into()));
            Self { previous }
        }
    }

    impl Drop for DataDirOverride {
        fn drop(&mut self) {
            let previous = self.previous.take();
            DATA_DIR.with(|c| *c.borrow_mut() = previous);
        }
    }

    /// A tempdir the data dir points at for the lifetime of the value.
    ///
    /// The override is dropped before the directory is removed, so nothing
    /// resolves into a tree that is going away.
    pub(crate) struct TestDataDir {
        _override: DataDirOverride,
        _dir: tempfile::TempDir,
    }

    impl TestDataDir {
        pub(crate) fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            Self {
                _override: DataDirOverride::set(dir.path()),
                _dir: dir,
            }
        }
    }

    /// Points `$HOME` at `home` and clears `$MAILYPOPPINS_CONFIG_DIR`, for
    /// this thread only.
    pub(crate) struct ConfigDirOverride {
        prev_home: Option<PathBuf>,
        prev_config: Option<Option<String>>,
    }

    impl ConfigDirOverride {
        pub(crate) fn new(home: &Path) -> Self {
            let prev_home = HOME.with(|c| c.borrow_mut().replace(home.to_path_buf()));
            let prev_config = CONFIG_DIR.with(|c| c.borrow_mut().replace(None));
            Self {
                prev_home,
                prev_config,
            }
        }

        /// Set `$MAILYPOPPINS_CONFIG_DIR` to `value` for this thread.
        pub(crate) fn set_config_dir(&self, value: &Path) {
            let value = value.to_string_lossy().into_owned();
            CONFIG_DIR.with(|c| *c.borrow_mut() = Some(Some(value)));
        }
    }

    impl Drop for ConfigDirOverride {
        fn drop(&mut self) {
            let prev_home = self.prev_home.take();
            let prev_config = self.prev_config.clone();
            HOME.with(|c| *c.borrow_mut() = prev_home);
            CONFIG_DIR.with(|c| *c.borrow_mut() = prev_config);
        }
    }
}

/// Root data directory for all app-owned files.
pub fn mailypoppins_data_dir() -> PathBuf {
    #[cfg(test)]
    if let Some(p) = test_env::data_dir() {
        return p;
    }
    if let Ok(p) = std::env::var("MAILYPOPPINS_DATA_DIR") {
        if !p.is_empty() {
            return PathBuf::from(shellexpand::tilde(&p).into_owned());
        }
    }
    dirs::data_dir()
        .map(|d| d.join("mailypoppins"))
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join(".local/share/mailypoppins")
        })
}

/// `<data_dir>/accounts/<account_name>/`
pub fn account_dir(account_name: &str) -> PathBuf {
    mailypoppins_data_dir().join("accounts").join(account_name)
}

/// `<account_dir>/drafts/`
pub fn drafts_dir(account_name: &str) -> PathBuf {
    account_dir(account_name).join("drafts")
}

/// `<account_dir>/contacts-cache.json`
pub fn contacts_cache_path(account_name: &str) -> PathBuf {
    account_dir(account_name).join("contacts-cache.json")
}

/// `<account_dir>/store.sqlite3`, the per-account SQLite store (see
/// `src/store/`).
pub fn store_path(account_name: &str) -> PathBuf {
    account_dir(account_name).join("store.sqlite3")
}

/// `<account_dir>/blobs/`, the content-addressed blob store root.
pub fn blobs_dir(account_name: &str) -> PathBuf {
    account_dir(account_name).join("blobs")
}

/// `<data_dir>/tokens/`
pub fn tokens_dir() -> PathBuf {
    mailypoppins_data_dir().join("tokens")
}

/// `<data_dir>/logs/`
pub fn logs_dir() -> PathBuf {
    mailypoppins_data_dir().join("logs")
}

/// Newest log file in `logs_dir()`, by filename. Daily files are named
/// `mailypoppins-YYYY-MM-DD.log` (see `init_logging`), so lexicographic
/// order equals date order. `None` when the directory is missing or
/// contains no matching file.
pub fn latest_log_file() -> Option<PathBuf> {
    let entries = fs::read_dir(logs_dir()).ok()?;
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("mailypoppins-") && n.ends_with(".log"))
                .unwrap_or(false)
        })
        .max()
}

/// True when the account's *server* files a copy of every submitted message in
/// Sent, so a client-side APPEND would duplicate it.
///
/// Three families do this, and they are recognised by what the config already
/// carries rather than by a new setting:
///
/// - Microsoft Graph, where `sendMail` writes to Sent Items as part of the API
///   call (recognised by `auth_method = "graph"`);
/// - Gmail, which files SMTP submissions in `[Gmail]/Sent Mail` (recognised by
///   the `gmail.com` / `googlemail.com` SMTP or IMAP host);
/// - Proton, whose Bridge does the same for the SMTP it exposes (recognised by
///   a `proton` hostname; a Bridge configured against bare `127.0.0.1` cannot
///   be told apart from any other local relay and needs
///   `save_to_sent = "never"`).
pub fn server_saves_to_sent(account: &AccountConfig) -> bool {
    if account.auth_method == AuthMethod::Graph {
        return true;
    }
    let hosts = [account.smtp.host.as_str(), account.imap.host.as_str()];
    hosts.iter().any(|host| {
        let host = host.to_ascii_lowercase();
        host.ends_with("gmail.com")
            || host.ends_with("googlemail.com")
            || host.contains("protonmail")
            || host.contains("proton.me")
    })
}

/// Whether this client should APPEND its own copy of a sent message to the
/// Sent mailbox: the `save_to_sent` setting, with `auto` resolved through
/// [`server_saves_to_sent`].
pub fn appends_to_sent(account: &AccountConfig) -> bool {
    match account.save_to_sent {
        SaveToSent::Always => true,
        SaveToSent::Never => false,
        SaveToSent::Auto => !server_saves_to_sent(account),
    }
}

/// Resolve the sent mailbox server name from config
pub fn resolve_sent_mailbox(account: &AccountConfig) -> String {
    account
        .mailboxes
        .sent
        .as_ref()
        .map(|m| m.server.clone())
        .unwrap_or_else(|| "Sent".to_string())
}

/// Find a mailbox mapping by role name or server name.
///
/// The name is read as a [`MailboxRole`] first, so `inbox`, `INBOX` and
/// `Inbox` all resolve to the configured inbox mapping; anything else is
/// matched against the server names, case-insensitively.
fn find_mailbox_mapping<'a>(account: &'a AccountConfig, mailbox: &str) -> Option<&'a MailboxMapping> {
    let role = MailboxRole::from(mailbox);
    let named = match role {
        MailboxRole::Inbox => account.mailboxes.inbox.as_ref(),
        MailboxRole::Archive => account.mailboxes.archive.as_ref(),
        MailboxRole::Sent => account.mailboxes.sent.as_ref(),
        MailboxRole::Other(_) => None,
    };
    if let Some(m) = named {
        return Some(m);
    }
    // Not a role name (or the role is not configured): match server names.
    let by_server = [
        account.mailboxes.inbox.as_ref(),
        account.mailboxes.archive.as_ref(),
        account.mailboxes.sent.as_ref(),
    ];
    for m in by_server.into_iter().flatten() {
        if m.server.eq_ignore_ascii_case(mailbox) {
            return Some(m);
        }
    }
    if let Some(ref extras) = account.mailboxes.extra {
        for m in extras {
            if m.server.eq_ignore_ascii_case(mailbox) {
                return Some(m);
            }
        }
    }
    None
}

/// Return all configured mailboxes: (role, mapping).
///
/// The role is the key ingest writes into `messages.mailbox` and the segment
/// selectors carry, so an unmapped mailbox comes back as
/// [`MailboxRole::Other`] holding its server name verbatim.
pub fn all_configured_mailboxes(account: &AccountConfig) -> Vec<(MailboxRole, &MailboxMapping)> {
    let mut result = Vec::new();
    if let Some(ref m) = account.mailboxes.inbox {
        result.push((MailboxRole::Inbox, m));
    }
    if let Some(ref m) = account.mailboxes.archive {
        result.push((MailboxRole::Archive, m));
    }
    if let Some(ref m) = account.mailboxes.sent {
        result.push((MailboxRole::Sent, m));
    }
    if let Some(ref extras) = account.mailboxes.extra {
        for m in extras {
            result.push((MailboxRole::Other(m.server.clone()), m));
        }
    }
    result
}

/// Given a user-specified mailbox name (which might be a role like "inbox" or
/// a server name like "INBOX"), return the actual IMAP server name from config.
pub fn find_server_name_for_role(account: &AccountConfig, name: &str) -> String {
    if let Some(mapping) = find_mailbox_mapping(account, name) {
        mapping.server.clone()
    } else {
        name.to_string()
    }
}

/// Resolve a user-typed mailbox name to the pair a sync target is made of: the
/// store key its rows are filed under, and the name to SELECT on the server.
///
/// Both halves come from the same configured mapping, which is the point
/// (#0064 review note 2). Building the role straight from the typed string
/// gives `Other("projects")` for a mailbox configured as `Projects`, so the
/// pass ingests under a key the sidebar never lists and the selectors never
/// resolve, while the server name resolves case-insensitively and the fetch
/// succeeds: rows land in a mailbox that does not exist locally.
///
/// `None` means the name matches no configured mailbox, which the caller
/// reports rather than syncing a mailbox the rest of the product cannot see.
pub fn find_sync_target(account: &AccountConfig, name: &str) -> Option<(MailboxRole, String)> {
    let requested = MailboxRole::from(name);
    all_configured_mailboxes(account)
        .into_iter()
        .find(|(role, mapping)| *role == requested || mapping.server.eq_ignore_ascii_case(name))
        .map(|(role, mapping)| (role, mapping.server.clone()))
}

/// Find the AccountConfig whose default_from matches the given from address.
pub fn find_account_by_from<'a>(config: &'a GlobalConfig, from: &str) -> Option<&'a AccountConfig> {
    let lower = from.to_lowercase();
    config.accounts.iter().find(|a| {
        lower.contains(&a.default_from.to_lowercase())
    })
}

/// Return the first (default) account, or None if no accounts are configured.
pub fn default_account(config: &GlobalConfig) -> Option<&AccountConfig> {
    config.accounts.first()
}

// ---------------------------------------------------------------------------
// Logging (unchanged)
// ---------------------------------------------------------------------------

/// Initialize file-based logging to `<data_dir>/logs/mailypoppins-YYYY-MM-DD.log`.
/// Non-fatal: prints a warning and continues if setup fails.
pub fn init_logging() {
    let log_dir = logs_dir();
    if let Err(e) = fs::create_dir_all(&log_dir) {
        eprintln!(
            "{} Could not create log directory {}: {}",
            "⚠".yellow(),
            log_dir.display(),
            e
        );
        return;
    }

    let filename = format!("mailypoppins-{}.log", Utc::now().format("%Y-%m-%d"));
    let log_path = log_dir.join(filename);

    let log_file = match fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "{} Could not open log file {}: {}",
                "⚠".yellow(),
                log_path.display(),
                e
            );
            return;
        }
    };

    // Custom log timestamp format with millisecond precision. Using local
    // time when available (best for human debugging); falls back to UTC if
    // the local offset cannot be determined safely (e.g. multithreaded env).
    let mut builder = ConfigBuilder::new();
    builder.set_time_format_custom(format_description!(
        "[year]-[month]-[day] [hour]:[minute]:[second].[subsecond digits:3]"
    ));
    let _ = builder.set_time_offset_to_local();
    let log_config = builder.build();

    if let Err(e) = CombinedLogger::init(vec![WriteLogger::new(
        LevelFilter::Debug,
        log_config,
        log_file,
    )]) {
        eprintln!("{} Could not initialize logger: {}", "⚠".yellow(), e);
    }
}

// ---------------------------------------------------------------------------
// Signature loading
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_minimal_config() {
        let toml_str = r#"
[[accounts]]
name = "test"
default_from = "user@example.com"

[accounts.smtp]
host = "smtp.example.com"
username = "user"
"#;
        let config: GlobalConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.accounts.len(), 1);
        assert_eq!(config.accounts[0].name, "test");
        assert_eq!(config.accounts[0].default_from, "user@example.com");
    }

    #[test]
    fn test_default_ports() {
        let toml_str = r#"
[[accounts]]
name = "test"

[accounts.smtp]
host = "smtp.example.com"

[accounts.imap]
host = "imap.example.com"
"#;
        let config: GlobalConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.accounts[0].smtp.port, 465);
        assert_eq!(config.accounts[0].imap.port, 993);
    }

    #[test]
    fn test_imap_username_fallback_to_smtp() {
        let account = AccountConfig {
            name: "test".to_string(),
            smtp: SmtpSettings {
                host: "smtp.example.com".to_string(),
                username: "smtp_user".to_string(),
                ..Default::default()
            },
            imap: ImapSettings {
                host: "imap.example.com".to_string(),
                username: "".to_string(), // empty -> should fallback
                ..Default::default()
            },
            ..Default::default()
        };
        // ImapConfig::load needs the secrets backend; test the logic inline
        let username = if account.imap.username.is_empty() {
            account.smtp.username.clone()
        } else {
            account.imap.username.clone()
        };
        assert_eq!(username, "smtp_user");
    }

    #[test]
    fn test_imap_host_fallback_to_smtp() {
        let account = AccountConfig {
            smtp: SmtpSettings {
                host: "smtp.example.com".to_string(),
                ..Default::default()
            },
            imap: ImapSettings {
                host: "".to_string(), // empty -> should fallback
                ..Default::default()
            },
            ..Default::default()
        };
        let host = if account.imap.host.is_empty() {
            account.smtp.host.clone()
        } else {
            account.imap.host.clone()
        };
        assert_eq!(host, "smtp.example.com");
    }

    // -----------------------------------------------------------------------
    // AccountConfig::is_local_only (#0071 review follow-up)
    // -----------------------------------------------------------------------

    #[test]
    fn an_account_without_any_host_is_local_only() {
        let account = AccountConfig {
            name: "drafts-only".to_string(),
            ..Default::default()
        };
        assert!(account.is_local_only());
    }

    /// The SMTP host is what `ImapConfig::load` falls back to, so an account
    /// that only names it still has a remote source.
    #[test]
    fn either_host_makes_an_account_remote() {
        let imap_only = AccountConfig {
            imap: ImapSettings {
                host: "imap.example.com".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(!imap_only.is_local_only());

        let smtp_only = AccountConfig {
            smtp: SmtpSettings {
                host: "smtp.example.com".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(!smtp_only.is_local_only());
    }

    /// A Graph account has no IMAP host by construction; skipping it as
    /// local-only would silence the one transport it does have.
    #[test]
    fn a_graph_account_is_never_local_only() {
        let account = AccountConfig {
            auth_method: AuthMethod::Graph,
            ..Default::default()
        };
        assert!(!account.is_local_only());
    }

    #[test]
    fn test_find_account_by_from_match() {
        let config = GlobalConfig {
            accounts: vec![AccountConfig {
                name: "personal".to_string(),
                default_from: "alice@example.com".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let found = find_account_by_from(&config, "Alice <alice@example.com>");
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "personal");
    }

    #[test]
    fn test_find_account_by_from_no_match() {
        let config = GlobalConfig {
            accounts: vec![AccountConfig {
                default_from: "alice@example.com".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(find_account_by_from(&config, "bob@other.com").is_none());
    }

    #[test]
    fn test_find_account_by_from_case_insensitive() {
        let config = GlobalConfig {
            accounts: vec![AccountConfig {
                default_from: "Alice@Example.COM".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(find_account_by_from(&config, "alice@example.com").is_some());
    }

    #[test]
    fn test_resolve_sent_mailbox_configured() {
        let account = AccountConfig {
            mailboxes: MailboxesConfig {
                sent: Some(MailboxMapping {
                    server: "Sent Items".to_string(),
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(resolve_sent_mailbox(&account), "Sent Items");
    }

    #[test]
    fn test_resolve_sent_mailbox_default() {
        let account = AccountConfig::default();
        assert_eq!(resolve_sent_mailbox(&account), "Sent");
    }

    #[test]
    fn test_all_configured_mailboxes_with_extras() {
        let account = AccountConfig {
            mailboxes: MailboxesConfig {
                inbox: Some(MailboxMapping {
                    server: "INBOX".to_string(),
                }),
                archive: Some(MailboxMapping {
                    server: "Archive".to_string(),
                }),
                sent: None,
                extra: Some(vec![MailboxMapping {
                    server: "Spam".to_string(),
                }]),
            },
            ..Default::default()
        };
        let all = all_configured_mailboxes(&account);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].0, MailboxRole::Inbox);
        assert_eq!(all[1].0, MailboxRole::Archive);
        assert_eq!(all[2].0, MailboxRole::Other("Spam".to_string()));
    }

    #[test]
    fn test_all_configured_mailboxes_empty() {
        let account = AccountConfig::default();
        let all = all_configured_mailboxes(&account);
        assert!(all.is_empty());
    }

    #[test]
    fn test_parse_config_with_theme() {
        let toml_str = r#"
theme = "tokyo-night"

[[accounts]]
name = "test"
"#;
        let config: GlobalConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.theme, "tokyo-night");
    }

    #[test]
    fn test_theme_defaults_to_empty() {
        let toml_str = r#"
[[accounts]]
name = "test"
"#;
        let config: GlobalConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.theme, "");
    }

    #[test]
    fn test_parse_config_with_notifications() {
        let toml_str = r#"
notifications = true

[[accounts]]
name = "test"
"#;
        let config: GlobalConfig = toml::from_str(toml_str).unwrap();
        assert!(config.notifications);
    }

    #[test]
    fn test_notifications_default_off() {
        let toml_str = r#"
[[accounts]]
name = "test"
"#;
        let config: GlobalConfig = toml::from_str(toml_str).unwrap();
        assert!(!config.notifications);
    }

    #[test]
    fn test_email_settings_defaults() {
        let settings = EmailSettings::default();
        assert_eq!(settings.font_family, "Helvetica, Arial, sans-serif");
        assert_eq!(settings.font_size, "12pt");
        assert!(settings.include_signature);
    }

    // -----------------------------------------------------------------------
    // find_server_name_for_role
    // -----------------------------------------------------------------------

    #[test]
    fn test_find_server_name_for_role_mapped() {
        let account = AccountConfig {
            mailboxes: MailboxesConfig {
                inbox: Some(MailboxMapping {
                    server: "INBOX".to_string(),
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(find_server_name_for_role(&account, "inbox"), "INBOX");
    }

    #[test]
    fn test_find_server_name_for_role_unmapped() {
        let account = AccountConfig::default();
        // Unknown role falls through to the name itself
        assert_eq!(find_server_name_for_role(&account, "Junk"), "Junk");
    }

    // -----------------------------------------------------------------------
    // find_sync_target (#0064 review note 2)
    // -----------------------------------------------------------------------

    fn account_with_extra() -> AccountConfig {
        AccountConfig {
            mailboxes: MailboxesConfig {
                inbox: Some(MailboxMapping {
                    server: "INBOX".to_string(),
                }),
                extra: Some(vec![MailboxMapping {
                    server: "Projects".to_string(),
                }]),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// `mp sync --mailbox projects` must file its rows under the key the
    /// sidebar lists, `Projects`, not under the string the user typed.
    #[test]
    fn a_sync_target_takes_its_key_from_the_configured_mailbox() {
        let account = account_with_extra();
        assert_eq!(
            find_sync_target(&account, "projects"),
            Some((
                MailboxRole::Other("Projects".to_string()),
                "Projects".to_string()
            ))
        );
        assert_eq!(
            find_sync_target(&account, "PROJECTS"),
            Some((
                MailboxRole::Other("Projects".to_string()),
                "Projects".to_string()
            ))
        );
    }

    /// A role name and the server name behind it are the same target.
    #[test]
    fn a_role_name_and_its_server_name_resolve_to_one_target() {
        let account = account_with_extra();
        let expected = Some((MailboxRole::Inbox, "INBOX".to_string()));
        assert_eq!(find_sync_target(&account, "inbox"), expected);
        assert_eq!(find_sync_target(&account, "INBOX"), expected);
        assert_eq!(find_sync_target(&account, "Inbox"), expected);
    }

    /// A name nothing is configured for has no key to file rows under, so the
    /// caller reports it instead of inventing one.
    #[test]
    fn an_unconfigured_mailbox_has_no_sync_target() {
        assert_eq!(find_sync_target(&account_with_extra(), "Junk"), None);
        assert_eq!(find_sync_target(&AccountConfig::default(), "inbox"), None);
    }

    // -----------------------------------------------------------------------
    // find_account_by_from edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_find_account_by_from_multiple_accounts() {
        let config = GlobalConfig {
            accounts: vec![
                AccountConfig {
                    name: "work".to_string(),
                    default_from: "alice@work.com".to_string(),
                    ..Default::default()
                },
                AccountConfig {
                    name: "personal".to_string(),
                    default_from: "alice@home.com".to_string(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let found = find_account_by_from(&config, "alice@home.com");
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "personal");
    }

    #[test]
    fn test_find_account_by_from_with_display_name() {
        let config = GlobalConfig {
            accounts: vec![AccountConfig {
                name: "test".to_string(),
                default_from: "alice@example.com".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        // Should match even when wrapped in a display name
        assert!(find_account_by_from(&config, "Alice Smith <alice@example.com>").is_some());
    }

    // -----------------------------------------------------------------------
    // config_dir + the one-time #0022 legacy move
    //
    // These override HOME and MAILYPOPPINS_CONFIG_DIR through the thread-local
    // test seam rather than the process environment (#0077).
    // -----------------------------------------------------------------------

    use test_env::ConfigDirOverride as ConfigEnv;

    fn seed_legacy_dir(home: &Path) -> PathBuf {
        let legacy = home.join(".config").join("email");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("config.toml"), "[[accounts]]\nname = \"a\"\n").unwrap();
        fs::write(legacy.join("secrets.enc"), b"cipher").unwrap();
        legacy
    }

    /// An explicit override names a location the caller chose: it must never
    /// pull a directory out from under another install.
    #[test]
    fn legacy_move_is_skipped_when_the_override_is_set() {
        let tmp = tempfile::tempdir().unwrap();
        let env = ConfigEnv::new(tmp.path());
        let legacy = seed_legacy_dir(tmp.path());
        let elsewhere = tmp.path().join("chosen");
        env.set_config_dir(&elsewhere);

        migrate_legacy_config_dir().unwrap();

        assert!(legacy.join("config.toml").exists(), "legacy dir was moved");
        assert!(!elsewhere.exists(), "override dir was created");
        assert_eq!(config_path(), elsewhere.join("config.toml"));
    }

    /// A pre-existing new directory is truth. The move must not clobber it and
    /// must not merge into it.
    #[test]
    fn legacy_move_never_overwrites_an_existing_config_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let _env = ConfigEnv::new(tmp.path());
        let legacy = seed_legacy_dir(tmp.path());
        let current = tmp.path().join(".config").join("mailypoppins");
        fs::create_dir_all(&current).unwrap();
        fs::write(current.join("config.toml"), "[[accounts]]\nname = \"kept\"\n").unwrap();

        migrate_legacy_config_dir().unwrap();

        assert_eq!(
            fs::read_to_string(current.join("config.toml")).unwrap(),
            "[[accounts]]\nname = \"kept\"\n"
        );
        assert!(legacy.join("config.toml").exists(), "legacy dir was consumed");
    }

    /// Nothing reads the old location. A config that failed to move must fail
    /// loudly rather than be served from `~/.config/email` forever.
    #[test]
    fn config_and_secrets_paths_never_resolve_into_the_legacy_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let _env = ConfigEnv::new(tmp.path());
        seed_legacy_dir(tmp.path());

        // Deliberately no migrate call: the legacy dir is the only one present.
        let expected = tmp.path().join(".config").join("mailypoppins");
        assert_eq!(config_dir(), expected);
        assert_eq!(config_path(), expected.join("config.toml"));
        assert_eq!(
            crate::secrets::secrets_path(),
            expected.join("secrets.enc")
        );
    }

    /// The gap `fs::rename` leaves: the file moves, the strings inside it do
    /// not. A signature path into the old directory becomes an unsigned
    /// message with only an easily-missed stderr line behind it.
    #[test]
    fn self_reference_scan_names_the_key_the_old_value_and_the_replacement() {
        let prefixes = vec![
            (
                "~/.config/email".to_string(),
                "~/.config/mailypoppins".to_string(),
            ),
            (
                "/home/u/.config/email".to_string(),
                "/home/u/.config/mailypoppins".to_string(),
            ),
        ];
        let toml = r#"
[[accounts]]
name = "work"

[accounts.signatures.robin]
path = "~/.config/email/signatures/robin.html"

[accounts.signatures.plain]
path = "/home/u/.config/email/signatures/plain.html"
"#;
        let hits = self_referencing_values(toml, &prefixes);
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert_eq!(hits[0].0, "accounts.signatures.robin.path");
        assert_eq!(hits[0].1, "~/.config/email/signatures/robin.html");
        assert_eq!(hits[0].2, "~/.config/mailypoppins/signatures/robin.html");
        assert_eq!(hits[1].0, "accounts.signatures.plain.path");
        assert_eq!(hits[1].2, "/home/u/.config/mailypoppins/signatures/plain.html");
    }

    /// Directory prefix, not substring: a value that merely contains the words
    /// is not a stale reference, and neither is a longer sibling directory.
    #[test]
    fn self_reference_scan_does_not_false_positive() {
        let prefixes = vec![(
            "~/.config/email".to_string(),
            "~/.config/mailypoppins".to_string(),
        )];
        let toml = r#"
[[accounts]]
default_from = "me@example.com"
note = "my ~/.config/email-archive/notes"
other = "/srv/.config/email/thing"
"#;
        assert!(self_referencing_values(toml, &prefixes).is_empty());
    }

    #[test]
    fn legacy_move_relocates_the_whole_directory_once() {
        let tmp = tempfile::tempdir().unwrap();
        let _env = ConfigEnv::new(tmp.path());
        let legacy = seed_legacy_dir(tmp.path());

        migrate_legacy_config_dir().unwrap();

        let current = tmp.path().join(".config").join("mailypoppins");
        assert!(!legacy.exists(), "legacy dir survived the move");
        assert_eq!(
            fs::read_to_string(current.join("config.toml")).unwrap(),
            "[[accounts]]\nname = \"a\"\n"
        );
        assert_eq!(fs::read(current.join("secrets.enc")).unwrap(), b"cipher");

        // Idempotent: a second pass, and every later run, is a no-op.
        migrate_legacy_config_dir().unwrap();
        assert!(current.join("config.toml").exists());
    }

    /// What a process that lost the rename race sees: old gone, new there.
    #[test]
    fn legacy_move_is_a_no_op_with_nothing_to_move() {
        let tmp = tempfile::tempdir().unwrap();
        let _env = ConfigEnv::new(tmp.path());

        // Neither directory exists: no config yet, nothing created.
        migrate_legacy_config_dir().unwrap();
        assert!(!tmp.path().join(".config").join("mailypoppins").exists());

        // Only the new one exists.
        let current = tmp.path().join(".config").join("mailypoppins");
        fs::create_dir_all(&current).unwrap();
        migrate_legacy_config_dir().unwrap();
        assert!(current.exists());
    }

    // -----------------------------------------------------------------------
    // mailypoppins_data_dir + derived path helpers
    // -----------------------------------------------------------------------

    #[test]
    fn data_dir_env_override() {
        let _o = test_env::DataDirOverride::set("/tmp/mailypoppins-test");
        assert_eq!(
            mailypoppins_data_dir(),
            PathBuf::from("/tmp/mailypoppins-test")
        );
    }

    #[test]
    fn account_dir_layout() {
        let _o = test_env::DataDirOverride::set("/tmp/x");
        assert_eq!(account_dir("alice"), PathBuf::from("/tmp/x/accounts/alice"));
        assert_eq!(
            drafts_dir("alice"),
            PathBuf::from("/tmp/x/accounts/alice/drafts")
        );
        assert_eq!(
            contacts_cache_path("alice"),
            PathBuf::from("/tmp/x/accounts/alice/contacts-cache.json")
        );
        assert_eq!(
            store_path("alice"),
            PathBuf::from("/tmp/x/accounts/alice/store.sqlite3")
        );
        assert_eq!(
            blobs_dir("alice"),
            PathBuf::from("/tmp/x/accounts/alice/blobs")
        );
        assert_eq!(tokens_dir(), PathBuf::from("/tmp/x/tokens"));
        assert_eq!(logs_dir(), PathBuf::from("/tmp/x/logs"));
    }

    #[test]
    fn latest_log_file_picks_newest_and_handles_missing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp = tmp.path().join("data");
        let _o = test_env::DataDirOverride::set(&tmp);

        // Missing logs dir -> None, no crash.
        assert_eq!(latest_log_file(), None);

        // Empty logs dir -> None.
        fs::create_dir_all(logs_dir()).unwrap();
        assert_eq!(latest_log_file(), None);

        // Newest daily file wins; non-matching files are ignored.
        fs::write(logs_dir().join("mailypoppins-2026-05-01.log"), "").unwrap();
        fs::write(logs_dir().join("mailypoppins-2026-05-03.log"), "").unwrap();
        fs::write(logs_dir().join("mailypoppins-2026-05-02.log"), "").unwrap();
        fs::write(logs_dir().join("unrelated.txt"), "").unwrap();
        assert_eq!(
            latest_log_file(),
            Some(logs_dir().join("mailypoppins-2026-05-03.log"))
        );
    }

    #[test]
    fn reject_legacy_directories_block() {
        let toml = r#"
[[accounts]]
name = "x"

[accounts.directories]
root = "~/notes/email"
"#;
        let path = std::path::PathBuf::from("/tmp/fake.toml");
        let err = reject_legacy_keys(toml, &path).unwrap_err();
        assert!(err.to_string().contains("directories"));
    }

    #[test]
    fn reject_legacy_local_field() {
        let toml = r#"
[[accounts]]
name = "x"

[accounts.mailboxes.inbox]
server = "INBOX"
local = "inbox"
"#;
        let path = std::path::PathBuf::from("/tmp/fake.toml");
        let err = reject_legacy_keys(toml, &path).unwrap_err();
        assert!(err.to_string().contains("local"));
    }

    #[test]
    fn reject_legacy_passes_clean_config() {
        let toml = r#"
[[accounts]]
name = "x"

[accounts.smtp]
host = "smtp.example.com"

[accounts.mailboxes.inbox]
server = "INBOX"
"#;
        let path = std::path::PathBuf::from("/tmp/fake.toml");
        assert!(reject_legacy_keys(toml, &path).is_ok());
    }

    // -----------------------------------------------------------------------
    // Config deserialization edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_config_with_custom_email_settings() {
        let toml_str = r#"
[email]
font_family = "Georgia, serif"
font_size = "14px"
include_signature = false

[[accounts]]
name = "test"

[accounts.smtp]
host = "smtp.example.com"
"#;
        let config: GlobalConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.email.font_family, "Georgia, serif");
        assert_eq!(config.email.font_size, "14px");
        assert!(!config.email.include_signature);
    }

    #[test]
    fn test_parse_config_with_extra_mailboxes() {
        let toml_str = r#"
[[accounts]]
name = "test"

[accounts.smtp]
host = "smtp.example.com"

[accounts.mailboxes.inbox]
server = "INBOX"

[[accounts.mailboxes.extra]]
server = "Spam"

[[accounts.mailboxes.extra]]
server = "Newsletters"
"#;
        let config: GlobalConfig = toml::from_str(toml_str).unwrap();
        let extras = config.accounts[0].mailboxes.extra.as_ref().unwrap();
        assert_eq!(extras.len(), 2);
        assert_eq!(extras[0].server, "Spam");
        assert_eq!(extras[1].server, "Newsletters");
    }

    #[test]
    fn test_default_account() {
        let config = GlobalConfig {
            accounts: vec![
                AccountConfig { name: "first".to_string(), ..Default::default() },
                AccountConfig { name: "second".to_string(), ..Default::default() },
            ],
            ..Default::default()
        };
        assert_eq!(default_account(&config).unwrap().name, "first");
    }

    #[test]
    fn test_default_account_empty() {
        let config = GlobalConfig::default();
        assert!(default_account(&config).is_none());
    }

    // -----------------------------------------------------------------
    // Retention config
    // -----------------------------------------------------------------

    #[test]
    fn retention_absent_section_yields_documented_defaults() {
        let toml_str = r#"
[[accounts]]
name = "test"

[accounts.smtp]
host = "smtp.example.com"
"#;
        let config: GlobalConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.retention, RetentionConfig::default());

        let policy = retention_for(&config, &config.accounts[0]).unwrap();
        assert_eq!(policy, RetentionPolicy::default());
        assert_eq!(policy.metadata_horizon_days, 0, "metadata defaults to keep-all");
        assert_eq!(policy.body_horizon_days, 365);
        assert_eq!(policy.attachment_horizon_days, 90);
        assert_eq!(policy.max_disk_bytes, 5_000_000_000);
    }

    #[test]
    fn retention_every_field_round_trips() {
        let toml_str = r#"
[retention]
metadata_horizon_days = 3650
body_horizon_days = 180
attachment_horizon_days = 30
max_disk_bytes = 2000000000

[[accounts]]
name = "test"
"#;
        let config: GlobalConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.retention.metadata_horizon_days, Some(3650));
        assert_eq!(config.retention.body_horizon_days, Some(180));
        assert_eq!(config.retention.attachment_horizon_days, Some(30));
        assert_eq!(config.retention.max_disk_bytes, Some(2_000_000_000));

        let policy = retention_for(&config, &config.accounts[0]).unwrap();
        assert_eq!(
            policy,
            RetentionPolicy {
                metadata_horizon_days: 3650,
                body_horizon_days: 180,
                attachment_horizon_days: 30,
                max_disk_bytes: 2_000_000_000,
            }
        );
    }

    #[test]
    fn retention_account_overrides_the_global_field_by_field() {
        let toml_str = r#"
[retention]
body_horizon_days = 180
max_disk_bytes = 2000000000

[[accounts]]
name = "inherits"

[[accounts]]
name = "overrides"

[accounts.retention]
attachment_horizon_days = 7
max_disk_bytes = 500000000
"#;
        let config: GlobalConfig = toml::from_str(toml_str).unwrap();

        let inherits = retention_for(&config, &config.accounts[0]).unwrap();
        assert_eq!(inherits.body_horizon_days, 180);
        assert_eq!(inherits.attachment_horizon_days, 90, "falls back to the default");
        assert_eq!(inherits.max_disk_bytes, 2_000_000_000);

        let overrides = retention_for(&config, &config.accounts[1]).unwrap();
        assert_eq!(overrides.attachment_horizon_days, 7);
        assert_eq!(overrides.max_disk_bytes, 500_000_000);
        assert_eq!(
            overrides.body_horizon_days, 180,
            "unset account fields keep the global value"
        );
        assert_eq!(overrides.metadata_horizon_days, 0);
    }

    #[test]
    fn retention_zero_horizon_means_keep_all() {
        let toml_str = r#"
[retention]
body_horizon_days = 0
attachment_horizon_days = 0

[[accounts]]
name = "test"
"#;
        let config: GlobalConfig = toml::from_str(toml_str).unwrap();
        let policy = retention_for(&config, &config.accounts[0]).unwrap();
        assert_eq!(policy.body_horizon_days, 0);
        assert_eq!(policy.attachment_horizon_days, 0);
    }

    #[test]
    fn retention_out_of_range_horizon_is_rejected_clearly() {
        let toml_str = r#"
[retention]
body_horizon_days = 40000

[[accounts]]
name = "test"
"#;
        let config: GlobalConfig = toml::from_str(toml_str).unwrap();
        let err = validate_retention(&config).unwrap_err().to_string();
        assert!(err.contains("body_horizon_days"), "missing field name: {err}");
        assert!(err.contains("40000"), "missing offending value: {err}");
        assert!(err.contains("36500"), "missing allowed range: {err}");
    }

    #[test]
    fn retention_out_of_range_disk_budget_is_rejected_clearly() {
        for value in ["1000", "9000000000000"] {
            let toml_str = format!(
                "[[accounts]]\nname = \"test\"\n\n[accounts.retention]\nmax_disk_bytes = {value}\n"
            );
            let config: GlobalConfig = toml::from_str(&toml_str).unwrap();
            let err = validate_retention(&config).unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains("max_disk_bytes"), "missing field name: {msg}");
            assert!(msg.contains(value), "missing offending value: {msg}");
            assert!(msg.contains("test"), "missing account name: {msg}");
        }
    }

    #[test]
    fn retention_negative_value_is_rejected_at_parse_time() {
        let toml_str = r#"
[retention]
body_horizon_days = -1
"#;
        let err = toml::from_str::<GlobalConfig>(toml_str).unwrap_err().to_string();
        assert!(
            err.contains("body_horizon_days"),
            "parse error should name the field: {err}"
        );
    }

    #[test]
    fn retention_global_is_validated_without_any_account() {
        let toml_str = "[retention]\nmax_disk_bytes = 1\n";
        let config: GlobalConfig = toml::from_str(toml_str).unwrap();
        assert!(config.accounts.is_empty());
        assert!(validate_retention(&config).is_err());
    }

    #[test]
    fn test_is_loopback_host() {
        // Loopback: localhost, 127.0.0.0/8, ::1
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("LOCALHOST"));
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("127.1.2.3"));
        assert!(is_loopback_host("::1"));
        assert!(is_loopback_host("[::1]"));
        assert!(is_loopback_host(" localhost "));

        // Not loopback
        assert!(!is_loopback_host("smtp.example.com"));
        assert!(!is_loopback_host("192.168.1.10"));
        assert!(!is_loopback_host("128.0.0.1"));
        assert!(!is_loopback_host("::2"));
        assert!(!is_loopback_host("localhost.evil.com"));
        assert!(!is_loopback_host(""));
    }

    #[test]
    fn test_ensure_invalid_certs_allowed() {
        assert!(ensure_invalid_certs_allowed("127.0.0.1").is_ok());
        assert!(ensure_invalid_certs_allowed("localhost").is_ok());
        let err = ensure_invalid_certs_allowed("imap.example.com").unwrap_err();
        assert!(err.to_string().contains("accept_invalid_certs"));
        assert!(err.to_string().contains("imap.example.com"));
    }

}

/// Load signature HTML content
pub fn load_signature(account: &AccountConfig, signature_name: Option<&str>) -> Option<String> {
    let sig_name = signature_name
        .map(|s| s.to_string())
        .or_else(|| account.signatures.default.clone())?;

    let entry = account.signatures.entries.get(&sig_name)?;

    // Expand ~ in signature path
    let expanded = shellexpand::tilde(&entry.path).into_owned();
    let path = Path::new(&expanded);

    if path.exists() {
        fs::read_to_string(path).ok()
    } else {
        eprintln!("{} Signature file not found: {}", "⚠".yellow(), entry.path);
        None
    }
}
