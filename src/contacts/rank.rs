//! Ranker for the contact index.
//!
//! Two-layer ordering:
//! 1. Lexicographic tier tuple: `(sent_to, sent_cc, received)` dominates.
//! 2. Frecency tiebreaker inside the same tier tuple:
//!    `score = weighted_frequency * exp(-age_days / half_life)`.
//!
//! The half-life is hardcoded at 180 days. Revisit as a config knob if
//! ranking quality is poor (Tier B follow-up).

use crate::contacts::types::{Contact, ContactSource};
use crate::types::MailboxRole;
use chrono::{DateTime, Utc};
use std::cmp::Ordering;
use std::collections::HashMap;

/// Hardcoded frecency half-life in days. Sized so that a contact with 10 hits
/// a year ago roughly ties a contact with 5 hits today (see research doc
/// `frecency-saga` thread). Revisit as a config knob if ranking quality is poor.
const FRECENCY_HALF_LIFE_DAYS: f64 = 180.0;

/// A single "observation" of an address from a parsed frontmatter.
#[derive(Debug)]
pub(crate) struct Observation {
    pub address: String,
    pub display_name: String,
    /// The mailbox this address was seen in. Only `sent` changes the ranking;
    /// every other role counts as received.
    pub mailbox_role: MailboxRole,
    /// Which frontmatter field this observation came from.
    pub field: ObservationField,
    /// RFC-3339 string from frontmatter `date` if parseable, else file mtime.
    pub observed_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ObservationField {
    From,
    To,
    Cc,
}

pub(crate) fn update_from_observation(contacts: &mut HashMap<String, Contact>, obs: Observation) {
    let entry = contacts
        .entry(obs.address.clone())
        .or_insert_with(|| Contact {
            address: obs.address.clone(),
            display_name: String::new(),
            sent_to: 0,
            sent_cc: 0,
            received: 0,
            first_seen: obs.observed_at.clone(),
            last_seen: obs.observed_at.clone(),
            source: ContactSource::Local,
        });

    // Bump the right counter based on which mailbox + which field.
    match (&obs.mailbox_role, obs.field) {
        (MailboxRole::Sent, ObservationField::To) => entry.sent_to += 1,
        (MailboxRole::Sent, ObservationField::Cc) => entry.sent_cc += 1,
        // Anything observed in inbox/archive/extra is "received" regardless of field.
        _ => entry.received += 1,
    }

    // Update first_seen (earliest) and last_seen (latest).
    if obs.observed_at < entry.first_seen {
        entry.first_seen = obs.observed_at.clone();
    }
    if obs.observed_at >= entry.last_seen {
        entry.last_seen = obs.observed_at.clone();
        // Display-name picking policy: most-recent non-empty wins.
        if !obs.display_name.trim().is_empty() {
            entry.display_name = obs.display_name;
        }
    } else if entry.display_name.is_empty() && !obs.display_name.trim().is_empty() {
        // Seeded case: no name yet and this observation has one.
        entry.display_name = obs.display_name;
    }
}

/// Compare two contacts for ranking. Higher scored goes first.
/// Tier tuple dominates; frecency is a tiebreaker inside the same tier tuple.
pub fn compare(a: &Contact, b: &Contact, now: DateTime<Utc>) -> Ordering {
    // Descending tier tuple comparison first.
    match b.tier_tuple().cmp(&a.tier_tuple()) {
        Ordering::Equal => {
            // Tiebreak on frecency (higher is better).
            let fa = frecency(a, now);
            let fb = frecency(b, now);
            fb.partial_cmp(&fa).unwrap_or(Ordering::Equal)
        }
        other => other,
    }
}

/// Frecency score for a single contact, evaluated at `now`.
/// `score = (sent_to*3 + sent_cc*2 + received) * exp(-age_days / HALF_LIFE)`
/// The tier-weighted frequency inside the formula is a safety net; the real
/// ordering is dominated by the tier tuple above.
fn frecency(c: &Contact, now: DateTime<Utc>) -> f64 {
    let last_seen = DateTime::parse_from_rfc3339(&c.last_seen)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or(now);
    let age_days = (now - last_seen).num_days().max(0) as f64;
    let freq = (c.sent_to as f64) * 3.0 + (c.sent_cc as f64) * 2.0 + (c.received as f64);
    freq * (-age_days / FRECENCY_HALF_LIFE_DAYS).exp()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(addr: &str, sent_to: u32, sent_cc: u32, received: u32, last: &str) -> Contact {
        Contact {
            address: addr.into(),
            display_name: String::new(),
            sent_to,
            sent_cc,
            received,
            first_seen: last.into(),
            last_seen: last.into(),
            source: ContactSource::Local,
        }
    }

    #[test]
    fn tier_dominates_frecency() {
        // Alice has 1 sent_to, 0 everything else. Bob has 100 received.
        // Alice should win on tier tuple despite Bob's huge received count.
        let alice = mk("alice@x", 1, 0, 0, "2026-01-01T00:00:00Z");
        let bob = mk("bob@x", 0, 0, 100, "2026-04-01T00:00:00Z");
        let now = Utc::now();
        assert_eq!(compare(&alice, &bob, now), Ordering::Less);
    }

    #[test]
    fn frecency_breaks_ties_within_tier() {
        // Both have 5 sent_to. Newer one wins.
        let old = mk("old@x", 5, 0, 0, "2025-01-01T00:00:00Z");
        let recent = mk("recent@x", 5, 0, 0, "2026-04-01T00:00:00Z");
        let now = "2026-04-08T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        assert_eq!(compare(&recent, &old, now), Ordering::Less);
    }
}
