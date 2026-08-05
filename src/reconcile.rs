//! Organizer-side iMIP REPLY reconciliation (#0030).
//!
//! When an attendee accepts/declines an invitation, their mail client sends a
//! `METHOD:REPLY` email carrying a single `ATTENDEE` with a `PARTSTAT`. Those
//! replies arrive through normal sync and are saved with an `event:` block +
//! `invite.ics` sidecar (#0027). This module reconciles them against the
//! locally-stored `METHOD:REQUEST` invites (the Sent copy, and any archived
//! copy) so the organizer sees who responded — updating each invite's
//! `event.attendees[].status` frontmatter.
//!
//! # Multi-machine consistency (design §3)
//!
//! The `event.attendees[]` cache is *local derived state*: it holds nothing
//! that is not already an IMAP-visible message (the sent invite + incoming
//! REPLY emails). Reconciliation is therefore designed to be:
//!
//! - **Idempotent** — re-running over the same messages yields byte-identical
//!   files. The surgical rewriter ([`crate::draft::set_event_attendee_status`])
//!   returns `Unchanged` and skips the write when the status already matches.
//! - **Re-runnable over the whole mailstore** — [`reconcile_account`] walks
//!   every mailbox, recomputing statuses from scratch. This is the primitive
//!   behind `mp calendar rebuild`; two machines syncing the same account
//!   converge on identical state with no machine-to-machine sync.
//!
//! # Algorithm
//!
//! 1. Walk the account root, parsing every `.md`'s `event:` block. For UID and
//!    SEQUENCE we prefer the sidecar `.ics` (source of truth); we fall back to
//!    frontmatter when the sidecar is missing (an invite may have been fetched
//!    before the sidecar convention, or archived without it).
//! 2. Index invites (`method == REQUEST`) and replies (`method == REPLY`) by
//!    UID.
//! 3. For each invite, gather replies with the same UID whose `SEQUENCE` is
//!    **not older** than the invite's. Group them by attendee address
//!    (case-insensitive); the winner per address is the reply with the highest
//!    `(sequence, dtstamp)` — a newer sequence supersedes, ties break on the
//!    latest `DTSTAMP`.
//! 4. Apply the winning `PARTSTAT` to the invite's matching `attendees[]`
//!    entry via the surgical frontmatter rewrite. Unknown attendee addresses
//!    in a REPLY are ignored (documented decision): the organizer's attendee
//!    list is the invite's, so a reply from an address that was never invited
//!    has nothing to update.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use gray_matter::{engine::YAML, Matter};
use walkdir::WalkDir;

use crate::draft::{set_event_attendee_status, AttendeeUpdate};
use crate::parse::CALENDAR_SIDECAR_NAME;
use crate::types::InboxFrontmatter;

/// A single REPLY observation: an attendee's PARTSTAT for a given UID.
#[derive(Debug, Clone)]
struct ReplyObs {
    address: String,
    status: String,
    sequence: u32,
    /// RFC3339 UTC `DTSTAMP`, or empty when the source omitted it. Compared
    /// lexicographically — RFC3339 UTC strings sort chronologically.
    dtstamp: String,
}

/// A locally-stored invite (`method == REQUEST`) we may update.
#[derive(Debug, Clone)]
struct InviteRef {
    path: PathBuf,
    sequence: u32,
}

/// UID -> the locally-stored invites carrying it (usually one Sent copy).
type InviteIndex = HashMap<String, Vec<InviteRef>>;
/// UID -> attendee address -> winning reply for that attendee.
type ReplyIndex = HashMap<String, HashMap<String, ReplyObs>>;

/// Outcome counters for a reconciliation pass.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReconcileStats {
    /// Attendee `status:` lines actually rewritten on disk.
    pub updated: usize,
    /// Invite files that were inspected and matched a reply but needed no
    /// change (already up to date) — the idempotent path.
    pub unchanged: usize,
    /// REPLY messages seen during the walk.
    pub replies_seen: usize,
    /// REQUEST invites seen during the walk.
    pub invites_seen: usize,
}

/// Read the authoritative UID / SEQUENCE / DTSTAMP for an email by parsing its
/// sidecar `.ics` when present, falling back to the frontmatter `event:` block.
/// Returns `(uid, sequence, dtstamp)` with `dtstamp` empty when unknown.
fn authoritative_ids(md_path: &Path, fm_event: &crate::types::EventFrontmatter) -> (Option<String>, u32, String) {
    let sidecar = crate::parse::attachments_dir_for(md_path).join(CALENDAR_SIDECAR_NAME);
    if let Ok(bytes) = fs::read(&sidecar) {
        if let Some(parsed) = crate::calendar::parse_ics(&bytes) {
            let uid = parsed.uid.or_else(|| fm_event.uid.clone());
            let dtstamp = parsed.dtstamp.unwrap_or_default();
            return (uid, parsed.sequence, dtstamp);
        }
    }
    (fm_event.uid.clone(), fm_event.sequence, String::new())
}

