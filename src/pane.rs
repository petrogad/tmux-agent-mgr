//! Sidebar pane lifecycle: creating it, killing it, keeping it the right width,
//! and cleaning up a window it was left alone in.
//!
//! The sidebar is a **real tmux pane** running our binary, not a popup. That is
//! what lets it stay open all day: tmux composites it like any other pane, so
//! nothing has to be repainted over the top of your work.
//!
//! Ported from `tmux-agent-sidebar`'s `src/cli/toggle.rs`, minus the
//! auto-create-on-new-window path — we only ever split on request, so a
//! declaratively built window (tmuxinator's `split-window` + `select-layout`)
//! can never have a pane injected into it mid-setup.

use std::collections::HashSet;

use crate::tmux;

/// Columns the working pane must keep. The sidebar is never allowed past
/// `window_width - this`, so it cannot squeeze your actual work off screen. This
/// guard deliberately overrides the configured minimum.
const MAIN_PANE_MIN_WIDTH: u32 = 20;
/// Floor applied when `@agent_mgr_min_width` is unset or unparseable.
const DEFAULT_MIN_WIDTH: u32 = 24;
/// Width used when `@agent_mgr_width` is neither a percentage nor a number.
const DEFAULT_EXPLICIT_WIDTH: u32 = 32;
/// Default percentage when `@agent_mgr_width` ends in `%` but the number is junk.
const DEFAULT_PERCENT: u32 = 20;

/// `agent-mgr toggle <window-id> [path]` — create the sidebar pane in a window,
/// or kill it if one is already there.
pub fn cmd_toggle(args: &[&str]) -> i32 {
    let Some(window_id) = args.first().copied() else {
        return 0;
    };
    let pane_path = args.get(1).copied().unwrap_or("~");

    if let Some(existing) = find_sidebar_pane(window_id) {
        tmux::run_tmux_quiet(&["kill-pane", "-t", &existing]);
        clear_window_rename(window_id);
        return 0;
    }

    create_sidebar(window_id, pane_path, Focus::Sidebar);
    0
}

/// `agent-mgr focus <window-id> <current-pane-id> [path]` — get into the sidebar,
/// or back out of it.
///
/// One key for the round trip: from a work pane it selects the sidebar, from inside
/// the sidebar it returns you to the pane you came from, and in a window that has no
/// sidebar it opens one and puts you in it. Anything else would make the key's effect
/// depend on state you cannot see from the keyboard.
pub fn cmd_focus(args: &[&str]) -> i32 {
    let Some(window_id) = args.first().copied() else {
        return 0;
    };
    let current = args.get(1).copied().unwrap_or_default();
    let pane_path = args.get(2).copied().unwrap_or("~");

    match focus_action(find_sidebar_pane(window_id).as_deref(), current) {
        // `-l` is the window's own last-pane memory, which is exactly "where I was
        // before I came in here" — no state of our own to keep in sync.
        FocusAction::HopBack => {
            tmux::run_tmux_quiet(&["select-pane", "-t", window_id, "-l"]);
        }
        FocusAction::Select(sidebar) => {
            tmux::run_tmux_quiet(&["select-pane", "-t", &sidebar]);
        }
        FocusAction::Create => create_sidebar(window_id, pane_path, Focus::Sidebar),
    }
    0
}

/// `agent-mgr toggle-all [window-id]` — one keystroke for the whole server.
///
/// If a sidebar exists anywhere, this turns them all off; otherwise it turns
/// them all on. Treating "any" as "on" means the key always does the thing you
/// expect after you have toggled one window individually.
///
/// `window-id` is the window the key was pressed in, and the only one whose new
/// sidebar takes focus: opening a sidebar in twelve background windows should not
/// silently move where you land in each of them.
pub fn cmd_toggle_all(args: &[&str]) -> i32 {
    let initiator = args.first().copied().unwrap_or_default();
    let listing = tmux::run_tmux(&["list-panes", "-a", "-F", &sidebar_kill_format()]).unwrap_or_default();

    if let Some(sidebars) = sidebar_panes_with_windows(&listing) {
        for (pane_id, window_id) in sidebars {
            tmux::run_tmux_quiet(&["kill-pane", "-t", &pane_id]);
            clear_window_rename(&window_id);
        }
        return 0;
    }

    let windows = tmux::run_tmux(&[
        "list-panes",
        "-a",
        "-F",
        &format!("#{{window_id}}\t{}", tmux::q("pane_current_path")),
    ])
    .unwrap_or_default();

    for (window_id, path) in unique_window_paths(&windows) {
        let focus = if window_id == initiator {
            Focus::Sidebar
        } else {
            Focus::Unchanged
        };
        create_sidebar(&window_id, &path, focus);
    }
    0
}

