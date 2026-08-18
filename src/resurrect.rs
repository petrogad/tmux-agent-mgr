//! Bringing sidebars back after `tmux-resurrect` restores a session.
//!
//! resurrect restores the *pane* — layout, index, width, cwd — but not the
//! process, so a window that had a sidebar comes back with a plain shell where
//! the sidebar was. That is not a configuration mistake, and no value of
//! `@resurrect-processes` fixes it: all three of resurrect's save-command
//! strategies (`ps`, `pgrep`, `linux_procfs`) read the *children* of `pane_pid`,
//! and our sidebar has none — it **is** the pane process. So resurrect saves an
//! empty full-command for it, and `restore.sh` skips every pane whose
//! full-command is empty.
//!
//! What resurrect does record is `pane_current_command`, which for a sidebar pane
//! is our binary's name. That is the handle this module uses: read resurrect's
//! own snapshot — the same one the restored layout came from — find the panes that
//! were sidebars, and respawn the binary into them.
//!
//! `respawn-pane -k` is deliberate. It keeps the pane's index and width (so the
//! restored geometry is left exactly as resurrect set it) and, unlike killing the
//! pane, does **not** fire `pane-exited` — which means it cannot trip
//! [`crate::pane::cmd_auto_close`] into closing the window. For the same reason
//! this never pre-sets `@agent_mgr_pane_role`: the sidebar claims the role itself
//! on startup (see `main.rs`), so a pane that is briefly a shell is briefly an
//! ordinary pane, which is the safe way round.
//!
//! One thing this deliberately does *not* guard: a manual `prefix C-r` into a live
//! server whose layout has drifted since the snapshot, where the target index may
//! now hold something else. No extra check is warranted, because by the time this
//! runs resurrect has already applied the saved layout and window names over that
//! same drifted window — "make this server look like the snapshot" is the contract
//! the user invoked, and this completes it rather than adding to it. The
//! already-running check below is what keeps the ordinary case a true no-op.

use std::path::PathBuf;
use std::process::Command;

use crate::tmux;

/// Where resurrect keeps its snapshots, if the user moved it.
const DIR_OPTION: &str = "@resurrect-dir";
/// resurrect symlinks its newest snapshot to this name inside the save dir.
const LATEST_SNAPSHOT: &str = "last";
/// Fields in a `pane` line of a snapshot. A line with any other count is not one
/// we understand, so it is skipped rather than guessed at.
const PANE_FIELDS: usize = 11;

/// Field offsets within a snapshot `pane` line, in `save.sh`'s `pane_format`
/// order. Only the four we need are named.
mod field {
    /// Line type: `pane`, `window`, `state`, `grouped_session`.
    pub const KIND: usize = 0;
    pub const SESSION: usize = 1;
    pub const WINDOW: usize = 2;
    pub const PANE_INDEX: usize = 5;
    /// `pane_current_command` — the only field that identifies a sidebar.
    pub const COMMAND: usize = 9;
}

/// `agent-mgr restore` — re-open the sidebars a resurrect restore left as shells.
///
/// Wired to `@resurrect-hook-post-restore-all` by `agent-mgr.conf`, and safe to
/// run by hand at any time: a pane already running the sidebar is left alone, so
/// running it twice is a no-op.
///
/// Silent and always 0 on every path. resurrect `eval`s this hook inside
/// `restore.sh`, which itself runs under `run-shell`, so anything printed here
/// would surface as a tmux message in the user's face at the end of a restore.
pub fn cmd_restore(_args: &[&str]) -> i32 {
    let Some(snapshot) = save_file() else {
        return 0;
    };
    let Ok(contents) = std::fs::read_to_string(&snapshot) else {
        // No snapshot means resurrect has never saved, or is not installed.
        return 0;
    };

    let exe = current_exe();
    let command = binary_name(&exe);

    for target in sidebar_targets(&contents, &command) {
        if tmux::display_message(&target, "#{pane_current_command}") == command {
            continue;
        }
        // Before the respawn, so it cannot race the sidebar's own output: with
        // `@resurrect-capture-pane-contents on` the pane was refilled with a
        // frozen frame of the *old* sidebar, and that would otherwise sit in the
        // new one's scrollback.
        tmux::run_tmux_quiet(&["clear-history", "-t", &target]);
        tmux::run_tmux_quiet(&["respawn-pane", "-k", "-t", &target, &exe]);
    }
    0
}