/// Reply/invite classification for one email.
enum Classified {
    Reply(String, ReplyObs),  // (uid, obs)
    Invite(String, InviteRef), // (uid, ref)
    Neither,
}

/// Classify a single `.md` file by reading its `event:` block. Best-effort:
/// any parse failure yields [`Classified::Neither`].
fn classify(md_path: &Path) -> Classified {
    let content = match fs::read_to_string(md_path) {
        Ok(c) => c,
        Err(_) => return Classified::Neither,
    };
    let matter = Matter::<YAML>::new();
    let parsed = matter.parse(&content);
    let fm: InboxFrontmatter = match parsed.data.and_then(|d| d.deserialize().ok()) {
        Some(fm) => fm,
        None => return Classified::Neither,
    };
    let Some(event) = fm.event.as_ref() else {
        return Classified::Neither;
    };
    let method = event.method.as_deref().unwrap_or("").to_uppercase();
    let (uid_opt, sequence, dtstamp) = authoritative_ids(md_path, event);
    let Some(uid) = uid_opt else {
        return Classified::Neither;
    };
    let uid = uid.trim().to_string();
    if uid.is_empty() {
        return Classified::Neither;
    }

    match method.as_str() {
        "REPLY" => {
            // A REPLY carries exactly the replying attendee. Prefer the sidecar
            // attendees (authoritative PARTSTAT); fall back to frontmatter.
            let obs = reply_obs_from(md_path, event, sequence, &dtstamp);
            match obs {
                Some(o) => Classified::Reply(uid, o),
                None => Classified::Neither,
            }
        }
        "REQUEST" => Classified::Invite(uid, InviteRef { path: md_path.to_path_buf(), sequence }),
        _ => Classified::Neither, // CANCEL and anything else: out of scope (#0031)
    }
}

/// Extract the single replying attendee's `(address, status)` from a REPLY,
/// preferring the sidecar `.ics` and falling back to the frontmatter block.
fn reply_obs_from(
    md_path: &Path,
    fm_event: &crate::types::EventFrontmatter,
    sequence: u32,
    dtstamp: &str,
) -> Option<ReplyObs> {
    // Sidecar first: it is the source of truth for the PARTSTAT.
    let sidecar = crate::parse::attachments_dir_for(md_path).join(CALENDAR_SIDECAR_NAME);
    if let Ok(bytes) = fs::read(&sidecar) {
        if let Some(parsed) = crate::calendar::parse_ics(&bytes) {
            if let Some(att) = parsed.attendees.into_iter().find(|a| !a.address.trim().is_empty()) {
                return Some(ReplyObs {
                    address: att.address.trim().to_lowercase(),
                    status: att.status,
                    sequence,
                    dtstamp: dtstamp.to_string(),
                });
            }
        }
    }
    // Frontmatter fallback (missing/unparseable sidecar).
    fm_event
        .attendees
        .iter()
        .find(|a| !a.address.trim().is_empty())
        .map(|a| ReplyObs {
            address: a.address.trim().to_lowercase(),
            status: a.status.clone(),
            sequence,
            dtstamp: dtstamp.to_string(),
        })
}

/// Whether reply `a` supersedes reply `b` for the same attendee+UID: a newer
/// sequence wins; within a sequence the later `DTSTAMP` wins.
fn supersedes(a: &ReplyObs, b: &ReplyObs) -> bool {
    (a.sequence, a.dtstamp.as_str()) > (b.sequence, b.dtstamp.as_str())
}

/// Walk `account_root`, indexing invites and the winning reply per
/// attendee+UID. Shared by [`reconcile_account`] and [`bump_after_sync`].
fn build_index(account_root: &Path) -> (InviteIndex, ReplyIndex, ReconcileStats) {
    let mut invites: InviteIndex = HashMap::new();
    // uid -> address -> winning reply
    let mut replies: ReplyIndex = HashMap::new();
    let mut stats = ReconcileStats::default();

    for entry in WalkDir::new(account_root).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if !path.is_file() || path.extension().is_none_or(|ext| ext != "md") {
            continue;
        }
        match classify(path) {
            Classified::Reply(uid, obs) => {
                stats.replies_seen += 1;
                let by_addr = replies.entry(uid).or_default();
                match by_addr.get(&obs.address) {
                    Some(existing) if !supersedes(&obs, existing) => {}
                    _ => {
                        by_addr.insert(obs.address.clone(), obs);
                    }
                }
            }
            Classified::Invite(uid, r) => {
                stats.invites_seen += 1;
                invites.entry(uid).or_default().push(r);
            }
            Classified::Neither => {}
        }
    }
    (invites, replies, stats)
}