/// What `focus` should do, decided before anything is touched.
///
/// Split from the I/O for the same reason `should_kill_window` is: the tests must not
/// move the developer's own tmux client around.
#[derive(Clone, Debug, Eq, PartialEq)]
enum FocusAction {
    /// We are in the sidebar already — go back where we came from.
    HopBack,
    /// Select the sidebar pane that already exists.
    Select(String),
    /// No sidebar in this window; open one.
    Create,
}

fn focus_action(sidebar: Option<&str>, current: &str) -> FocusAction {
    match sidebar {
        // An empty `current` cannot match a real pane id, so a binding that failed to
        // pass one still focuses rather than hopping somewhere unasked.
        Some(sidebar) if !current.is_empty() && sidebar == current => FocusAction::HopBack,
        Some(sidebar) => FocusAction::Select(sidebar.to_owned()),
        None => FocusAction::Create,
    }
}

/// `agent-mgr resize <window-id>` — re-clamp an existing sidebar after the
/// window changed size, so a percentage width keeps meaning what it says.
/// Wired to tmux's `window-resized` hook.
pub fn cmd_resize(args: &[&str]) -> i32 {
    let Some(window_id) = args.first().copied() else {
        return 0;
    };
    let Some(sidebar) = find_sidebar_pane(window_id) else {
        return 0;
    };

    let target = configured_width(window_id);
    let current: u32 = tmux::display_message(&sidebar, "#{pane_width}")
        .parse()
        .unwrap_or(0);
    if current == target {
        return 0;
    }

    tmux::run_tmux_quiet(&["resize-pane", "-x", &target.to_string(), "-t", &sidebar]);
    0
}

/// `agent-mgr auto-close <window-id>` — close a window whose only remaining pane
/// is the sidebar. Wired to tmux's `pane-exited` hook so that exiting your shell
/// closes the window as it normally would, instead of leaving a lone sidebar.
pub fn cmd_auto_close(args: &[&str]) -> i32 {
    let Some(window_id) = args.first().copied() else {
        return 0;
    };

    let role_format = format!("#{{{}}}", tmux::PANE_ROLE);
    let panes = tmux::run_tmux(&["list-panes", "-t", window_id, "-F", &role_format]);
    let session_windows = numeric_format(window_id, "#{session_windows}");
    let session_attached = numeric_format(window_id, "#{session_attached}");

    if should_kill_window(panes.as_deref(), session_windows, session_attached) {
        tmux::run_tmux_quiet(&["kill-window", "-t", window_id]);
    }
    0
}

fn numeric_format(target: &str, format: &str) -> Option<u32> {
    tmux::run_tmux(&["display-message", "-t", target, "-p", format])
        .and_then(|value| value.trim().parse().ok())
}

/// Where focus should be once the sidebar exists.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Focus {
    /// Land in the new sidebar — you opened it to use it.
    Sidebar,
    /// Leave focus exactly where it was, for a window you did not ask to be moved in.
    Unchanged,
}

