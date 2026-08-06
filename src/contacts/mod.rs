//! Contact index built from the local message store.
//!
//! Reads each account's message rows, aggregates from/to/cc addresses, filters
//! noise, ranks with a tiered comparator (sent > cc > received) with frecency
//! tiebreaker, and caches results to JSON.

mod cache;
mod extractor;
mod filter;
pub mod hooks;
mod matcher;
mod rank;
mod types;
mod vcard;

pub use cache::{cache_path, load_cache, save_cache, save_rebuilt_cache, CacheSave};
pub use extractor::{build_index_for_account, observe, ObservedIn};
pub use matcher::{search, MatchResult};
pub use types::{Contact, ContactIndex, ContactSource, ContactTier};
pub use vcard::{contact_to_vcard, vcard_file_stem};
