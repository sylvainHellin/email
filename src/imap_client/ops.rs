use std::fs;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use futures::TryStreamExt;
use gray_matter::{engine::YAML, Matter};
use log::info;
use super::open_imap_session;
use crate::config::ImapConfig;
use crate::parse::attachments_dir_for;
use crate::types::InboxFrontmatter;

pub async fn archive_email_on_server(imap_config: &ImapConfig, message_id: &str, archive_mailbox: &str) -> Result<()> {
    move_email_on_server(imap_config, message_id, "INBOX", archive_mailbox).await
}

/// Move an email to a different mailbox on the IMAP server (#0018).
///
/// SELECTs `source_mailbox`, finds the message by Message-ID, then
/// UID COPY + \Deleted + EXPUNGE -- the same machinery as archiving
/// (which is a move with a fixed destination). COPY+EXPUNGE is used
/// instead of MOVE so servers without the MOVE extension work too;
/// COPY preserves flags, so read/unread state survives the move.
pub async fn move_email_on_server(
    imap_config: &ImapConfig,
    message_id: &str,
    source_mailbox: &str,
    dest_mailbox: &str,
) -> Result<()> {
    info!(
        "Moving email on server: Message-ID={} {} -> {}",
        message_id, source_mailbox, dest_mailbox
    );
    let mut session = open_imap_session(imap_config).await?;

    session
        .select(source_mailbox)
        .await
        .map_err(|e| anyhow!("Failed to select {}: {}", source_mailbox, e))?;

    let query = format!("HEADER Message-ID \"{}\"", message_id);
    let uids = session
        .uid_search(&query)
        .await
        .map_err(|e| anyhow!("IMAP search failed: {}", e))?;

    if uids.is_empty() {
        session.logout().await.ok();
        return Err(anyhow!(
            "Email with Message-ID {} not found in {} on server",
            message_id,
            source_mailbox
        ));
    }

    let uid = *uids.iter().next().expect("uids verified non-empty");
    let uid_str = uid.to_string();

    session
        .uid_copy(&uid_str, dest_mailbox)
        .await
        .map_err(|e| anyhow!("Failed to copy email to {}: {}", dest_mailbox, e))?;

    session
        .uid_store(&uid_str, "+FLAGS (\\Deleted)")
        .await
        .map_err(|e| anyhow!("Failed to mark email as deleted: {}", e))?
        .try_collect::<Vec<_>>()
        .await
        .map_err(|e| anyhow!("Failed to collect store response: {}", e))?;

    session
        .expunge()
        .await
        .map_err(|e| anyhow!("Failed to expunge: {}", e))?
        .try_collect::<Vec<_>>()
        .await
        .map_err(|e| anyhow!("Failed to collect expunge response: {}", e))?;

    session.logout().await.ok();
    Ok(())
}

pub async fn archive_email_locally(imap_config: &ImapConfig, archive_dir: &Path, file_path: &Path, archive_mailbox: &str) -> Result<()> {
    move_email_locally(
        imap_config,
        archive_dir,
        file_path,
        "INBOX",
        archive_mailbox,
        "inbox",
        "archived",
    )
    .await
}

/// Move an email to another mailbox: local file move (with `status:`
/// frontmatter update) plus server-side move, rolling back the local
/// changes if the server move fails (#0018). Generalizes the archive
/// flow -- archiving is `move_email_locally(.., "INBOX", archive,
/// "inbox", "archived")`.
pub async fn move_email_locally(
    imap_config: &ImapConfig,
    dest_dir: &Path,
    file_path: &Path,
    source_mailbox: &str,
    dest_mailbox: &str,
    old_status: &str,
    new_status: &str,
) -> Result<()> {
    info!(
        "Moving email locally: {} -> {}",
        file_path.display(),
        dest_dir.display()
    );
    let content = fs::read_to_string(file_path)
        .with_context(|| format!("Failed to read file: {}", file_path.display()))?;

    let matter = Matter::<YAML>::new();
    let parsed = matter.parse(&content);
    let inbox_fm: InboxFrontmatter = parsed
        .data
        .ok_or_else(|| anyhow!("No frontmatter found in {}", file_path.display()))?
        .deserialize()
        .context("Failed to parse frontmatter")?;

    let message_id = inbox_fm.message_id.as_ref().ok_or_else(|| {
        anyhow!("No message_id in frontmatter -- cannot move on server")
    })?;

    fs::create_dir_all(dest_dir)?;

    let new_content = content.replacen(
        &format!("status: {}", old_status),
        &format!("status: {}", new_status),
        1,
    );

    let file_name = file_path
        .file_name()
        .ok_or_else(|| anyhow!("Invalid file path"))?;
    let dest = dest_dir.join(file_name);

    fs::write(&dest, &new_content)?;
    fs::remove_file(file_path)?;

    let html_companion = file_path.with_extension("html");
    let html_dest = dest.with_extension("html");
    let had_html = html_companion.exists();
    if had_html {
        fs::copy(&html_companion, &html_dest)?;
        fs::remove_file(&html_companion)?;
    }

    let att_source = attachments_dir_for(file_path);
    let att_dest = attachments_dir_for(&dest);
    let had_att_dir = att_source.is_dir();
    if had_att_dir {
        fs::rename(&att_source, &att_dest)?;
    }

    if let Err(e) =
        move_email_on_server(imap_config, message_id, source_mailbox, dest_mailbox).await
    {
        info!("IMAP move failed, rolling back local changes: {}", e);
        fs::write(file_path, &content)?;
        fs::remove_file(&dest)?;
        if had_html {
            fs::copy(&html_dest, &html_companion)?;
            fs::remove_file(&html_dest)?;
        }
        if had_att_dir {
            fs::rename(&att_dest, &att_source).ok();
        }
        return Err(e.context("IMAP move failed; local changes have been rolled back"));
    }

    Ok(())
}

