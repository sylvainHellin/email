use anyhow::Result;
use colored::*;

use crate::config::{
    all_configured_mailboxes, blobs_dir, config_path, drafts_dir, get_secret, load_global_config,
    mailypoppins_data_dir, retention_for, store_path, AuthMethod,
};
use crate::store::sweep::human_bytes;

/// A horizon in days, with `0` spelled as the keep-all it means.
fn horizon_label(days: u32) -> String {
    if days == 0 {
        "0 (keep all)".to_string()
    } else {
        format!("{days}")
    }
}

/// Print the config file path.
pub fn cmd_config_path() {
    println!("{}", config_path().display());
}

/// Display the resolved config with masked passwords.
pub fn cmd_config_show() -> Result<()> {
    let config = load_global_config()?;

    println!("{}", "=== mailypoppins configuration ===".bold().cyan());
    println!("{}: {}", "Config file".bold(), config_path().display());
    println!("{}: {}", "Data dir".bold(), mailypoppins_data_dir().display());

    let theme_name = if config.theme.is_empty() {
        format!("{} (default)", crate::tui::theme::DEFAULT_THEME_NAME)
    } else {
        config.theme.clone()
    };
    println!("{}: {}", "Theme".bold(), theme_name);
    println!("{}: {}", "Notifications".bold(), config.notifications);

    println!("\n{}", "[email]".bold());
    println!("  font_family       = {}", config.email.font_family);
    println!("  font_size         = {}", config.email.font_size);
    println!("  include_signature = {}", config.email.include_signature);

    if config.accounts.is_empty() {
        println!("\n{}", "(no accounts configured)".yellow());
        return Ok(());
    }

    for account in &config.accounts {
        println!("\n{}", format!("[[accounts]] name = \"{}\"", account.name).bold().cyan());
        println!("  default_from = {}", account.default_from);

        match account.auth_method {
            AuthMethod::OAuth2 | AuthMethod::Graph => {
                let label = if account.auth_method == AuthMethod::Graph {
                    "graph (Microsoft Graph API)"
                } else {
                    "oauth2"
                };
                println!("  auth_method  = {}", label.yellow());
                if let Some(ref oauth2) = account.oauth2 {
                    println!("\n  {}", "[oauth2]".bold());
                    println!("    client_id = {}", oauth2.client_id);
                    println!("    tenant_id = {}", oauth2.tenant_id);
                    // Show token cache status
                    let cache_path = crate::oauth2::token_cache_path(&account.name);
                    if cache_path.exists() {
                        match crate::oauth2::load_token_cache(&account.name) {
                            Some(cache) => {
                                if cache.is_expired() {
                                    println!("    token      = {} (run `mp config oauth2-login`)", "expired".red());
                                } else {
                                    println!("    token      = {}", "valid".green());
                                }
                            }
                            None => {
                                println!("    token      = {} (corrupt or undecryptable cache file)", "invalid".red());
                            }
                        }
                    } else {
                        println!("    token      = {} (run `mp config oauth2-login`)", "not cached".red());
                    }
                }
            }
            AuthMethod::Password => {
                println!("  auth_method  = password");
            }
        }

        if account.auth_method == AuthMethod::Graph {
            // Graph accounts don't use SMTP/IMAP -- show a note instead
            println!("\n  {}", "[transport]".bold());
            println!("    Uses Microsoft Graph API (no SMTP/IMAP)");
        } else {
            println!("\n  {}", "[smtp]".bold());
            println!("    host     = {}", account.smtp.host);
            println!("    port     = {}", account.smtp.port);
            println!("    username = {}", account.smtp.username);
            let smtp_key = format!("smtp-password-{}", account.name);
            let smtp_pw = if get_secret(&smtp_key).is_ok() {
                "****".green().to_string()
            } else {
                "(not set)".red().to_string()
            };
            println!("    password = {}", smtp_pw);
            if account.smtp.accept_invalid_certs {
                println!("    accept_invalid_certs = {}", "true".yellow());
            }

            println!("\n  {}", "[imap]".bold());
            println!(
                "    host     = {}",
                if account.imap.host.is_empty() {
                    format!("{} (falls back to smtp.host)", account.smtp.host)
                        .dimmed()
                        .to_string()
                } else {
                    account.imap.host.clone()
                }
            );
            println!("    port     = {}", account.imap.port);
            println!(
                "    username = {}",
                if account.imap.username.is_empty() {
                    format!("{} (falls back to smtp.username)", account.smtp.username)
                        .dimmed()
                        .to_string()
                } else {
                    account.imap.username.clone()
                }
            );
            let imap_key = format!("imap-password-{}", account.name);
            let smtp_key = format!("smtp-password-{}", account.name);
            let imap_pw = if get_secret(&imap_key).is_ok() {
                "****".green().to_string()
            } else if get_secret(&smtp_key).is_ok() {
                format!("{} (falls back to smtp)", "****".green())
            } else {
                "(not set)".red().to_string()
            };
            println!("    password = {}", imap_pw);
            if account.imap.accept_invalid_certs {
                println!("    accept_invalid_certs = {}", "true".yellow());
            }
        }

        println!("\n  {}", "[local paths]".bold());
        println!("    store  = {}", store_path(&account.name).display());
        println!("    blobs  = {}", blobs_dir(&account.name).display());
        println!("    drafts = {}", drafts_dir(&account.name).display());

        // Role to server folder, and nothing else: a mailbox has no local
        // directory since received mail moved into the store above. This block
        // used to print `<account_dir>/inbox` per mailbox, a path nothing has
        // created since the cutover (#0057).
        println!("\n  {}", "[mailboxes]".bold());
        let all = all_configured_mailboxes(account);
        if all.is_empty() {
            println!("    (none configured)");
        } else {
            for (role, mapping) in &all {
                println!("    {} -> {}", format!("[{}]", role).bold(), mapping.server);
            }
        }

        // Retention is enforced as of #0060: the sweep runs after every sync
        // and via `mp store gc`. This block used to (and must no longer) label
        // it unenforced.
        match retention_for(&config, account) {
            Ok(policy) => {
                println!("\n  {}", "[retention]".bold());
                println!("    enforced             = {}", "yes (sweep after sync + `mp store gc`)".green());
                println!("    max_disk_bytes       = {}", human_bytes(policy.max_disk_bytes));
                println!(
                    "    body_horizon_days    = {}",
                    horizon_label(policy.body_horizon_days)
                );
                println!(
                    "    attachment_horizon_days = {}",
                    horizon_label(policy.attachment_horizon_days)
                );
            }
            Err(e) => {
                println!("\n  {}", "[retention]".bold());
                println!("    {}", format!("invalid: {e:#}").red());
            }
        }

        println!("\n  {}", "[signatures]".bold());
        println!(
            "    default = {}",
            account.signatures.default.as_deref().unwrap_or("(none)")
        );
        for (key, entry) in &account.signatures.entries {
            println!("    [signatures.{}]", key);
            if let Some(ref name) = entry.name {
                println!("      name = {}", name);
            }
            println!("      path = {}", entry.path);
        }
    }

    Ok(())
}