/// Panes that were sidebars, as `session:window.pane` targets.
///
/// Pure, so the snapshot format is pinned by tests rather than by a live restore.
/// `command` is our binary's name as `pane_current_command` would report it.
///
/// Fields are positional and tab-separated. A pane title or session name
/// containing a tab would shift the columns, so the field count is checked
/// exactly and the numeric fields must parse — a mis-parsed line here would aim a
/// `respawn-pane -k` at somebody's editor.
fn sidebar_targets(snapshot: &str, command: &str) -> Vec<String> {
    snapshot
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            if fields.len() != PANE_FIELDS
                || fields[field::KIND] != "pane"
                || fields[field::COMMAND] != command
                || fields[field::SESSION].is_empty()
            {
                return None;
            }
            let window: u32 = fields[field::WINDOW].parse().ok()?;
            let pane: u32 = fields[field::PANE_INDEX].parse().ok()?;
            Some(format!("{}:{window}.{pane}", fields[field::SESSION]))
        })
        .collect()
}

/// Path of resurrect's newest snapshot, or `None` when it cannot be determined.
fn save_file() -> Option<PathBuf> {
    let option = tmux::run_tmux(&["show-option", "-gqv", DIR_OPTION])?
        .trim()
        .to_owned();
    let home = std::env::var("HOME").ok()?;
    // Only pay for a subprocess when the option actually asks for the hostname.
    let hostname = if option.contains("$HOSTNAME") {
        hostname()
    } else {
        String::new()
    };
    let legacy = PathBuf::from(&home).join(".tmux/resurrect");

    Some(
        save_dir(
            &option,
            &home,
            std::env::var("XDG_DATA_HOME").ok().as_deref(),
            &hostname,
            legacy.is_dir(),
        )
        .join(LATEST_SNAPSHOT),
    )
}

/// Resolve resurrect's save directory the way its own `helpers.sh` does.
///
/// Pure: every input is an argument, because getting the fallback order wrong
/// means silently reading somebody else's snapshot, or none. Note that the legacy
/// `~/.tmux/resurrect` wins over the XDG path *only when it already exists* —
/// that is resurrect's rule, not ours.
fn save_dir(
    option: &str,
    home: &str,
    xdg: Option<&str>,
    hostname: &str,
    legacy_exists: bool,
) -> PathBuf {
    let option = option.trim();
    if !option.is_empty() {
        return PathBuf::from(expand(option, home, hostname));
    }
    if legacy_exists {
        return PathBuf::from(home).join(".tmux/resurrect");
    }
    match xdg.map(str::trim).filter(|xdg| !xdg.is_empty()) {
        Some(xdg) => PathBuf::from(xdg).join("tmux/resurrect"),
        None => PathBuf::from(home).join(".local/share/tmux/resurrect"),
    }
}

/// Expand the three things resurrect expands in `@resurrect-dir`. `~` is replaced
/// everywhere it appears, not just at the front — sloppy, but it is what
/// resurrect's `sed` does, and disagreeing would put us in a different directory.
fn expand(path: &str, home: &str, hostname: &str) -> String {
    path.replace("$HOME", home)
        .replace("$HOSTNAME", hostname)
        .replace('~', home)
}

fn hostname() -> String {
    Command::new("hostname")
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .unwrap_or_default()
}

/// Our own path, to respawn into the pane. Same fallback as
/// [`crate::pane`] uses when creating a sidebar.
fn current_exe() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.to_str().map(str::to_owned))
        .unwrap_or_else(|| "agent-mgr".to_owned())
}

