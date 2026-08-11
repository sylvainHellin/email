---
id: 0060
title: Enforce the retention policy (config is parsed and validated but nothing evicts)
type: feature
priority: later
status: done
created: 2026-08-06
closed: 2026-08-07
---

## Resolution

Shipped the eviction sweep (`src/store/sweep.rs`), wired after every `mp sync`
and exposed as `mp store gc`. `mp config show` reports retention as enforced.

### Policy, signed off by Sylvain

- **Default cap 10 GB** (`max_disk_bytes`; per-account override still wins). The
  pre-enforcement default was 5 GB, a number nothing acted on.
- **Sweep runs after every sync and via a manual `mp store gc`.**
- **Two-strike marker.** The first over-cap run *warns only* -- a visible
  `store at X / cap Y, will prune on next run` plus a persisted, store-level
  marker in `meta` (`retention_over_cap`), not an in-memory flag, so the *next*
  over-cap run (even a separate `mp` invocation) evicts. Dropping back under the
  cap clears the marker.
- **Eviction order.** Age horizon first (attachments past
  `attachment_horizon_days`, then bodies past `body_horizon_days`), then
  attachment blobs oldest-first, then body blobs oldest-first, stopping the
  moment the store is back under the cap. A blob's age is its freshest
  referencing message, so a shared blob survives while any referencing message
  is still inside its horizon (refcount respected).
- **Never deletes `messages` rows.** Only blob files and their `blobs` refcount
  rows go; the listing stays complete.
- **`mp store gc --dry-run`** prints what would go. A plan reclaiming more than
  half the store's blob bytes is refused without `--force`.

### Deferred: one acceptance criterion is only partially met (-> #0085)

"Opening an evicted message re-fetches and re-materialises its body" is **not**
fully satisfied. The store degrades gracefully (an evicted body shows empty) and
a *re-ingest of the same message* re-materialises the body (pinned by
`an_evicted_body_re_materialises_on_re_acquire` and
`reingest_after_the_old_body_blob_is_evicted_leaves_one_fts_entry`), but there is
no automatic on-open server fetch: a plain `mp sync` skips a UID it already holds
a row for (`ingest::known_uids`), so it does not re-download an evicted body. A
blob-aware download-skip would be *wrong* for retention (it would re-download
everything the sweep just evicted, an evict/re-download churn), so the design
intent recorded at `src/config.rs` (`RetentionPolicy` docs) is on-demand fetch.
That missing half is **#0085**, and is REQUIRED before anyone lowers a cap below
their working set. The `>50%` `--force` guard on `mp store gc` is the interim
safety net.
---

From the architecture review synthesis, Tier 3: [2026-08-06_architecture-review-synthesis](../../.agents/handoff/2026-08-06_architecture-review-synthesis.md).
Effort: M for the sweep, S for the interim note.

The retention knob is fully implemented up to the point where it would do something.
The user sets a cap, the value is validated, and the blob store then grows without bound.

## Evidence

- `src/config.rs:31-34` global `[retention]` table, `:88-91` per-account overrides, `:271` `RetentionPolicy` as a fully resolved policy with no optionals.
- `src/config.rs:352` `retention_for` layers account overrides over the global table, and `:506` plus `:515` `validate_retention` rejects out-of-range values at load time.
- No caller evicts. `rg 'retention_for' src/` reaches only the config module and its tests; nothing in `src/store/blobs.rs` consults a policy.
- The design intent is on record at `src/config.rs:220`: bodies are re-fetched on open, which is what makes retention a safe user-facing knob.

## Scope

1. Interim, shippable immediately: `mp config show` labels the retention block as configured but not yet enforced, so the setting stops implying a guarantee.
2. Eviction sweep over the blob store honouring the resolved `RetentionPolicy`: age horizon first, then `max_disk_bytes` oldest-first until under the cap.
3. Evict blobs only, never message rows: the listing stays complete and an opened message re-fetches its body.
4. Respect `refcount` in `blobs` (`src/store/schema.rs:166-170`) so a blob shared by several messages is not evicted while still referenced.
5. Run the sweep after sync, and expose it as `mp store gc` or the equivalent for a manual run.

## Acceptance criteria

- A store over its `max_disk_bytes` cap shrinks below it after a sync, and the message list is unchanged.
- Opening an evicted message re-fetches and re-materialises its body.
- A blob referenced by two messages survives eviction of one of them.
- Until the sweep ships, `mp config show` does not present retention as enforced.
