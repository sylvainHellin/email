---
id: 0067
title: Contacts guard refinements (nondeterministic observed_at, corrupt cache, partial erosion)
type: bug
priority: later
status: done
created: 2026-08-06
closed: 2026-08-11
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

## Resolution (2026-08-11)

All six scope items landed, one of them differently than the ticket imagined.

1. **Deterministic `observed_at`.** The ticket's premise was stale: `messages.mtime` was dropped from the schema by the v4 bundle (#0054), so there is no ingest mtime left to expose on `MessageRow`. Instead the fallback is a constant, `extractor::UNDATED_OBSERVED_AT` (the epoch), which is the same rule the store applies to the same rows (`date_sort = 0` sorts undated mail last). Undated mail now sinks in the frecency tiebreaker instead of floating, and it is identical on every rebuild.
2. **Corrupt cache degrades.** `cache::cached_count` turns a read or parse error into a `warn!` plus zero kept, so `mp contacts rebuild` repairs an unparseable `contacts-cache.json` instead of failing on it.
3. **Partial-erosion guard.** `CacheSave::RefusedShrunk { kept, rebuilt }` refuses a rebuild that finds under `SHRINK_REFUSE_RATIO` (20%) of the cached corpus, and both the CLI and the TUI say which numbers they saw. A modest shrink still writes.
4. **Save errors are visible.** `App::refresh_contacts` returns on the `Err` arm rather than falling through to the success status, and `contacts_cmd::load_or_build` surfaces a refusal and returns what the disk holds rather than the refused rebuild.
5. **Missing-store branch covered** by `an_account_with_no_store_builds_an_empty_index`.
6. **`default_from` is parsed as an address** (`extractor::self_address`), so a `Name <addr>` config filters the user's own address out. Configured aliases are still not consulted: the config has no alias field today, so there was nothing to read.