/// The name tmux would report as `pane_current_command` for [`current_exe`].
/// Derived rather than hardcoded so a renamed binary matches its own snapshots.
fn binary_name(exe: &str) -> String {
    exe.rsplit('/').next().unwrap_or(exe).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lines in resurrect's real format, taken from an actual snapshot: two
    /// sidebars, an ordinary pane, and the other three line types.
    const SNAPSHOT: &str = "\
grouped_session\tclone\toriginal\t:1\t:2
pane\tgroups\t1\t0\t:-\t1\thost\t:/home/me/work\t0\tagent-mgr\t:
pane\tgroups\t1\t0\t:-\t2\thost\t:/home/me/work\t1\tzsh\t:nvim src/main.rs
pane\tvoice\t2\t1\t:*\t1\thost\t:/home/me/other\t1\tagent-mgr\t:
window\tvoice\t2\t:agent-mgr\t1\t:*\tb693,302x82,0,0{45x82,0,0,13,256x82,46,0,8}\t:
state\tvoice\tgroups";

    #[test]
    fn every_sidebar_pane_becomes_a_target_and_nothing_else_does() {
        assert_eq!(
            sidebar_targets(SNAPSHOT, "agent-mgr"),
            vec!["groups:1.1", "voice:2.1"]
        );
    }

    #[test]
    fn a_window_line_naming_our_binary_is_not_a_pane() {
        // `automatic-rename` names a window after the focused pane's command, so
        // a window called `agent-mgr` is saved right next to the panes and has
        // our binary's name in it. Matching on it would respawn a stranger.
        assert!(!sidebar_targets(SNAPSHOT, "agent-mgr").contains(&"voice:2.:*".to_owned()));
        assert!(sidebar_targets("window\tvoice\t2\t:agent-mgr\t1\t:*\tlayout\t:", "agent-mgr").is_empty());
    }

    #[test]
    fn a_line_with_the_wrong_field_count_is_skipped() {
        // A tab in a pane title or session name shifts every column after it.
        // Guessing at that would point `respawn-pane -k` somewhere arbitrary.
        let shifted = "pane\tgroups\t1\t0\t:-\t1\tho\tst\t:/home/me\t0\tagent-mgr\t:";
        assert!(sidebar_targets(shifted, "agent-mgr").is_empty());
        assert!(sidebar_targets("pane\tgroups\t1", "agent-mgr").is_empty());
        assert!(sidebar_targets("", "agent-mgr").is_empty());
    }

    #[test]
    fn a_non_numeric_window_or_pane_index_is_skipped() {
        let junk = "pane\tgroups\tX\t0\t:-\t1\thost\t:/home/me\t0\tagent-mgr\t:";
        assert!(sidebar_targets(junk, "agent-mgr").is_empty());
        let junk = "pane\tgroups\t1\t0\t:-\tY\thost\t:/home/me\t0\tagent-mgr\t:";
        assert!(sidebar_targets(junk, "agent-mgr").is_empty());
    }

    #[test]
    fn an_empty_session_name_is_skipped() {
        // `:1.1` would resolve against the current session — a pane we were never
        // asked to touch.
        let nameless = "pane\t\t1\t0\t:-\t1\thost\t:/home/me\t0\tagent-mgr\t:";
        assert!(sidebar_targets(nameless, "agent-mgr").is_empty());
    }

    #[test]
    fn the_binary_name_is_what_is_matched_not_the_string_agent_mgr() {
        // A renamed or wrapped binary must match its own snapshots, so the
        // comparison follows `current_exe` rather than a hardcoded name.
        assert!(sidebar_targets(SNAPSHOT, "something-else").is_empty());
        assert_eq!(sidebar_targets(SNAPSHOT, "zsh"), vec!["groups:1.2"]);
    }

    #[test]
    fn an_explicit_resurrect_dir_wins_over_every_default() {
        assert_eq!(
            save_dir("/tmp/snapshots", "/home/me", Some("/xdg"), "", true),
            PathBuf::from("/tmp/snapshots")
        );
    }

    #[test]
    fn an_explicit_dir_expands_home_tilde_and_hostname() {
        // `@resurrect-dir` with `$HOSTNAME` is a documented way to keep separate
        // snapshots per machine; not expanding it would read the wrong file.
        assert_eq!(
            save_dir("$HOME/snap/$HOSTNAME", "/home/me", None, "box", false),
            PathBuf::from("/home/me/snap/box")
        );
        assert_eq!(
            save_dir("~/snap", "/home/me", None, "", false),
            PathBuf::from("/home/me/snap")
        );
    }

    #[test]
    fn the_legacy_dir_is_used_only_when_it_already_exists() {
        // resurrect's own rule: `~/.tmux/resurrect` is preferred if present, so a
        // long-standing install keeps its history, but a fresh one goes to XDG.
        assert_eq!(
            save_dir("", "/home/me", Some("/xdg"), "", true),
            PathBuf::from("/home/me/.tmux/resurrect")
        );
        assert_eq!(
            save_dir("", "/home/me", Some("/xdg"), "", false),
            PathBuf::from("/xdg/tmux/resurrect")
        );
    }

    #[test]
    fn an_unset_or_empty_xdg_data_home_falls_back_to_local_share() {
        assert_eq!(
            save_dir("", "/home/me", None, "", false),
            PathBuf::from("/home/me/.local/share/tmux/resurrect")
        );
        assert_eq!(
            save_dir("", "/home/me", Some("  "), "", false),
            PathBuf::from("/home/me/.local/share/tmux/resurrect")
        );
    }

    #[test]
    fn the_binary_name_is_the_last_path_segment() {
        assert_eq!(binary_name("/opt/bin/agent-mgr"), "agent-mgr");
        assert_eq!(binary_name("agent-mgr"), "agent-mgr");
    }
}
