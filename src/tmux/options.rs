//! Single source of truth for every tmux option this plugin reads or writes.
//!
//! Three groups, and the split matters:
//!
//! 1. **Daemon-owned pane options** — the *resolved* status. The daemon is the
//!    only writer; everything else reads. This is what makes N open sidebars
//!    cost one poller instead of N.
//! 2. **Hook-owned pane options** — raw facts pushed by the agent. Hooks never
//!    write resolved state; the daemon reconciles these with passive detection
//!    and owns the liveness sweep that clears them when the agent process is
//!    gone (see [`crate::daemon`]).
//! 3. **User configuration** — read-only from our side, seeded with defaults by
//!    `agent-mgr.conf`.
//!
//! Keeping the two pane namespaces separate is what makes precedence explicit
//! and lets a stale hook write be cleaned up without disturbing passive state.

use super::commands::{run_tmux, unset_pane_option_raw};

// ─── 1. daemon-owned pane options (resolved status) ──────────────────

/// Agent detected in this pane (`claude` / `codex`), empty when none.
pub const PANE_AGENT: &str = "@agent_mgr_agent";
/// Resolved [`crate::model::AgentState`] label.
pub const PANE_STATE: &str = "@agent_mgr_state";
/// Which source won for this pane: `hook` or `passive`.
pub const PANE_SOURCE: &str = "@agent_mgr_source";
/// `0` while a finished run has not been looked at yet, `1` otherwise.
pub const PANE_SEEN: &str = "@agent_mgr_seen";
/// Epoch seconds the current run started; empty when not running.
pub const PANE_RUN_STARTED_AT: &str = "@agent_mgr_run_started_at";
/// Epoch seconds of the last status write. Diagnostic only — lets you tell a
/// wedged daemon from a genuinely quiet one.
pub const PANE_UPDATED: &str = "@agent_mgr_updated";

// ─── 2. hook-owned pane options (raw facts) ──────────────────────────

/// Agent name as its own hooks report it. Presence proves hooks are wired up.
pub const PANE_HOOK_AGENT: &str = "@agent_mgr_hook_agent";
/// Raw hook status label (`running` / `waiting` / `background` / `idle` /
/// `error`), parsed by [`crate::model::parse_hook_state`].
pub const PANE_HOOK_STATE: &str = "@agent_mgr_hook_state";
/// Epoch seconds of the last hook write, used to age out hook state.
pub const PANE_HOOK_UPDATED: &str = "@agent_mgr_hook_updated";
/// Permission mode label (`plan`, `acceptEdits`, `bypassPermissions`, …).
pub const PANE_PERMISSION_MODE: &str = "@agent_mgr_permission_mode";
/// Why the pane is blocked (`permission`, `idle_prompt`, …).
pub const PANE_WAIT_REASON: &str = "@agent_mgr_wait_reason";
/// Comma-separated `Type:id` list of live subagents.
pub const PANE_SUBAGENTS: &str = "@agent_mgr_subagents";
/// Completed task count for the current task list.
pub const PANE_TASK_DONE: &str = "@agent_mgr_task_done";
/// Total task count for the current task list.
pub const PANE_TASK_TOTAL: &str = "@agent_mgr_task_total";
/// Most recent backgrounded shell command, sanitized to one line.
pub const PANE_BG_CMD: &str = "@agent_mgr_bg_cmd";
/// Agent-reported session id.
pub const PANE_SESSION_ID: &str = "@agent_mgr_session_id";
/// Agent-reported working directory, preferred over `pane_current_path` for the
/// git lookup because it tracks the agent's cwd rather than the shell's.
pub const PANE_CWD: &str = "@agent_mgr_cwd";

/// Every hook-owned key, swept together by the daemon's liveness check and by
/// `SessionEnd`. Kept as one list so adding a key above can't be forgotten here.
pub const HOOK_OWNED_PANE_OPTIONS: &[&str] = &[
    PANE_HOOK_AGENT,
    PANE_HOOK_STATE,
    PANE_HOOK_UPDATED,
    PANE_PERMISSION_MODE,
    PANE_WAIT_REASON,
    PANE_SUBAGENTS,
    PANE_TASK_DONE,
    PANE_TASK_TOTAL,
    PANE_BG_CMD,
    PANE_SESSION_ID,
    PANE_CWD,
];