pub async fn delete_email_on_server(imap_config: &ImapConfig, message_id: &str) -> Result<()> {
    info!("Deleting email on server: Message-ID={}", message_id);
    let mut session = open_imap_session(imap_config).await?;

    session
        .select("INBOX")
        .await
        .map_err(|e| anyhow!("Failed to select INBOX: {}", e))?;

    let query = format!("HEADER Message-ID \"{}\"", message_id);
    let uids = session
        .uid_search(&query)
        .await
        .map_err(|e| anyhow!("IMAP search failed: {}", e))?;

    if uids.is_empty() {
        session.logout().await.ok();
        return Err(anyhow!(
            "Email with Message-ID {} not found in INBOX on server",
            message_id
        ));
    }

    let uid = *uids.iter().next().expect("uids verified non-empty");
    let uid_str = uid.to_string();

    session
        .uid_store(&uid_str, "+FLAGS (\\Deleted)")
        .await
        .map_err(|e| anyhow!("Failed to mark email as deleted: {}", e))?
        .try_collect::<Vec<_>>()
        .await
        .map_err(|e| anyhow!("Failed to collect store response: {}", e))?;

    session
        .expunge()
        .await
        .map_err(|e| anyhow!("Failed to expunge: {}", e))?
        .try_collect::<Vec<_>>()
        .await
        .map_err(|e| anyhow!("Failed to collect expunge response: {}", e))?;

    session.logout().await.ok();
    Ok(())
}

