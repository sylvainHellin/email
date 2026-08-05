//! Snapshot of the whole `mp --help` surface (#0049, unit 0b).
//!
//! `mp --help` is documented as the source of truth for the website's command
//! pages, and nothing pinned it: the CLI surface could change without a single
//! test noticing. This captures the top-level help and every subcommand's help,
//! recursively, as one document.
//!
//! parity, all of it. The new build must expose the same commands, arguments,
//! defaults and wording, or update the website pages under
//! `website/src/pages/` in the same commit. A diff here is a decision, never an
//! approval reflex.
//!
//! The help is captured by running the real binary rather than by rendering the
//! clap `Command` in-process, because the two are not the same: `--help` maps
//! to clap's short help for commands with no long help and to the long help for
//! the ones that have it (`mp dump-keys`), and an in-process `render_long_help`
//! would record a layout no user ever sees. `CARGO_BIN_EXE_mp` makes cargo
//! build the binary for this test, so no extra dependency is involved.
//! Deterministic: the `wrap_help` clap feature is off, so the width is fixed,
//! and `--help` short-circuits before any config or network access.

use std::process::Command;

const MP: &str = env!("CARGO_BIN_EXE_mp");

/// Run `mp <args> --help` and return stdout. Asserts the call succeeded and
/// wrote nothing to stderr, so a broken subcommand cannot slip in as an empty
/// section.
fn help_for(args: &[&str]) -> String {
    let out = Command::new(MP)
        .args(args)
        .arg("--help")
        .output()
        .expect("mp must run");
    assert!(out.status.success(), "mp {args:?} --help failed: {out:?}");
    assert!(
        out.stderr.is_empty(),
        "mp {args:?} --help wrote to stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("help output is UTF-8")
}

/// Subcommand names listed in the `Commands:` block of a help text, in the
/// order clap prints them. Continuation lines of a wrapped description are
/// indented past the name column, so a two-space indent identifies a name.
/// clap's auto-generated `help` subcommand is skipped: it is regenerated at
/// every nesting level and carries no information.
fn subcommand_names(help: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut in_commands = false;
    for line in help.lines() {
        if line == "Commands:" {
            in_commands = true;
            continue;
        }
        if !in_commands {
            continue;
        }
        if line.trim().is_empty() {
            break;
        }
        if !line.starts_with("  ") || line.starts_with("   ") {
            continue;
        }
        let name = line.trim_start().split_whitespace().next().unwrap_or("");
        if !name.is_empty() && name != "help" {
            names.push(name.to_string());
        }
    }
    names
}

fn collect(path: &[&str], out: &mut String) {
    let help = help_for(path);
    if path.is_empty() {
        out.push_str("$ mp --help\n");
    } else {
        out.push_str(&format!("$ mp {} --help\n", path.join(" ")));
    }
    out.push_str(&help);
    out.push('\n');

    for name in subcommand_names(&help) {
        let mut child: Vec<&str> = path.to_vec();
        child.push(&name);
        collect(&child, out);
    }
}

#[test]
fn cli_help_surface_snapshot() {
    let mut out = String::new();
    collect(&[], &mut out);
    // Sanity: the walk really recursed rather than snapshotting one screen.
    let screens = out.lines().filter(|l| l.starts_with("$ mp")).count();
    assert!(screens > 20, "the help walk collected {screens} screens");
    insta::assert_snapshot!(out);
}