// ─── plugin-internal bookkeeping ─────────────────────────────────────

/// Marks the sidebar's own pane so the TUI can exclude itself from the list and
/// `toggle` can find the pane to kill.
pub const PANE_ROLE: &str = "@agent_mgr_pane_role";
/// Value written to [`PANE_ROLE`] for a sidebar pane.
pub const PANE_ROLE_SIDEBAR: &str = "sidebar";
/// PID of the TUI running in this pane, so tmux focus hooks can SIGUSR1 it for
/// an instant refresh instead of us polling faster.
pub const PANE_TUI_PID: &str = "@agent_mgr_pid";
/// PID of the status daemon, held globally so a second instance stands down.
pub const DAEMON_PID: &str = "@agent_mgr_daemon_pid";
/// Per-window rolled-up status icon, interpolated into `window-status-format`.
pub const WINDOW_ICON: &str = "@agent_mgr_window_icon";
/// Per-session display rank, low first.
///
/// tmux has no notion of session order — `list-sessions` is alphabetical — so this
/// is the sidebar's own ordering and affects nothing outside it. Stored on the
/// session so it survives for the tmux server's lifetime and is shared by every
/// sidebar and popup, rather than each instance keeping its own idea of the order.
pub const SESSION_ORDER: &str = "@agent_mgr_session_order";

// ─── 3. user configuration ───────────────────────────────────────────

/// Sidebar width: a column count or `N%` of the window width.
pub const CFG_WIDTH: &str = "@agent_mgr_width";
/// Lower clamp on the resolved width.
pub const CFG_MIN_WIDTH: &str = "@agent_mgr_min_width";
/// Upper clamp on the resolved width. Unset means uncapped.
pub const CFG_MAX_WIDTH: &str = "@agent_mgr_max_width";
/// Which side the sidebar pane is created on: `left` or `right`.
pub const CFG_POSITION: &str = "@agent_mgr_position";
/// When `on`, list only panes running an agent instead of every pane.
pub const CFG_AGENTS_ONLY: &str = "@agent_mgr_agents_only";
/// Path to the global notes file. A leading `~/` is expanded; unset means the
/// XDG default. Global rather than per-session on purpose — see [`crate::notes`].
pub const CFG_NOTES_FILE: &str = "@agent_mgr_notes_file";
// These two are consumed only by `agent-mgr.conf` — the tab glyph is appended to
// `window-status-format` and the nav keys are bound, both in tmux config rather
// than in Rust. They are declared here so the option surface lives in one place,
// and `every_config_option_is_referenced_by_the_shipped_conf` below fails if a
// rename here ever leaves the conf behind.
#[allow(dead_code, reason = "read by agent-mgr.conf, never by Rust")]
pub mod conf_only {
    /// When `on`, append the rolled-up status icon to `window-status-format`.
    pub const CFG_TAB_STATUS: &str = "@agent_mgr_tab_status";
    /// When `on`, bind vim-aware `C-h/j/k/l` pane and session navigation.
    pub const CFG_NAV: &str = "@agent_mgr_nav";
    /// Prefix key toggling the sidebar in the current window.
    pub const CFG_KEY: &str = "@agent_mgr_key";
    /// Prefix key toggling the sidebar in every window.
    pub const CFG_KEY_ALL: &str = "@agent_mgr_key_all";
    /// Prefix key selecting the sidebar, or hopping back out of it when you are
    /// already in it; opens one first if this window has none.
    pub const CFG_KEY_FOCUS: &str = "@agent_mgr_key_focus";
    /// Prefix-less key opening the full-screen popup; `none` binds nothing.
    pub const CFG_KEY_POPUP: &str = "@agent_mgr_key_popup";
    /// Absolute path to the resolved `agent-mgr` binary, published by
    /// `tmux-agent-mgr.tmux` at load. The conf's key bindings read it, and so
    /// does `hook.sh` — asking tmux on every fire is what lets the binary be
    /// rebuilt or relocated without regenerating the agent's hook config.
    pub const BIN: &str = "@agent_mgr_bin";
    /// Whether this tmux can host the popup surface (`display-popup -B -E`,
    /// tmux >= 3.3). Written by `tmux-agent-mgr.tmux` at load, read by the conf to
    /// decide whether binding the popup key would produce a working key or one
    /// that only fails when pressed.
    pub const HAS_POPUP: &str = "@agent_mgr_has_popup";
}