/// Split a full-height sidebar pane into `window_id`.
fn create_sidebar(window_id: &str, pane_path: &str, focus: Focus) {
    let width = configured_width(window_id).to_string();
    let position = Position::from_setting(&tmux::display_message(
        window_id,
        &format!("#{{{}}}", tmux::CFG_POSITION),
    ));

    let geometry = tmux::run_tmux(&[
        "list-panes",
        "-t",
        window_id,
        "-F",
        "#{pane_left} #{pane_width} #{pane_id}",
    ])
    .unwrap_or_default();
    let target = outermost_pane(&geometry, position).unwrap_or_else(|| window_id.to_owned());

    // `split-window` leaves focus in the new pane, which is what we want when you
    // opened the sidebar yourself. Only the windows you did *not* aim at need their
    // previous pane remembered — and asking tmux for it costs a subprocess, so only
    // those pay for it.
    let previously_active = match focus {
        Focus::Sidebar => String::new(),
        Focus::Unchanged => tmux::display_message(window_id, "#{pane_id}"),
    };

    let exe = std::env::current_exe()
        .ok()
        .and_then(|path| path.to_str().map(str::to_owned))
        .unwrap_or_else(|| "agent-mgr".to_owned());

    let sidebar = tmux::run_tmux(&[
        "split-window",
        position.split_flags(),
        "-l",
        &width,
        "-t",
        &target,
        "-c",
        pane_path,
        "-P",
        "-F",
        "#{pane_id}",
        &exe,
    ])
    .map(|id| id.trim().to_owned())
    .unwrap_or_default();

    if !sidebar.is_empty() {
        // Tag it immediately: this is how the TUI excludes itself from its own
        // list and how a later toggle finds the pane to kill.
        tmux::set_pane_option_raw(&sidebar, tmux::PANE_ROLE, tmux::PANE_ROLE_SIDEBAR);
        // With `automatic-rename on`, a focused sidebar would otherwise rename the
        // tab to our binary. Guard the window so the sidebar keeps the name.
        guard_window_rename(window_id);
    }

    if focus == Focus::Unchanged {
        if previously_active.is_empty() {
            // tmux could not tell us which pane it was; its own last-pane memory is
            // the next best answer.
            tmux::run_tmux_quiet(&["select-pane", "-t", window_id, "-l"]);
        } else {
            tmux::run_tmux_quiet(&["select-pane", "-t", &previously_active]);
        }
    }
}

/// Read the width options for `window_id` and resolve them to a column count.
fn configured_width(window_id: &str) -> u32 {
    let setting = tmux::display_message(window_id, &format!("#{{{}}}", tmux::CFG_WIDTH));
    let window_width = tmux::display_message(window_id, "#{window_width}")
        .parse()
        .unwrap_or(0);
    let min = tmux::display_message(window_id, &format!("#{{{}}}", tmux::CFG_MIN_WIDTH))
        .trim()
        .parse()
        .unwrap_or(DEFAULT_MIN_WIDTH);
    let max = tmux::display_message(window_id, &format!("#{{{}}}", tmux::CFG_MAX_WIDTH))
        .trim()
        .parse()
        .ok();

    resolve_width(&setting, window_width, min, max)
}

/// Resolve `@agent_mgr_width` (a column count or `N%`) into columns.
///
/// Clamped min → max → main-pane guard, in that order: an explicit `max` below
/// `min` still caps the result, and the guard wins on a genuinely tiny window
/// even against the configured minimum.
fn resolve_width(setting: &str, window_width: u32, min: u32, max: Option<u32>) -> u32 {
    let mut width = match setting.trim().strip_suffix('%') {
        Some(percent) => {
            let percent: u32 = percent.trim().parse().unwrap_or(DEFAULT_PERCENT);
            if window_width == 0 {
                // A percentage is meaningless without a window width; fall back
                // to the minimum rather than guessing.
                min
            } else {
                window_width * percent / 100
            }
        }
        None => setting.trim().parse().unwrap_or(DEFAULT_EXPLICIT_WIDTH),
    };

    width = width.max(min);
    if let Some(max) = max {
        width = width.min(max);
    }
    if window_width > 0 {
        width = width.min(window_width.saturating_sub(MAIN_PANE_MIN_WIDTH));
    }
    width.max(1)
}

