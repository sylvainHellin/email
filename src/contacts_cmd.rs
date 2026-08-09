//! CLI handlers for `mp contacts …`.

use crate::config::{account_dir, AccountConfig, GlobalConfig};
use crate::contacts::{
    build_index_for_account, cache_path, load_cache, save_rebuilt_cache, search, CacheSave,
    Contact, ContactIndex,
};
use anyhow::{anyhow, Result};
use colored::*;
use std::path::{Path, PathBuf};

pub fn handle_search(
    config: &GlobalConfig,
    query: Option<String>,
    parsable: bool,
    limit: usize,
    account_name: Option<String>,
) -> Result<()> {
    let account = pick_account(config, account_name.as_deref())?;
    let root = account_root(account)?;
    let index = load_or_build(account, &root)?;

    let q = query.unwrap_or_default();
    let results = search(&index, &q, limit);

    if parsable {
        // Mutt/aerc/himalaya-vim compatibility format: first line is a header,
        // then `email\tname` lines. Mutt discards the first line by protocol.
        println!("{} results for '{}'", results.len(), q);
        for r in results {
            println!("{}\t{}", r.contact.address, r.contact.display_name);
        }
    } else if results.is_empty() {
        println!("{} No matches", "ℹ".blue());
    } else {
        for (i, r) in results.iter().enumerate() {
            let tier_mark = tier_indicator(r.contact);
            let name = if r.contact.display_name.is_empty() {
                "(no name)".dimmed()
            } else {
                r.contact.display_name.as_str().yellow()
            };
            println!(
                "{:>3}. {} {} {}",
                i + 1,
                tier_mark,
                name,
                format!("<{}>", r.contact.address).cyan(),
            );
        }
    }
    Ok(())
}

pub fn handle_rebuild(config: &GlobalConfig, account_name: Option<String>) -> Result<()> {
    let accounts: Vec<&AccountConfig> = match account_name {
        Some(name) => vec![pick_account(config, Some(&name))?],
        None => {
            if config.accounts.is_empty() {
                return Err(anyhow!("no accounts configured"));
            }
            config.accounts.iter().collect()
        }
    };
    for account in accounts {
        let root = account_root(account)?;
        println!(
            "{} Rebuilding contacts for {} …",
            "ℹ".blue(),
            account.name.yellow()
        );
        let index = build_index_for_account(account)?;
        let count = index.contacts.len();
        match save_rebuilt_cache(&root, &index)? {
            CacheSave::Written => println!(
                "{} {} contacts cached at {}",
                "✓".green(),
                count.to_string().bold(),
                cache_path(&root).display()
            ),
            CacheSave::RefusedEmpty { kept } => println!(
                "{} Rebuild found no contacts; kept the {} already cached at {}",
                "⚠".yellow(),
                kept.to_string().bold(),
                cache_path(&root).display()
            ),
            CacheSave::RefusedShrunk { kept, rebuilt } => println!(
                "{} Rebuild found only {} contacts against {} cached; kept the cache at {}",
                "⚠".yellow(),
                rebuilt.to_string().bold(),
                kept.to_string().bold(),
                cache_path(&root).display()
            ),
        }
    }
    Ok(())
}

pub fn handle_stats(config: &GlobalConfig, account_name: Option<String>) -> Result<()> {
    let account = pick_account(config, account_name.as_deref())?;
    let root = account_root(account)?;
    let index = load_or_build(account, &root)?;

    let total = index.contacts.len();
    let sent_to_total: u32 = index.contacts.values().map(|c| c.sent_to).sum();
    let sent_cc_total: u32 = index.contacts.values().map(|c| c.sent_cc).sum();
    let received_total: u32 = index.contacts.values().map(|c| c.received).sum();

    println!("{} Contacts for {}", "ℹ".blue(), account.name.yellow());
    println!("  Total contacts:  {}", total.to_string().bold());
    println!("  Sent-to hits:    {}", sent_to_total);
    println!("  Sent-cc hits:    {}", sent_cc_total);
    println!("  Received hits:   {}", received_total);
    println!("  Cache path:      {}", cache_path(&root).display());
    println!("  Built at:        {}", index.built_at);
    println!();
    println!("{}", "Top 10:".bold());
    let top = search(&index, "", 10);
    for (i, r) in top.iter().enumerate() {
        let name = if r.contact.display_name.is_empty() {
            "(no name)".to_string()
        } else {
            r.contact.display_name.clone()
        };
        println!(
            "  {:>2}. {} <{}> (sent_to={}, sent_cc={}, received={})",
            i + 1,
            name,
            r.contact.address,
            r.contact.sent_to,
            r.contact.sent_cc,
            r.contact.received,
        );
    }
    Ok(())
}

// --- helpers ---

fn pick_account<'a>(config: &'a GlobalConfig, name: Option<&str>) -> Result<&'a AccountConfig> {
    match name {
        Some(n) => config
            .accounts
            .iter()
            .find(|a| a.name.eq_ignore_ascii_case(n))
            .ok_or_else(|| anyhow!("no account named '{}'", n)),
        None => config
            .accounts
            .first()
            .ok_or_else(|| anyhow!("no accounts configured")),
    }
}

fn account_root(account: &AccountConfig) -> Result<PathBuf> {
    Ok(account_dir(&account.name))
}

fn load_or_build(account: &AccountConfig, root: &Path) -> Result<ContactIndex> {
    if let Some(idx) = load_cache(root)? {
        return Ok(idx);
    }
    // No cache yet — build on demand.
    let idx = build_index_for_account(account)?;
    match save_rebuilt_cache(root, &idx)? {
        CacheSave::Written => Ok(idx),
        // The guard kept something on disk, so the caller must be shown what
        // the disk holds and not the rebuild that was just refused (#0067).
        refused => {
            eprintln!(
                "{} Contacts rebuild refused ({refused:?}); showing the cache at {}",
                "⚠".yellow(),
                cache_path(root).display()
            );
            Ok(load_cache(root)?.unwrap_or(idx))
        }
    }
}

fn tier_indicator(c: &Contact) -> colored::ColoredString {
    // Nerd Font icons (NOT emojis): nf-md-account_check, nf-md-account, nf-md-email.
    if c.sent_to > 0 {
        "󰁞".green()
    } else if c.sent_cc > 0 {
        "".cyan()
    } else {
        "".blue()
    }
}