/// Read a global option, treating empty as unset.
pub fn global(name: &str) -> Option<String> {
    run_tmux(&["show", "-gqv", name])
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// Read a global option as a boolean. Anything but an explicit `off` / `false` /
/// `0` counts as on, so a typo leaves a default-on feature on rather than
/// silently disabling it.
pub fn global_bool(name: &str, default: bool) -> bool {
    match global(name) {
        Some(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "off" | "false" | "0" | "no"
        ),
        None => default,
    }
}

/// Clear every hook-owned option on a pane. Used by `SessionEnd` and by the
/// daemon when it finds no agent process left under the pane.
pub fn clear_hook_state(pane_id: &str) {
    for key in HOOK_OWNED_PANE_OPTIONS {
        unset_pane_option_raw(pane_id, key);
    }
}

/// The tmux config we ship, embedded so tests can check it against the constants
/// above. Several options here are only ever *read* by that file, never by Rust,
/// so without this the two could silently drift apart.
#[cfg(test)]
const SHIPPED_CONF: &str = include_str!("../../agent-mgr.conf");

/// The TPM entry point, for the same reason: it is the only writer of
/// [`conf_only::HAS_POPUP`], and the conf is its only reader.
#[cfg(test)]
const SHIPPED_TMUX: &str = include_str!("../../tmux-agent-mgr.tmux");

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn hook_owned_list_covers_every_hook_key_exactly_once() {
        // Guards the "add a const above, forget to add it to the sweep list"
        // failure, which would leave stale detail on a dead pane forever.
        let declared: HashSet<&str> = HOOK_OWNED_PANE_OPTIONS.iter().copied().collect();
        assert_eq!(
            declared.len(),
            HOOK_OWNED_PANE_OPTIONS.len(),
            "duplicate entry in HOOK_OWNED_PANE_OPTIONS"
        );
        for key in [
            PANE_HOOK_AGENT,
            PANE_HOOK_STATE,
            PANE_HOOK_UPDATED,
            PANE_PERMISSION_MODE,
            PANE_WAIT_REASON,
            PANE_SUBAGENTS,
            PANE_TASK_DONE,
            PANE_TASK_TOTAL,
            PANE_BG_CMD,
            PANE_SESSION_ID,
            PANE_CWD,
        ] {
            assert!(declared.contains(key), "{key} missing from the sweep list");
        }
    }

    #[test]
    fn every_config_option_is_referenced_by_the_shipped_conf() {
        // `@agent_mgr_tab_status` and `@agent_mgr_nav` are read only by
        // agent-mgr.conf; renaming the const here without updating the conf would
        // silently turn the feature off, with nothing to notice it.
        for key in [
            CFG_WIDTH,
            CFG_MIN_WIDTH,
            CFG_MAX_WIDTH,
            CFG_POSITION,
            CFG_AGENTS_ONLY,
            CFG_NOTES_FILE,
            conf_only::CFG_TAB_STATUS,
            conf_only::CFG_NAV,
            conf_only::CFG_KEY,
            conf_only::CFG_KEY_ALL,
            conf_only::CFG_KEY_FOCUS,
            conf_only::CFG_KEY_POPUP,
            conf_only::HAS_POPUP,
            conf_only::BIN,
        ] {
            assert!(
                SHIPPED_CONF.contains(key),
                "{key} is not mentioned in agent-mgr.conf"
            );
        }
    }

    #[test]
    fn the_binary_path_is_published_by_the_entrypoint() {
        // `hook.sh` and every key binding resolve the binary through this option,
        // so the entry point renaming it would break all three at once.
        assert!(
            SHIPPED_TMUX.contains(conf_only::BIN),
            "tmux-agent-mgr.tmux must publish {}",
            conf_only::BIN
        );
    }

    #[test]
    fn the_popup_capability_flag_is_written_by_the_entrypoint_and_read_by_the_conf() {
        // Split across the bash/tmux boundary with no compiler between them: the
        // entry point decides, the conf acts on it. A rename on either side would
        // silently stop binding the popup key.
        assert!(
            SHIPPED_TMUX.contains(conf_only::HAS_POPUP),
            "tmux-agent-mgr.tmux must publish {}",
            conf_only::HAS_POPUP
        );
        assert!(SHIPPED_CONF.contains(conf_only::HAS_POPUP));
    }

    #[test]
    fn the_conf_binds_every_subcommand_main_dispatches_for_a_key() {
        // Subcommand names are strings in a tmux binding on one side and match arms
        // in main.rs on the other, with nothing tying them together. A rename would
        // leave a key that only fails when pressed.
        for (command, arity) in [
            ("toggle \\\"##{window_id}\\\"", "window and path"),
            ("toggle-all \\\"##{window_id}\\\"", "the initiating window"),
            ("focus \\\"##{window_id}\\\" \\\"##{pane_id}\\\"", "window and pane"),
        ] {
            assert!(
                SHIPPED_CONF.contains(command),
                "the conf does not invoke `{command}` with {arity}"
            );
        }
    }

    #[test]
    fn the_conf_binds_the_popup_subcommand_main_actually_dispatches() {
        // `agent-mgr popup` is a string in a tmux binding on one side and a match
        // arm in main.rs on the other; nothing else ties them together.
        assert!(
            SHIPPED_CONF.contains("popup"),
            "the conf must invoke the popup subcommand"
        );
        assert!(
            SHIPPED_CONF.contains("display-popup"),
            "the popup key must open a display-popup"
        );
    }

    #[test]
    fn the_conf_wires_up_the_options_the_daemon_and_tui_maintain() {
        // The focus hooks signal the pid we publish, and the tab glyph is the
        // window option the daemon writes. Both are string-matched across the
        // Rust/tmux boundary, so a rename has to be caught here.
        assert!(SHIPPED_CONF.contains(PANE_TUI_PID));
        assert!(SHIPPED_CONF.contains(WINDOW_ICON));
    }

    #[test]
    fn every_focus_hook_is_removed_before_it_is_re_added() {
        // tmux has no "replace this hook" verb, so a reload of a config that only
        // appends stacks another copy and signals every sidebar N times per focus
        // change. The three together are what makes the cursor follow tmux focus:
        // client-session-changed is the C-j/C-k case neither select hook fires.
        for hook in [
            "after-select-pane",
            "after-select-window",
            "client-session-changed",
        ] {
            assert!(
                SHIPPED_CONF.contains(&format!("set-hook -ga {hook}")),
                "{hook} is never set"
            );
            assert!(
                SHIPPED_CONF.contains(&format!("show-hooks -g {hook}")),
                "{hook} is set but not removed first — a reload would duplicate it"
            );
        }
    }

    #[test]
    fn the_conf_never_hardcodes_a_max_width_default() {
        // An unset max means uncapped; seeding it would silently cap every
        // sidebar at whatever number we picked.
        assert!(
            !SHIPPED_CONF.contains(&format!("set -g {CFG_MAX_WIDTH}")),
            "max width must stay unset by default"
        );
    }

    #[test]
    fn daemon_and_hook_namespaces_do_not_overlap() {
        // Precedence only works if the two writers can't clobber each other.
        let daemon: HashSet<&str> = [
            PANE_AGENT,
            PANE_STATE,
            PANE_SOURCE,
            PANE_SEEN,
            PANE_RUN_STARTED_AT,
            PANE_UPDATED,
        ]
        .into_iter()
        .collect();
        for key in HOOK_OWNED_PANE_OPTIONS {
            assert!(
                !daemon.contains(key),
                "{key} is written by both the daemon and hooks"
            );
        }
    }
}