/// Decide whether `auto-close` should kill the window, from the raw tmux answers.
///
/// Pure so the guard logic is testable without a live server — and the guards
/// matter, because getting this wrong drops a user's whole session.
///
/// - `panes`: stdout of `list-panes -F '#{@agent_mgr_pane_role}'`, or `None` if
///   the call failed.
/// - `session_windows` / `session_attached`: parsed formats, `None` on failure.
fn should_kill_window(
    panes: Option<&str>,
    session_windows: Option<u32>,
    session_attached: Option<u32>,
) -> bool {
    // No output is not the same as no panes. The window may already be gone, or
    // tmux may just be busy — treating it as "empty" would let a race kill a
    // live window.
    let Some(panes) = panes else {
        return false;
    };
    if panes.trim().is_empty() {
        return false;
    }

    // An unset role renders as an empty line, and that pane is an ordinary user
    // pane, so any line that isn't ours keeps the window alive.
    if panes.lines().any(|line| line != tmux::PANE_ROLE_SIDEBAR) {
        return false;
    }

    let Some(windows) = session_windows else {
        return false;
    };

    match windows {
        // Cannot prove there is anywhere to fall back to; preserve.
        0 => false,
        // Killing the last window destroys the session and drops every attached
        // client. One client is fine — that is what plain `exit` does on the last
        // pane. Two or more means a shared session (several terminal tabs on
        // `main`) where we cannot tell which clients are wanted, so leave the
        // sidebar stranded instead of mass-disconnecting. An unknown count errs
        // the same way.
        1 => matches!(session_attached, Some(count) if count <= 1),
        _ => true,
    }
}

fn pane_role_format() -> String {
    format!("#{{pane_id}}\t#{{{}}}", tmux::PANE_ROLE)
}

/// tmux's built-in window option naming a window after its active pane's command
/// when `automatic-rename` is on. While a sidebar is present we override it
/// per-window so focusing the sidebar does not rename the tab to our binary.
const AUTOMATIC_RENAME_FORMAT: &str = "automatic-rename-format";

/// Wrap `effective` so a focused sidebar pane keeps the window's current name,
/// while every other pane still auto-names by `effective`. The active pane is in
/// context when tmux expands this, so it can read the sidebar role tag.
fn rename_guard_format(effective: &str) -> String {
    format!(
        "#{{?#{{==:#{{{role}}},{sidebar}}},#{{window_name}},{effective}}}",
        role = tmux::PANE_ROLE,
        sidebar = tmux::PANE_ROLE_SIDEBAR,
    )
}

/// Read the effective global `automatic-rename-format` so a user's custom format
/// survives for ordinary panes. tmux reports its built-in default here when the
/// option is unset, so the fallback is only reached if the query itself fails.
fn effective_rename_format() -> String {
    let value = tmux::run_tmux(&["show-options", "-gwv", AUTOMATIC_RENAME_FORMAT])
        .map(|value| value.trim().to_owned())
        .unwrap_or_default();
    if value.is_empty() {
        "#{pane_current_command}".to_owned()
    } else {
        value
    }
}

/// Guard `window_id`'s tab against the sidebar renaming it. Composes the user's
/// effective format into the else-branch so their customization is preserved.
fn guard_window_rename(window_id: &str) {
    let format = rename_guard_format(&effective_rename_format());
    let _ = tmux::set_window_option(window_id, AUTOMATIC_RENAME_FORMAT, &format);
}

/// Drop the per-window guard so the window re-inherits the global format once its
/// sidebar is gone; otherwise a later change to the global would be shadowed by
/// the copy baked in at creation.
fn clear_window_rename(window_id: &str) {
    let _ = tmux::unset_window_option(window_id, AUTOMATIC_RENAME_FORMAT);
}

/// `#{pane_id}\t#{window_id}\t#{@agent_mgr_pane_role}` — like [`pane_role_format`]
/// but carrying the window id, so a server-wide teardown can clear each sidebar's
/// window guard as it kills the pane.
fn sidebar_kill_format() -> String {
    format!("#{{pane_id}}\t#{{window_id}}\t#{{{}}}", tmux::PANE_ROLE)
}

/// `(pane_id, window_id)` for every sidebar in [`sidebar_kill_format`] output, or
/// `None` when there are none — so callers distinguish "turn them off" from "on".
fn sidebar_panes_with_windows(listing: &str) -> Option<Vec<(String, String)>> {
    let panes: Vec<(String, String)> = listing
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let pane_id = fields.next()?;
            let window_id = fields.next()?;
            let role = fields.next()?;
            (role == tmux::PANE_ROLE_SIDEBAR).then(|| (pane_id.to_owned(), window_id.to_owned()))
        })
        .collect();
    (!panes.is_empty()).then_some(panes)
}