/// Reconcile every invite under `account_root` against every REPLY under it.
/// Walks the whole mailstore, so it is safe to run repeatedly (idempotent) and
/// is the primitive behind `mp calendar rebuild`.
///
/// `changed` collects the invite `.md` paths whose bytes were actually
/// rewritten, so callers can invalidate only the affected mailbox caches.
pub fn reconcile_account(account_root: &Path) -> Result<ReconcileStats> {
    let mut changed = Vec::new();
    reconcile_into(account_root, &mut changed)
}

/// Core reconciliation: applies winning replies to each matching invite,
/// pushing every invite path whose bytes changed into `changed`.
fn reconcile_into(account_root: &Path, changed: &mut Vec<PathBuf>) -> Result<ReconcileStats> {
    if !account_root.is_dir() {
        return Ok(ReconcileStats::default());
    }
    let (invites, replies, mut stats) = build_index(account_root);

    for (uid, invite_list) in &invites {
        let Some(by_addr) = replies.get(uid) else {
            continue;
        };
        for invite in invite_list {
            let mut invite_changed = false;
            for (address, obs) in by_addr {
                // Ignore replies for a sequence older than this invite's.
                if obs.sequence < invite.sequence {
                    continue;
                }
                match set_event_attendee_status(&invite.path, address, &obs.status) {
                    Ok(AttendeeUpdate::Updated) => {
                        stats.updated += 1;
                        invite_changed = true;
                    }
                    Ok(AttendeeUpdate::Unchanged) => stats.unchanged += 1,
                    Ok(AttendeeUpdate::NotFound) => {}
                    Err(e) => log::warn!(
                        "reconcile: failed to update {} for {}: {}",
                        invite.path.display(),
                        address,
                        e
                    ),
                }
            }
            if invite_changed {
                changed.push(invite.path.clone());
            }
        }
    }

    Ok(stats)
}

