---
id: 0069
title: Delete the file-era invite rewriters (set_event_rsvp, set_event_attendee_status, InboxFrontmatter)
type: chore
priority: later
status: done
created: 2026-08-06
closed: 2026-08-11
---

Adjacent finding from the fresh-context review of [#0057](0057-dead-file-era-code-deletion.md), left out of that ticket's enumerated scope.
Effort: S.

Two invite rewriters and the frontmatter type their tests read back have no production caller.
They edit a `.md` file in place, which is the shape of the receive path the store cutover deleted.

## Evidence

- `src/draft.rs:665` `set_event_rsvp` and `src/draft.rs:808` `set_event_attendee_status` rewrite `event.rsvp` and an attendee's `status:` inside the frontmatter of a message file.
  Nothing in `src/` calls either outside their own tests.
- RSVP state comes from the store now: `src/tui/app/calendar_view.rs:157` sets `event.rsvp` from `reconcile::own_rsvp` off store rows, and `reconcile::apply_replies` folds the attendee statuses in from the reply index.
- `src/types.rs:163` `InboxFrontmatter` survives only as the deserialization target those tests use to read the rewritten file back (`src/draft.rs:1766`, `:1802`, `:2349`); its own doc comment says so and names this cleanup.
- The tests are the circular part: they write a file the receive path no longer writes, rewrite it, and deserialize it through a type nothing else parses.
  They pass, and they pin nothing a user can reach.

## Scope

1. Delete `draft::set_event_rsvp`, `draft::set_event_attendee_status` and `draft::AttendeeUpdate`.
2. Delete `types::InboxFrontmatter`.
3. Delete the tests of all three, rather than porting them: there is no store-era behaviour underneath them to port to.

Check for a remaining caller once more before deleting, and check the iMIP tickets ([#0031](0031-imip-cancel-update.md)) do not plan to reuse the attendee rewriter; if one does, the store-era equivalent is a row update, not a file rewrite, so the ticket should say that rather than keep this code alive.

## Acceptance criteria

- `rg 'set_event_rsvp|set_event_attendee_status|InboxFrontmatter'` returns nothing in `src/`.
- `cargo test` green, with the deleted tests gone rather than ignored.
- The TUI RSVP flow (`c` view, accept / tentative / decline on an invite) behaves identically.

## Resolution (2026-08-11)

Deleted, with one deviation.

- `draft::set_event_rsvp`, `draft::set_event_attendee_status`, `draft::AttendeeUpdate` and their fifteen tests are gone, along with the two YAML helpers only they used (`split_yaml_key`, `unquote_yaml`). `rg` confirms no remaining reference in `src/`, and no caller existed outside the deleted tests. #0031 does not plan to reuse them: it names no rewriter.
- `types::InboxFrontmatter` is gone from the production API, but not simply deleted: since this ticket was written, #0075 gave it a second and non-circular use as the parse target of `store::read`'s Markdown-rendition format-parity assertion, which pins something a user can see (`mp show`'s rendition still reads as the file-era frontmatter). The struct therefore moved into that test module as `FileEraFrontmatter`, i.e. it became the fixture it always was. Its own five serde tests in `types.rs` went with it: they exercised derives on a type with no production role.
- The lessons-learned entries that describe the deleted rewriters (#0029, #0030) are left as written: they are a record of what was learned when, not a description of current code.