pub async fn delete_email_locally(imap_config: &ImapConfig, file_path: &Path) -> Result<()> {
    info!("Deleting email locally: {}", file_path.display());
    let content = fs::read_to_string(file_path)
        .with_context(|| format!("Failed to read file: {}", file_path.display()))?;

    let matter = Matter::<YAML>::new();
    let parsed = matter.parse(&content);
    let message_id: Option<String> = parsed
        .data
        .and_then(|d| d.deserialize::<std::collections::HashMap<String, serde_yaml::Value>>().ok())
        .and_then(|map| map.get("message_id").and_then(|v| v.as_str().map(|s| s.to_string())));

    let html_companion = file_path.with_extension("html");
    let html_content = if html_companion.exists() {
        Some(fs::read_to_string(&html_companion)?)
    } else {
        None
    };

    let att_dir = attachments_dir_for(file_path);
    let att_backup = att_dir.with_extension("deleting");
    let had_att_dir = att_dir.is_dir();
    if had_att_dir {
        fs::rename(&att_dir, &att_backup)?;
    }

    fs::remove_file(file_path)
        .with_context(|| format!("Failed to remove local file: {}", file_path.display()))?;

    if html_content.is_some() {
        fs::remove_file(&html_companion).ok();
    }

    if let Some(ref mid) = message_id {
        if let Err(e) = delete_email_on_server(imap_config, mid).await {
            info!("IMAP delete failed, rolling back local changes: {}", e);
            fs::write(file_path, &content)?;
            if let Some(ref html) = html_content {
                fs::write(&html_companion, html)?;
            }
            if had_att_dir {
                fs::rename(&att_backup, &att_dir).ok();
            }
            return Err(e.context("IMAP delete failed; local changes have been rolled back"));
        }
    } else {
        info!(
            "No message_id in frontmatter -- skipping server deletion for {}",
            file_path.display()
        );
    }

    if had_att_dir && att_backup.exists() {
        fs::remove_dir_all(&att_backup).ok();
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Read / unread (\Seen flag)
// ---------------------------------------------------------------------------

/// Mark an email as read (\Seen) on the IMAP server.
pub async fn mark_read_on_server(
    imap_config: &ImapConfig,
    message_id: &str,
    mailbox: &str,
) -> Result<()> {
    info!("Marking email as read on server: Message-ID={}", message_id);
    let mut session = open_imap_session(imap_config).await?;

    session
        .select(mailbox)
        .await
        .map_err(|e| anyhow!("Failed to select {}: {}", mailbox, e))?;

    let query = format!("HEADER Message-ID \"{}\"", message_id);
    let uids = session
        .uid_search(&query)
        .await
        .map_err(|e| anyhow!("IMAP search failed: {}", e))?;

    if uids.is_empty() {
        session.logout().await.ok();
        return Ok(()); // Not found on server -- not an error
    }

    let uid = *uids.iter().next().expect("uids verified non-empty");
    session
        .uid_store(&uid.to_string(), "+FLAGS (\\Seen)")
        .await
        .map_err(|e| anyhow!("Failed to set \\Seen flag: {}", e))?
        .try_collect::<Vec<_>>()
        .await
        .map_err(|e| anyhow!("Failed to collect store response: {}", e))?;

    session.logout().await.ok();
    Ok(())
}

/// Mark an email as unread (remove \Seen) on the IMAP server.
pub async fn mark_unread_on_server(
    imap_config: &ImapConfig,
    message_id: &str,
    mailbox: &str,
) -> Result<()> {
    info!("Marking email as unread on server: Message-ID={}", message_id);
    let mut session = open_imap_session(imap_config).await?;

    session
        .select(mailbox)
        .await
        .map_err(|e| anyhow!("Failed to select {}: {}", mailbox, e))?;

    let query = format!("HEADER Message-ID \"{}\"", message_id);
    let uids = session
        .uid_search(&query)
        .await
        .map_err(|e| anyhow!("IMAP search failed: {}", e))?;

    if uids.is_empty() {
        session.logout().await.ok();
        return Ok(());
    }

    let uid = *uids.iter().next().expect("uids verified non-empty");
    session
        .uid_store(&uid.to_string(), "-FLAGS (\\Seen)")
        .await
        .map_err(|e| anyhow!("Failed to remove \\Seen flag: {}", e))?
        .try_collect::<Vec<_>>()
        .await
        .map_err(|e| anyhow!("Failed to collect store response: {}", e))?;

    session.logout().await.ok();
    Ok(())
}

/// Update the `read:` field in an email's YAML frontmatter on disk.
pub fn update_read_status_locally(file_path: &Path, read: bool) -> Result<()> {
    let content = fs::read_to_string(file_path)
        .with_context(|| format!("Failed to read file: {}", file_path.display()))?;

    // Try replacing existing `read: true` or `read: false` line
    let new_content = if content.contains("\nread: true\n") || content.contains("\nread: false\n") {
        content
            .replacen("\nread: true\n", &format!("\nread: {}\n", read), 1)
            .replacen("\nread: false\n", &format!("\nread: {}\n", read), 1)
    } else {
        // Insert `read:` before the closing `---` of frontmatter
        if let Some(first) = content.find("---") {
            if let Some(second_offset) = content[first + 3..].find("---") {
                let insert_pos = first + 3 + second_offset;
                format!(
                    "{}read: {}\n{}",
                    &content[..insert_pos],
                    read,
                    &content[insert_pos..]
                )
            } else {
                content
            }
        } else {
            content
        }
    };

    fs::write(file_path, new_content)?;
    Ok(())
}

/// Extract message_id from a frontmatter file.
pub fn get_message_id_from_file(path: &Path) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    let matter = Matter::<YAML>::new();
    let parsed = matter.parse(&content);
    let fm: InboxFrontmatter = parsed.data?.deserialize().ok()?;
    fm.message_id
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_update_read_status_locally_toggle_false_to_true() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("email.md");
        fs::write(&path, "---\nfrom: a@x.com\nread: false\nstatus: inbox\n---\n\nBody").unwrap();

        update_read_status_locally(&path, true).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("\nread: true\n"));
        assert!(!content.contains("read: false"));
    }

    #[test]
    fn test_update_read_status_locally_toggle_true_to_false() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("email.md");
        fs::write(&path, "---\nfrom: a@x.com\nread: true\nstatus: inbox\n---\n\nBody").unwrap();

        update_read_status_locally(&path, false).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("\nread: false\n"));
        assert!(!content.contains("read: true"));
    }

    #[test]
    fn test_update_read_status_locally_insert_when_missing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("email.md");
        fs::write(&path, "---\nfrom: a@x.com\nstatus: inbox\n---\n\nBody").unwrap();

        update_read_status_locally(&path, true).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("read: true"));
        // Should still have valid frontmatter
        assert!(content.contains("---"));
    }

    #[test]
    fn test_get_message_id_from_file_found() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("email.md");
        fs::write(&path, "---\nfrom: a@x.com\nto: b@x.com\nsubject: test\nmessage_id: \"<abc@x.com>\"\n---\n\nBody").unwrap();

        let mid = get_message_id_from_file(&path);
        assert_eq!(mid, Some("<abc@x.com>".to_string()));
    }

    #[test]
    fn test_get_message_id_from_file_missing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("email.md");
        fs::write(&path, "---\nfrom: a@x.com\nto: b@x.com\nsubject: test\n---\n\nBody").unwrap();

        let mid = get_message_id_from_file(&path);
        assert_eq!(mid, None);
    }

    #[test]
    fn test_get_message_id_from_file_nonexistent() {
        let mid = get_message_id_from_file(Path::new("/nonexistent/path/email.md"));
        assert_eq!(mid, None);
    }
}