/// Extract sidebar pane ids from `pane_role_format` output. `None` when there
/// are none, so callers can distinguish "turn them off" from "turn them on".
fn sidebar_panes(listing: &str) -> Option<Vec<String>> {
    let panes: Vec<String> = listing
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .filter(|(_, role)| *role == tmux::PANE_ROLE_SIDEBAR)
        .map(|(pane_id, _)| pane_id.to_owned())
        .collect();
    (!panes.is_empty()).then_some(panes)
}

fn find_sidebar_pane(window_id: &str) -> Option<String> {
    let listing = tmux::run_tmux(&["list-panes", "-t", window_id, "-F", &pane_role_format()])?;
    sidebar_panes(&listing)?.into_iter().next()
}

/// One `(window_id, path)` per window, keeping the first pane's path.
fn unique_window_paths(listing: &str) -> Vec<(String, String)> {
    let mut seen = HashSet::new();
    let mut windows = Vec::new();
    for line in listing.lines() {
        let Some((window_id, path)) = line.split_once('\t') else {
            continue;
        };
        if seen.insert(window_id.to_owned()) {
            windows.push((window_id.to_owned(), path.to_owned()));
        }
    }
    windows
}

/// Which edge of the window the sidebar lives on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Position {
    Left,
    Right,
}

impl Position {
    /// Only an explicit `right` moves the sidebar; anything else — unset, empty,
    /// or a typo — stays left, so a mistake never relocates it unexpectedly.
    fn from_setting(setting: &str) -> Self {
        if setting.trim().eq_ignore_ascii_case("right") {
            Self::Right
        } else {
            Self::Left
        }
    }

    /// `-hfb` inserts the new pane before the target (to its left), `-hf` after
    /// it. Both `f` variants span the full window height.
    fn split_flags(self) -> &'static str {
        match self {
            Self::Left => "-hfb",
            Self::Right => "-hf",
        }
    }
}

