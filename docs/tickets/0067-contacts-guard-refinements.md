---
id: 0067
title: Contacts guard refinements (nondeterministic observed_at, corrupt cache, partial erosion)
type: bug
priority: later
status: open
created: 2026-08-06
---

Deferred notes from the fresh-context review of [#0053](0053-contacts-rebuild-data-loss.md) (commit `f7dd645`).
The data-loss fix itself is sound; these are the edges the ticket did not scope.
All small, none urgent, batched because they live in three files.

## Evidence

- `src/contacts/extractor.rs:47-50`: the `observed_at` fallback changed from file mtime to `Utc::now()`.
  A row whose `Date:` header is absent or unparseable now gets `last_seen = now`, so it floats to the top of the frecency tiebreaker inside its tier and gets a different stamp on every rebuild, making the index nondeterministic.
  Ingest already records the analogue of the old fallback in `messages.mtime` and marks unparseable dates with `date_sort = 0`, but `MessageRow` does not expose `mtime`.
- `src/contacts/cache.rs:48`: `load_cache(account_root)?` inside the guard means a corrupt or unreadable `contacts-cache.json` turns an *empty* rebuild into a hard error (`mp contacts rebuild` fails, the TUI shows "Contacts cache save failed"), and the user cannot repair by rebuilding.
  A `warn!` plus treat-as-zero-kept degrades better.
  Narrow: only when the rebuild is also empty.
- `src/contacts/cache.rs:47`: the guard is all-or-nothing.
  A *partial* read (store pruned, one mailbox's rows missing) still replaces 1735 contacts with 3 and reports success.
  #0053 scoped only the zero case, but the lessons-learned framing ("a rebuild is a deletion") argues for a ratio or a merge guard.
- `src/tui/app/mod.rs:474-477`: on `Err(e)` from the save, the error status is overwritten two lines later by `set_status("Contacts refreshed (n)")`, so a save failure is invisible to the user.
  Pre-existing shape, faithfully preserved by #0053; a `return` in that arm fixes it.
- `src/contacts_cmd.rs:156-157`: `load_or_build` discards the `CacheSave` outcome and returns the freshly built, possibly empty, index.
  Unreachable today because it only runs with no cache on disk, but if the guard ever fires there the command prints 0 contacts while the disk holds N.
- Coverage gap: the new extractor tests target `build_index_from_store`, so the `build_index_for_account` wiring (`config::store_path` -> `open_store` -> missing-store-means-empty, `src/contacts/extractor.rs:39-43`) is only covered indirectly.
  A one-line test for the missing-store branch is cheap.
- Pre-existing, untouched by #0053: self-filtering compares the lowercased `default_from` verbatim (`src/contacts/extractor.rs:126-128`), so an alias or a `Name <addr>`-formatted `default_from` does not filter the user's own address out of the corpus.

## Scope

1. Give `MessageRow` the ingest mtime (or the `date_sort = 0` marker) and use it as the `observed_at` fallback, so a rebuild is deterministic.
2. Degrade a corrupt cache to `warn!` plus zero-kept inside the empty-rebuild guard.
3. Add a ratio or merge guard so a partial rebuild cannot erode a populated corpus.
4. Return early on a save error in the TUI refresh path, and surface the `CacheSave` outcome from `load_or_build`.
5. Test the missing-store branch of `build_index_for_account`.
6. Parse `default_from` as an address (and consider configured aliases) before self-filtering.

## Acceptance criteria

- Two consecutive rebuilds of an account with undated mail produce the same index.
- A corrupt `contacts-cache.json` does not make `mp contacts rebuild` fail on an empty store.
- A rebuild that finds a fraction of the previous corpus refuses or merges rather than replacing, and says which.
- A `Name <addr>` `default_from` filters the user's own address out.
