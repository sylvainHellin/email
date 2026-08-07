---
id: 0019
title: Configurable keybindings
type: feature
priority: later
status: dropped
created: 2026-05-01
---

> **Subsumed 2026-07-11 by [#0032 TUI foundation package](0032-tui-foundation-package.md).**
> The `Action` enum + single `KEYMAP` data table delivered there is the groundwork this ticket
> needs; configurable keybindings fall out of keymap-as-data. Original notes retained for record.

Allow remapping keys via `~/.config/mailypoppins/config.toml`. The current bindings are vim-flavoured and hardcoded.

## Notes

- Define an `Action` enum (already exists in `tui/app/types.rs`) and let users map keys -> actions:

  ```toml
  [keybindings]
  archive = "a"
  delete = "d"
  reply = "r"
  ```

- Validate at config load: every action must be either explicitly bound or fall back to the default.
- Conflict detection: error if two actions claim the same key.