/// Best-effort incremental hook, called after a sync cycle saves new emails.
/// Only does work when the sync actually saved a `METHOD:REPLY` invite
/// (`saw_reply`), keeping the common no-calendar path free. When triggered it
/// runs the full idempotent account reconciliation (cheap: one mailstore walk)
/// so a REPLY that arrives before its invite, or vice versa, still converges.
///
/// Errors are logged, never propagated — a reconciliation hiccup must never
/// fail a sync. Returns the affected invite paths so callers (the TUI) can
/// invalidate the relevant mailbox caches.
pub fn bump_after_sync(account_root: &Path, saw_reply: bool) -> Vec<PathBuf> {
    if !saw_reply {
        return Vec::new();
    }
    let mut changed = Vec::new();
    if let Err(e) = reconcile_into(account_root, &mut changed) {
        log::warn!("reconcile hook failed for {}: {}", account_root.display(), e);
        return Vec::new();
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Write an invite/reply `.md` plus its `invite.ics` sidecar in a mailbox
    /// dir under `root`. Returns the `.md` path.
    fn write_msg(root: &Path, mailbox: &str, name: &str, md: &str, ics: Option<&str>) -> PathBuf {
        let dir = root.join(mailbox);
        fs::create_dir_all(&dir).unwrap();
        let md_path = dir.join(format!("{name}.md"));
        fs::write(&md_path, md).unwrap();
        if let Some(ics) = ics {
            let att = crate::parse::attachments_dir_for(&md_path);
            fs::create_dir_all(&att).unwrap();
            fs::write(att.join(CALENDAR_SIDECAR_NAME), ics).unwrap();
        }
        md_path
    }

    fn invite_md(uid: &str, seq: u32, attendees: &[(&str, &str)]) -> String {
        let mut s = String::from("---\nfrom: me@example.com\nto: a@example.com\nsubject: Plan\nstatus: sent\nevent:\n");
        s.push_str(&format!("  uid: {uid}\n  method: REQUEST\n  sequence: {seq}\n  rsvp: needs-action\n  attendees:\n"));
        for (addr, status) in attendees {
            s.push_str(&format!("  - address: {addr}\n    status: {status}\n"));
        }
        s.push_str("---\n\nYou are invited.\n");
        s
    }

    fn invite_ics(uid: &str, seq: u32) -> String {
        format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:{uid}\r\nSEQUENCE:{seq}\r\nSUMMARY:Plan\r\nDTSTART:20260720T120000Z\r\nORGANIZER:mailto:me@example.com\r\nATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:a@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
        )
    }

    fn reply_md(uid: &str, seq: u32, addr: &str, status: &str) -> String {
        format!(
            "---\nfrom: {addr}\nto: me@example.com\nsubject: \"Re: Plan\"\nstatus: inbox\nevent:\n  uid: {uid}\n  method: REPLY\n  sequence: {seq}\n  attendees:\n  - address: {addr}\n    status: {status}\n---\n\nRSVP.\n"
        )
    }

    fn reply_ics(uid: &str, seq: u32, addr: &str, partstat: &str, dtstamp: &str) -> String {
        format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REPLY\r\nBEGIN:VEVENT\r\nUID:{uid}\r\nSEQUENCE:{seq}\r\nDTSTAMP:{dtstamp}\r\nORGANIZER:mailto:me@example.com\r\nATTENDEE;PARTSTAT={partstat}:mailto:{addr}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
        )
    }

    fn status_of(md: &Path, addr: &str) -> Option<String> {
        let matter = Matter::<YAML>::new();
        let parsed = matter.parse(&fs::read_to_string(md).unwrap());
        let fm: InboxFrontmatter = parsed.data.unwrap().deserialize().unwrap();
        fm.event?
            .attendees
            .into_iter()
            .find(|a| a.address.eq_ignore_ascii_case(addr))
            .map(|a| a.status)
    }

    #[test]
    fn reply_flips_matching_attendee() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let inv = write_msg(root, "sent", "inv", &invite_md("u1@x", 0, &[("a@example.com", "needs-action")]), Some(&invite_ics("u1@x", 0)));
        write_msg(root, "inbox", "rep", &reply_md("u1@x", 0, "a@example.com", "accepted"), Some(&reply_ics("u1@x", 0, "a@example.com", "ACCEPTED", "20260710T120000Z")));

        let stats = reconcile_account(root).unwrap();
        assert_eq!(stats.updated, 1);
        assert_eq!(status_of(&inv, "a@example.com").as_deref(), Some("accepted"));
    }

    #[test]
    fn case_insensitive_address_match() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let inv = write_msg(root, "sent", "inv", &invite_md("u1@x", 0, &[("Alice@Example.com", "needs-action")]), Some(&invite_ics("u1@x", 0)));
        write_msg(root, "inbox", "rep", &reply_md("u1@x", 0, "alice@example.com", "declined"), Some(&reply_ics("u1@x", 0, "alice@example.com", "DECLINED", "20260710T120000Z")));

        reconcile_account(root).unwrap();
        assert_eq!(status_of(&inv, "Alice@Example.com").as_deref(), Some("declined"));
    }

    #[test]
    fn latest_dtstamp_wins_within_sequence() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let inv = write_msg(root, "sent", "inv", &invite_md("u1@x", 0, &[("a@example.com", "needs-action")]), Some(&invite_ics("u1@x", 0)));
        // Earlier reply: accepted. Later reply: declined. Declined must win.
        write_msg(root, "inbox", "r1", &reply_md("u1@x", 0, "a@example.com", "accepted"), Some(&reply_ics("u1@x", 0, "a@example.com", "ACCEPTED", "20260710T090000Z")));
        write_msg(root, "inbox", "r2", &reply_md("u1@x", 0, "a@example.com", "declined"), Some(&reply_ics("u1@x", 0, "a@example.com", "DECLINED", "20260711T090000Z")));

        reconcile_account(root).unwrap();
        assert_eq!(status_of(&inv, "a@example.com").as_deref(), Some("declined"));
    }

    #[test]
    fn older_sequence_reply_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // Invite bumped to sequence 2; a stale reply for sequence 1 must be ignored.
        let inv = write_msg(root, "sent", "inv", &invite_md("u1@x", 2, &[("a@example.com", "needs-action")]), Some(&invite_ics("u1@x", 2)));
        write_msg(root, "inbox", "rep", &reply_md("u1@x", 1, "a@example.com", "accepted"), Some(&reply_ics("u1@x", 1, "a@example.com", "ACCEPTED", "20260710T120000Z")));

        let stats = reconcile_account(root).unwrap();
        assert_eq!(stats.updated, 0);
        assert_eq!(status_of(&inv, "a@example.com").as_deref(), Some("needs-action"));
    }

    #[test]
    fn newer_sequence_reply_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let inv = write_msg(root, "sent", "inv", &invite_md("u1@x", 1, &[("a@example.com", "needs-action")]), Some(&invite_ics("u1@x", 1)));
        // Reply for seq 1 accepted, reply for seq 2 declined -> seq2 (newer) wins.
        write_msg(root, "inbox", "r1", &reply_md("u1@x", 1, "a@example.com", "accepted"), Some(&reply_ics("u1@x", 1, "a@example.com", "ACCEPTED", "20260710T090000Z")));
        write_msg(root, "inbox", "r2", &reply_md("u1@x", 2, "a@example.com", "declined"), Some(&reply_ics("u1@x", 2, "a@example.com", "DECLINED", "20260709T090000Z")));

        reconcile_account(root).unwrap();
        assert_eq!(status_of(&inv, "a@example.com").as_deref(), Some("declined"));
    }

    #[test]
    fn unknown_attendee_in_reply_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let inv = write_msg(root, "sent", "inv", &invite_md("u1@x", 0, &[("a@example.com", "needs-action")]), Some(&invite_ics("u1@x", 0)));
        // A reply from an address that was never on the invite: no-op.
        write_msg(root, "inbox", "rep", &reply_md("u1@x", 0, "stranger@example.com", "accepted"), Some(&reply_ics("u1@x", 0, "stranger@example.com", "ACCEPTED", "20260710T120000Z")));

        let stats = reconcile_account(root).unwrap();
        assert_eq!(stats.updated, 0);
        assert_eq!(status_of(&inv, "a@example.com").as_deref(), Some("needs-action"));
    }

    #[test]
    fn missing_sidecar_falls_back_to_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // Neither invite nor reply has a sidecar: UID/sequence/status come from
        // frontmatter. DTSTAMP is empty, but a single reply still applies.
        let inv = write_msg(root, "sent", "inv", &invite_md("u1@x", 0, &[("a@example.com", "needs-action")]), None);
        write_msg(root, "inbox", "rep", &reply_md("u1@x", 0, "a@example.com", "tentative"), None);

        let stats = reconcile_account(root).unwrap();
        assert_eq!(stats.updated, 1);
        assert_eq!(status_of(&inv, "a@example.com").as_deref(), Some("tentative"));
    }

    #[test]
    fn idempotent_second_run_is_byte_identical() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let inv = write_msg(root, "sent", "inv", &invite_md("u1@x", 0, &[("a@example.com", "needs-action"), ("b@example.com", "needs-action")]), Some(&invite_ics("u1@x", 0)));
        write_msg(root, "inbox", "ra", &reply_md("u1@x", 0, "a@example.com", "accepted"), Some(&reply_ics("u1@x", 0, "a@example.com", "ACCEPTED", "20260710T120000Z")));
        write_msg(root, "inbox", "rb", &reply_md("u1@x", 0, "b@example.com", "declined"), Some(&reply_ics("u1@x", 0, "b@example.com", "DECLINED", "20260710T120000Z")));

        let s1 = reconcile_account(root).unwrap();
        assert_eq!(s1.updated, 2);
        let after_first = fs::read(&inv).unwrap();

        // Second run: nothing changes, bytes are identical, and every match is
        // counted as unchanged rather than updated.
        let s2 = reconcile_account(root).unwrap();
        assert_eq!(s2.updated, 0);
        assert_eq!(s2.unchanged, 2);
        let after_second = fs::read(&inv).unwrap();
        assert_eq!(after_first, after_second, "reconciliation must be idempotent");
    }

    #[test]
    fn bump_after_sync_returns_changed_invite_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let inv = write_msg(root, "sent", "inv", &invite_md("u1@x", 0, &[("a@example.com", "needs-action")]), Some(&invite_ics("u1@x", 0)));
        write_msg(root, "inbox", "rep", &reply_md("u1@x", 0, "a@example.com", "accepted"), Some(&reply_ics("u1@x", 0, "a@example.com", "ACCEPTED", "20260710T120000Z")));

        // saw_reply=false short-circuits (no work).
        assert!(bump_after_sync(root, false).is_empty());
        // saw_reply=true reconciles and reports the touched invite.
        let changed = bump_after_sync(root, true);
        assert_eq!(changed, vec![inv.clone()]);
        // Re-running reports no changes (idempotent).
        assert!(bump_after_sync(root, true).is_empty());
        assert_eq!(status_of(&inv, "a@example.com").as_deref(), Some("accepted"));
    }
}