/// Pick the pane to split from so the sidebar lands on the window's outer edge:
/// the leftmost pane for a left sidebar, the one with the largest right edge for
/// a right sidebar.
fn outermost_pane(geometry: &str, position: Position) -> Option<String> {
    let panes = geometry.lines().filter_map(|line| {
        let mut parts = line.split_whitespace();
        let left: u32 = parts.next()?.parse().ok()?;
        let width: u32 = parts.next()?.parse().ok()?;
        Some((left, width, parts.next()?.to_owned()))
    });

    match position {
        Position::Left => panes.min_by_key(|(left, _, _)| *left),
        Position::Right => panes.max_by_key(|(left, width, _)| left.saturating_add(*width)),
    }
    .map(|(_, _, pane_id)| pane_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── width resolution ─────────────────────────────────────────────

    #[test]
    fn percentage_width_on_a_normal_window() {
        assert_eq!(resolve_width("20%", 200, 24, None), 40);
    }

    #[test]
    fn percentage_below_the_minimum_is_clamped_up() {
        // 20% of 80 = 16, below the min of 24, and 80 is wide enough to allow it.
        assert_eq!(resolve_width("20%", 80, 24, None), 24);
    }

    #[test]
    fn explicit_columns_are_honoured_within_the_clamps() {
        assert_eq!(resolve_width("32", 200, 24, None), 32);
        assert_eq!(resolve_width("5", 200, 24, None), 24);
        assert_eq!(resolve_width("99", 200, 24, Some(40)), 40);
    }

    #[test]
    fn the_main_pane_guard_wins_on_a_tiny_window() {
        // A 30-column window may give the sidebar at most 10, even though the
        // configured minimum is 24: your work pane keeps its reserve.
        assert_eq!(resolve_width("50%", 30, 24, None), 10);
        assert_eq!(resolve_width("25", 30, 24, None), 10);
    }

    #[test]
    fn width_never_collapses_below_one_column() {
        assert_eq!(resolve_width("50%", 20, 24, None), 1);
        assert_eq!(resolve_width("50%", 10, 24, None), 1);
    }

    #[test]
    fn junk_settings_fall_back_to_defaults() {
        assert_eq!(resolve_width("wat", 200, 24, None), DEFAULT_EXPLICIT_WIDTH);
        // "%"-suffixed but unparseable: use the default percentage, not the min.
        assert_eq!(resolve_width("wat%", 200, 24, None), 200 * DEFAULT_PERCENT / 100);
    }

    #[test]
    fn unknown_window_width_skips_the_percentage_and_the_guard() {
        assert_eq!(resolve_width("20%", 0, 24, None), 24);
        assert_eq!(resolve_width("32", 0, 24, None), 32);
    }

    #[test]
    fn a_max_below_the_min_still_caps() {
        assert_eq!(resolve_width("20%", 200, 30, Some(10)), 10);
    }

    // ─── auto-close guards ────────────────────────────────────────────

    #[test]
    fn kills_a_window_holding_only_the_sidebar() {
        // The intended path. The attached count is irrelevant because other
        // windows exist, so killing this one cannot end the session.
        assert!(should_kill_window(Some("sidebar"), Some(2), None));
        assert!(should_kill_window(Some("sidebar"), Some(2), Some(5)));
    }

    #[test]
    fn keeps_a_window_that_still_has_a_real_pane() {
        assert!(!should_kill_window(Some("sidebar\npane"), Some(5), Some(1)));
        // An unset role renders as an empty line — that is a user's pane.
        assert!(!should_kill_window(Some("sidebar\n\n"), Some(5), Some(1)));
        assert!(!should_kill_window(Some("\nsidebar\n"), Some(5), Some(1)));
    }

    #[test]
    fn a_failed_or_empty_query_never_kills() {
        // Treating "no answer" as "no panes" would let a busy-tmux race destroy
        // a live window.
        assert!(!should_kill_window(None, Some(5), Some(1)));
        assert!(!should_kill_window(Some(""), Some(5), Some(1)));
        assert!(!should_kill_window(Some("   \n"), Some(5), Some(1)));
    }

    #[test]
    fn last_window_dies_only_when_at_most_one_client_is_attached() {
        // One client, or none: matches what plain `exit` would do.
        assert!(should_kill_window(Some("sidebar"), Some(1), Some(1)));
        assert!(should_kill_window(Some("sidebar"), Some(1), Some(0)));
        // Several terminal tabs share this session — killing it would drop them
        // all at once. Strand the sidebar instead.
        assert!(!should_kill_window(Some("sidebar"), Some(1), Some(2)));
        assert!(!should_kill_window(Some("sidebar"), Some(1), Some(7)));
        // Unknown client count: err toward preservation.
        assert!(!should_kill_window(Some("sidebar"), Some(1), None));
    }

    #[test]
    fn an_unprovable_session_shape_never_kills() {
        assert!(!should_kill_window(Some("sidebar"), None, Some(1)));
        assert!(!should_kill_window(Some("sidebar"), Some(0), Some(1)));
    }

    // ─── the focus key ────────────────────────────────────────────────

    #[test]
    fn the_focus_key_goes_in_from_a_work_pane_and_back_out_from_the_sidebar() {
        // One key for the round trip: an effect that depended on state you cannot see
        // from the keyboard would be worse than two keys.
        assert_eq!(focus_action(Some("%2"), "%1"), FocusAction::Select("%2".to_owned()));
        assert_eq!(focus_action(Some("%2"), "%2"), FocusAction::HopBack);
    }

    #[test]
    fn the_focus_key_opens_a_sidebar_in_a_window_that_has_none() {
        assert_eq!(focus_action(None, "%1"), FocusAction::Create);
        assert_eq!(focus_action(None, ""), FocusAction::Create);
    }

    #[test]
    fn a_binding_that_passed_no_current_pane_still_focuses_rather_than_hopping() {
        // An empty id must not match the sidebar's, or the key would send you
        // somewhere you did not ask for.
        assert_eq!(focus_action(Some("%2"), ""), FocusAction::Select("%2".to_owned()));
    }

    // ─── placement ────────────────────────────────────────────────────

    #[test]
    fn only_an_explicit_right_moves_the_sidebar() {
        assert_eq!(Position::from_setting("right"), Position::Right);
        assert_eq!(Position::from_setting(" RIGHT "), Position::Right);
        assert_eq!(Position::from_setting("left"), Position::Left);
        assert_eq!(Position::from_setting(""), Position::Left);
        assert_eq!(Position::from_setting("rihgt"), Position::Left);
    }

    #[test]
    fn split_flags_match_tmux_side_semantics() {
        assert_eq!(Position::Left.split_flags(), "-hfb");
        assert_eq!(Position::Right.split_flags(), "-hf");
    }

    #[test]
    fn outermost_pane_finds_the_window_edge() {
        let geometry = "40 80 %3\n0 20 %1\n20 20 %2";
        assert_eq!(
            outermost_pane(geometry, Position::Left),
            Some("%1".to_owned())
        );
        assert_eq!(
            outermost_pane(geometry, Position::Right),
            Some("%3".to_owned())
        );
    }

    #[test]
    fn outermost_pane_skips_malformed_lines() {
        assert_eq!(
            outermost_pane("bad\n0 nope %1\n12 30 %2", Position::Left),
            Some("%2".to_owned())
        );
        assert_eq!(outermost_pane("", Position::Right), None);
    }

    // ─── listing helpers ──────────────────────────────────────────────

    #[test]
    fn sidebar_panes_finds_every_sidebar_or_none() {
        assert_eq!(
            sidebar_panes("%1\t\n%2\tsidebar\n%3\t\n%4\tsidebar"),
            Some(vec!["%2".to_owned(), "%4".to_owned()])
        );
        assert_eq!(sidebar_panes("%1\t\n%2\t"), None);
        assert_eq!(sidebar_panes(""), None);
    }

    #[test]
    fn sidebar_panes_with_windows_pairs_each_sidebar_with_its_window() {
        // The window id is what lets a server-wide toggle-off clear each
        // sidebar's per-window rename guard as it kills the pane.
        assert_eq!(
            sidebar_panes_with_windows("%1\t@1\t\n%2\t@1\tsidebar\n%3\t@2\tsidebar"),
            Some(vec![
                ("%2".to_owned(), "@1".to_owned()),
                ("%3".to_owned(), "@2".to_owned()),
            ])
        );
        assert_eq!(sidebar_panes_with_windows("%1\t@1\t\n%2\t@1\t"), None);
        assert_eq!(sidebar_panes_with_windows(""), None);
    }

    // ─── the automatic-rename guard ───────────────────────────────────

    #[test]
    fn the_rename_guard_keeps_the_window_name_only_for_the_sidebar_pane() {
        // Focused sidebar → #{window_name} (tab unchanged); any other pane → the
        // effective format, so ordinary panes auto-name exactly as before. This
        // is what stops `automatic-rename` retitling the tab to our binary.
        assert_eq!(
            rename_guard_format("#{pane_current_command}"),
            "#{?#{==:#{@agent_mgr_pane_role},sidebar},#{window_name},#{pane_current_command}}"
        );
    }

    #[test]
    fn the_rename_guard_preserves_a_user_custom_format_for_normal_panes() {
        // A user's custom global format is composed verbatim into the else-branch
        // rather than overwritten, so their naming survives outside the sidebar.
        assert_eq!(
            rename_guard_format("#{b:pane_current_path}"),
            "#{?#{==:#{@agent_mgr_pane_role},sidebar},#{window_name},#{b:pane_current_path}}"
        );
    }

    #[test]
    fn unique_window_paths_dedupes_and_keeps_spaces_in_paths() {
        assert_eq!(
            unique_window_paths("@1\t/home/me/My Project\n@1\t/other\n@2\t/tmp/x"),
            vec![
                ("@1".to_owned(), "/home/me/My Project".to_owned()),
                ("@2".to_owned(), "/tmp/x".to_owned()),
            ]
        );
    }

    #[test]
    fn unique_window_paths_skips_malformed_lines() {
        assert_eq!(
            unique_window_paths("garbage\n@1\t/tmp"),
            vec![("@1".to_owned(), "/tmp".to_owned())]
        );
    }
}
