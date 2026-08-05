//! CLI handlers for `mp calendar …` (organizer-side iMIP reconciliation).

use crate::config::{store_path, AccountConfig, GlobalConfig};
use crate::reconcile::reconcile_account;
use crate::store::{BlobStore, Store};
use anyhow::{anyhow, Result};
use colored::*;

/// `mp calendar rebuild [--account NAME]`: report what the stored REPLY
/// messages resolve on the stored invitations.
///
/// It writes nothing. Attendee statuses are derived where they are displayed,
/// from the `invite.ics` blobs of the account's rows (#0038 scope item 6), so
/// there is no cached copy left to rebuild and the command exists to show what
/// the fold sees. Safe to run repeatedly by construction.
pub fn handle_rebuild(config: &GlobalConfig, account_name: Option<String>) -> Result<()> {
    let accounts: Vec<&AccountConfig> = match account_name {
        Some(name) => vec![config
            .accounts
            .iter()
            .find(|a| a.name.eq_ignore_ascii_case(&name))
            .ok_or_else(|| anyhow!("no account named '{}'", name))?],
        None => {
            if config.accounts.is_empty() {
                return Err(anyhow!("no accounts configured"));
            }
            config.accounts.iter().collect()
        }
    };

    for account in accounts {
        println!(
            "{} Reconciling calendar replies for {} …",
            "ℹ".blue(),
            account.name.yellow()
        );
        let path = store_path(&account.name);
        if !path.exists() {
            println!("{} no store yet for {}", "•".blue(), account.name);
            continue;
        }
        let store = Store::open(&path)?;
        let blobs = BlobStore::for_account(&account.name);
        let report = reconcile_account(&store, &blobs, &account.name);
        println!(
            "{} {} attendee status(es) resolved across {} invite(s) / {} reply(ies)",
            "✓".green(),
            report.resolved.to_string().bold(),
            report.invites_seen,
            report.replies_seen,
        );
    }
    Ok(())
}
