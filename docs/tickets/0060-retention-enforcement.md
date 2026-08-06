---
id: 0060
title: Enforce the retention policy (config is parsed and validated but nothing evicts)
type: feature
priority: later
status: open
created: 2026-08-06
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
