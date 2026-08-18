//! Passive agent detection: recognizing an agent by its process name, and
//! inferring what it is doing from the pane title and the visible screen.
//!
//! This is the zero-setup path — nothing is wrapped, shimmed, or launched by us,
//! and no hook needs to be installed. The cost is that it reads the agents' UI,
//! so it is **heuristic and version-sensitive**: a Claude or Codex redesign, an
//! unusual theme, or a non-English locale can throw off a reading. When the
//! agent's own hooks are wired up, [`crate::daemon`] prefers those instead.
//!
//! Ported from `tmux-agent-switcher`'s `src/detect.rs`, which is where these
//! heuristics (and their edge cases) were worked out.

use crate::model::{AgentEvidence, AgentKind, AgentState};

/// How many lines of the screen tail Claude's state detection looks at. Claude
/// draws its input box at the bottom, and a longer window lets scrollback pin a
/// stale state.
///
/// Codex is deliberately not bounded this way — see [`codex_state`] and
/// [`crate::daemon::capture_screen`].
pub const SCREEN_TAIL_LINES: usize = 25;

/// Identify an agent from a pane's foreground command.
pub fn agent_from_process_name(name: &str) -> Option<AgentKind> {
    let basename = basename(name);
    if basename == "codex" || basename.starts_with("codex-") {
        Some(AgentKind::Codex)
    } else if basename == "claude"
        || basename == "claude-code"
        || basename.starts_with("claude-")
        || is_claude_version_name(basename)
    {
        Some(AgentKind::Claude)
    } else {
        None
    }
}

/// Identify an agent from a full command line, as `ps args=` reports it.
///
/// Beyond `argv[0]`, this reads `argv[1]` when it looks like a path — which is
/// where an interpreter's script lands. The npm install of Codex is
/// `node /usr/local/bin/codex`, so `argv[0]` alone says only `node`.
///
/// Requiring a `/` is what keeps it from firing on an *argument* that happens to
/// name an agent: `rg codex` must not read as a Codex process.
pub fn agent_from_command_line(args: &str) -> Option<AgentKind> {
    let mut argv = args.split_whitespace();
    let argv0 = argv.next().unwrap_or_default().trim_matches('"');
    if let Some(agent) = agent_from_process_name(argv0) {
        return Some(agent);
    }
    let argv1 = argv.next().unwrap_or_default().trim_matches('"');
    if argv1.contains('/') {
        return agent_from_process_name(argv1);
    }
    None
}

/// `true` for foreground commands that are worth a process-tree walk even though
/// they are not themselves an agent: language runtimes and package runners that
/// an agent's launcher script can sit behind.
///
/// This gate is what keeps a workspace of plain shells from paying for a `ps`
/// every poll. Codex installed from npm is the case it exists for: the pane runs
/// `node /usr/local/bin/codex`, which spawns the real binary as a child, so
/// tmux reports the pane's current command as a bare `node`.
pub fn may_host_agent(command: &str) -> bool {
    matches!(
        basename(command),
        "node" | "bun" | "deno" | "npm" | "npx" | "pnpm" | "yarn"
    )
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Claude Code's native installer runs a versioned binary at
/// `~/.local/share/claude/versions/<version>` and sets its `process.title` to
/// that same version string, so tmux reports the pane's current command as a
/// bare `MAJOR.MINOR.PATCH` (e.g. `2.1.197`) rather than `claude`. Treat that
/// shape as Claude so detection still fires for native installs.
fn is_claude_version_name(name: &str) -> bool {
    let mut parts = 0;
    for part in name.split('.') {
        if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return false;
        }
        parts += 1;
    }
    parts == 3
}

/// The cheap path: some titles are unambiguous on their own, which saves a
/// `capture-pane` subprocess per pane per poll.
///
/// Returns `None` when the title is inconclusive and the screen must be read.
pub fn state_from_title(agent: AgentKind, title: &str) -> Option<AgentState> {
    let title = title.trim();
    match agent {
        AgentKind::Claude if starts_with_spinner(title) => Some(AgentState::Working),
        // Claude's idle title (`✳ …`) looks the same whether or not a modal is
        // on screen, so it is never conclusive by itself.
        //
        // Codex is never conclusive either, because it does not set the title at
        // *all* — see [`codex_state`]. Whatever tmux reports came from the shell
        // that launched it, so reading it can only mislead.
        _ => None,
    }
}

/// The full path: decide state from the title plus the tail of the screen.
pub fn state_from_evidence(agent: AgentKind, evidence: &AgentEvidence) -> AgentState {
    match agent {
        AgentKind::Codex => codex_state(evidence),
        AgentKind::Claude => claude_state(evidence),
    }
}

/// Codex is read entirely from the screen, because it never sets the terminal
/// title. Verified against Codex 0.144.3: the vendored binary emits no OSC 0/2
/// sequence at all, so `pane_title` still holds whatever the launching shell put
/// there. Treating a non-empty title as "idle" — which is what this used to do —
/// pinned every npm-installed Codex pane to idle forever, mid-turn included.
///
/// The signals, captured from a live session rather than guessed:
///
/// - **Working**: the status line above the composer always carries the
///   interrupt hint — `• Working (7s • esc to interrupt)`. The verb varies
///   freely (`Starting MCP servers (9/10): …`, `Planning tool execution`) and the
///   bullet alternates `•`/`◦` as it animates, so `esc to interrupt` is the only
///   stable part.
/// - **Blocked**: a `›`-cursor selection list, which is the same widget behind
///   the directory-trust prompt, `/model`, and command approval.
///
/// The two are mutually exclusive on screen — Codex's bottom pane shows a modal
/// or a status line, never both.
///
/// Unlike Claude this reads the *whole* screen rather than a window of trailing
/// lines, because Codex is top-anchored until its transcript grows tall enough to
/// fill the pane: a fresh session in a 92-row pane puts the composer around row
/// 12, far above any bottom-anchored window. That needs no staleness bound
/// because the status line is transient — it is erased the frame a turn ends,
/// never left behind in the transcript. Caller must therefore pass the visible
/// screen and not scrollback; [`crate::daemon::capture_screen`] does.
fn codex_state(evidence: &AgentEvidence) -> AgentState {
    let recent = evidence.screen_tail.as_str();
    let lower = recent.to_lowercase();

    if has_selection_menu(recent)
        || contains_any(
            &lower,
            &[
                "press enter to confirm",
                "press enter to continue",
                "enter to submit answer",
                "allow command?",
                "[y/n]",
            ],
        )
    {
        return AgentState::Blocked;
    }

    if lower.contains("esc to interrupt") {
        return AgentState::Working;
    }

    AgentState::Idle
}

fn claude_state(evidence: &AgentEvidence) -> AgentState {
    let title = evidence.osc_title.trim();
    // Claude's prompts always sit at the bottom of the screen. Only matching the
    // last handful of lines stops old scrollback from pinning a state — e.g. a
    // long-scrolled-past "do you want to proceed?" holding a busy pane Blocked.
    let recent = recent_lines(&evidence.screen_tail, SCREEN_TAIL_LINES);

    // Blocked: a modal selection menu is on screen with the cursor resting on one
    // of several numbered options. The match is structural rather than
    // wording-based so it survives a copy edit, with the selection-list footer as
    // a fallback.
    if has_selection_menu(&recent)
        || contains_all(
            &recent.to_lowercase(),
            &["enter to select", "esc to cancel"],
        )
    {
        return AgentState::Blocked;
    }

    // Working: Claude prefixes its OSC title with a spinner frame while active.
    if starts_with_spinner(title) {
        return AgentState::Working;
    }

    // The same question asked of the screen, for when the title says nothing
    // useful: a spinner glyph we do not know yet, or a terminal that never
    // delivered the OSC at all. Claude only offers the interrupt hint mid-turn
    // — its idle footer carries the permission-mode hint alone — and the hint
    // is transient rather than left behind in the transcript, so the same
    // string that reads Codex reads Claude.
    if recent.to_lowercase().contains("esc to interrupt") {
        return AgentState::Working;
    }

    // Otherwise it is sitting at its input prompt. Note the `❯` input box is
    // present while working too, so its presence is not an idle signal.
    AgentState::Idle
}

/// The cursor glyph an agent rests on the selected row of a menu. Claude uses
/// `❯` (U+276F) and Codex `›` (U+203A); both also use it to mark their composer,
/// which is why a bare cursor is not enough to call something a menu.
const MENU_CURSORS: [char; 2] = ['❯', '›'];

/// `true` when the screen shows a selection menu: the cursor rests on a numbered
/// option *and* at least two numbered options are present.
///
/// Requiring a second option is what distinguishes a real menu from the bare
/// input box with something like `1. fix the parser` typed into it. Stripping the
/// box border first is what makes it work against Claude's real bordered
/// rendering (`│ ❯ 1. Yes │`), where the option no longer starts the line; Codex
/// draws the same list unbordered (`› 1. Yes, continue`).
///
/// Known ambiguity: a user composing a *multi-line* numbered list in the input
/// box is structurally identical to a menu and can read as Blocked. Anchoring on
/// a menu footer would remove it, but Claude's permission and plan modals don't
/// render one, so that would miss the very prompts this exists to catch. The
/// idle→busy debounce in [`crate::daemon`] absorbs the common fast-typed case.
fn has_selection_menu(text: &str) -> bool {
    let mut cursor_on_option = false;
    let mut option_lines = 0;

    for line in text.lines() {
        let line = strip_border(line);
        let (has_cursor, rest) = match line.strip_prefix(MENU_CURSORS) {
            Some(rest) => (true, rest.trim_start()),
            None => (false, line),
        };
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        if digits == 0 {
            continue;
        }
        let after = &rest[digits..];
        if after.starts_with('.') || after.starts_with(')') {
            option_lines += 1;
            cursor_on_option |= has_cursor;
        }
    }

    cursor_on_option && option_lines >= 2
}

/// Strip leading whitespace and box-drawing verticals, so matching works whether
/// or not the content is wrapped in a border.
fn strip_border(line: &str) -> &str {
    line.trim_start_matches(|ch: char| {
        ch.is_whitespace() || matches!(ch, '│' | '┃' | '║' | '╎' | '┆' | '┊' | '|')
    })
}

fn recent_lines(text: &str, count: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(count);
    all[start..].join("\n")
}

/// Claude advertises activity by prefixing its OSC title with a spinner frame
/// followed by a space. *Which* frames it cycles through is not stable across
/// releases: braille (`⠋`) held for a long time, and 2.1.235 sets `◐ `
/// instead. Accepting every family it has used, rather than only the one that
/// happened to ship when this was written, is what keeps an upgrade from
/// silently pinning every Claude pane to idle — which is exactly what 2.1.235
/// did.
///
/// Deliberately *not* a catch-all "any non-ASCII leader": Claude's idle title
/// starts with `✳ `, so a blanket rule would read idle as working.
fn starts_with_spinner(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some(ch) if is_spinner_frame(ch)) && matches!(chars.next(), Some(' '))
}

/// A single spinner frame: braille, or one of the geometric-shape animations
/// (part-filled circles and squares) that terminal spinners commonly cycle.
fn is_spinner_frame(ch: char) -> bool {
    matches!(ch,
        '\u{2800}'..='\u{28ff}'   // braille patterns
        | '\u{25cb}'..='\u{25d7}' // ○ ◌ ◍ ◎ ● ◐ ◑ ◒ ◓ ◔ ◕ ◖ ◗
        | '\u{25e0}'..='\u{25e5}' // ◠ ◡ ◢ ◣ ◤ ◥
        | '\u{25f0}'..='\u{25ff}' // ◰ ◱ ◲ ◳ ◴ ◵ ◶ ◷
    )
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

fn contains_all(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().all(|needle| haystack.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence(title: &str, screen: &[&str]) -> AgentEvidence {
        AgentEvidence {
            screen_tail: screen.join("\n"),
            osc_title: title.to_owned(),
        }
    }

    #[test]
    fn recognizes_both_agents_from_a_process_name() {
        assert_eq!(
            agent_from_process_name("/opt/bin/codex"),
            Some(AgentKind::Codex)
        );
        assert_eq!(
            agent_from_process_name("codex-aarch64-a"),
            Some(AgentKind::Codex)
        );
        assert_eq!(agent_from_process_name("claude"), Some(AgentKind::Claude));
        assert_eq!(
            agent_from_process_name("claude-code"),
            Some(AgentKind::Claude)
        );
    }

    #[test]
    fn recognizes_the_native_installer_bare_semver_as_claude() {
        // anthropics/claude-code#49852: tmux reports the version, not "claude".
        assert_eq!(agent_from_process_name("2.1.197"), Some(AgentKind::Claude));
        assert_eq!(
            agent_from_process_name("/home/me/.local/share/claude/versions/2.1.197"),
            Some(AgentKind::Claude)
        );
        // ...without swallowing ordinary commands.
        assert_eq!(agent_from_process_name("zsh"), None);
        assert_eq!(agent_from_process_name("node"), None);
        assert_eq!(agent_from_process_name("2.1"), None);
        assert_eq!(agent_from_process_name("1.2.3.4"), None);
        assert_eq!(agent_from_process_name("v1.2.3"), None);
    }

    #[test]
    fn title_fast_path_short_circuits_the_unambiguous_cases() {
        assert_eq!(
            state_from_title(AgentKind::Claude, "⠋ thinking"),
            Some(AgentState::Working)
        );
        // Claude's idle-looking title is never conclusive: a modal may be up.
        assert_eq!(state_from_title(AgentKind::Claude, "✳ review this"), None);
        assert_eq!(state_from_title(AgentKind::Claude, ""), None);
    }

    #[test]
    fn a_codex_title_is_never_conclusive_because_codex_never_sets_one() {
        // Codex 0.144.3 emits no OSC 0/2 sequence, so `pane_title` still holds
        // whatever the launching shell put there — a hostname, or the cwd's
        // basename. Reading it as "idle" (which this used to do for any
        // non-empty title) pinned every Codex pane to idle forever, because the
        // screen was then never captured at all.
        for title in ["", "coder", "tmp", "aciarlillo-engine", "⠋ working"] {
            assert_eq!(
                state_from_title(AgentKind::Codex, title),
                None,
                "{title:?} must fall through to the screen"
            );
        }
    }

    #[test]
    fn claude_selection_menu_is_blocked_even_with_an_idle_title() {
        let state = state_from_evidence(
            AgentKind::Claude,
            &evidence(
                "✳ design the thing",
                &[
                    "│ Would you like to proceed?              │",
                    "│ ❯ 1. Yes, and auto-accept edits         │",
                    "│   2. Yes, and manually approve edits    │",
                    "│   3. No, keep planning                  │",
                ],
            ),
        );
        assert_eq!(state, AgentState::Blocked);
    }

    #[test]
    fn claude_bordered_menu_without_a_known_phrase_is_blocked() {
        // A custom AskUserQuestion menu: no recognizable wording, non-1
        // numbering, ')' delimiter, drawn inside a border. Structural matching
        // is what catches this.
        let state = state_from_evidence(
            AgentKind::Claude,
            &evidence(
                "✳ pick a database",
                &[
                    "│ Which database should we use?           │",
                    "│ ❯ 2) Postgres                           │",
                    "│   3) SQLite                             │",
                ],
            ),
        );
        assert_eq!(state, AgentState::Blocked);
    }

    #[test]
    fn claude_idle_input_box_is_not_blocked() {
        let state = state_from_evidence(
            AgentKind::Claude,
            &evidence(
                "✳ clarify the logic",
                &[
                    "※ recap: did the thing. next: your review.",
                    "──────────────── ultracode ─",
                    "❯ ",
                    "────────────────",
                    "  ⏵⏵ bypass permissions on (shift+tab to cycle)",
                ],
            ),
        );
        assert_eq!(state, AgentState::Idle);
    }

    #[test]
    fn claude_prompt_scrolled_out_of_the_tail_no_longer_blocks() {
        let mut screen = vec!["Do you want to proceed?".to_owned()];
        for index in 0..30 {
            screen.push(format!("build output line {index}"));
        }
        let state = state_from_evidence(
            AgentKind::Claude,
            &AgentEvidence {
                screen_tail: screen.join("\n"),
                osc_title: "⠙ working".to_owned(),
            },
        );
        assert_eq!(state, AgentState::Working);
    }

    #[test]
    fn selection_menu_needs_both_a_cursor_and_a_second_option() {
        assert!(has_selection_menu("│ ❯ 1. Yes │\n│   2. No │"));
        assert!(has_selection_menu("❯ 10) ten\n  11) eleven"));
        // Codex's cursor is a different glyph drawing the same widget.
        assert!(has_selection_menu("› 1. Yes, continue\n  2. No, quit"));
        // The bare input box, or a single "1." line typed into it, is not a menu.
        assert!(!has_selection_menu("❯ "));
        assert!(!has_selection_menu("❯ 1. fix the parser and then rebase"));
        assert!(!has_selection_menu("› Find and fix a bug in @filename"));
        // A numbered list in ordinary output has no cursor on it.
        assert!(!has_selection_menu("1. first\n2. second"));
    }

    #[test]
    fn codex_permission_phrases_read_as_blocked() {
        for phrase in [
            "Press enter to confirm or esc to go back",
            "Press enter to continue",
            "Allow command?",
            "run it? [y/n]",
        ] {
            assert_eq!(
                state_from_evidence(AgentKind::Codex, &evidence("tmp", &[phrase])),
                AgentState::Blocked,
                "{phrase:?} should read as blocked"
            );
        }
    }

    #[test]
    fn codex_working_is_read_from_the_interrupt_hint() {
        // Captured from Codex 0.144.3. The verb and the animating bullet both
        // vary, so `esc to interrupt` is the only part worth matching. A stale
        // shell title sits on every one of these.
        for status in [
            "• Working (7s • esc to interrupt)",
            "◦ Working (11s • esc to interrupt)",
            "• Planning tool execution (2s • esc to interrupt)",
            "◦ Starting MCP servers (9/10): mcp-gateway-sourcegraph (4s • esc to interrupt)",
        ] {
            assert_eq!(
                state_from_evidence(
                    AgentKind::Codex,
                    &evidence("tmp", &[status, "", "› Find and fix a bug in @filename"]),
                ),
                AgentState::Working,
                "{status:?} should read as working"
            );
        }
    }

    #[test]
    fn codex_selection_list_is_blocked_with_its_own_cursor_glyph() {
        // The directory-trust prompt, verbatim. Codex marks the selected row with
        // `›` (U+203A) where Claude uses `❯`, and draws the list unbordered.
        let state = state_from_evidence(
            AgentKind::Codex,
            &evidence(
                "tmp",
                &[
                    "  Do you trust the contents of this directory?",
                    "",
                    "› 1. Yes, continue",
                    "  2. No, quit",
                ],
            ),
        );
        assert_eq!(state, AgentState::Blocked);
    }

    #[test]
    fn codex_idle_composer_is_neither_working_nor_blocked() {
        // The real idle screen: the composer's own `›` must not read as a menu,
        // and with no interrupt hint there is no work in flight.
        let state = state_from_evidence(
            AgentKind::Codex,
            &evidence(
                "tmp",
                &[
                    "• Command failed: sandbox initialization was denied.",
                    "",
                    "› Find and fix a bug in @filename",
                    "",
                    "  gpt-5.6-sol high · /tmp",
                ],
            ),
        );
        assert_eq!(state, AgentState::Idle);
    }

    #[test]
    fn codex_is_read_from_the_whole_screen_not_a_trailing_window() {
        // Codex is top-anchored until its transcript fills the pane, so in a tall
        // pane the status line sits far above the bottom. Bounding this the way
        // Claude's is bounded reported a working Codex as idle — the bug that
        // `capture_screen` and this test exist for.
        let mut screen = vec!["• Working (3s • esc to interrupt)".to_owned()];
        for _ in 0..70 {
            screen.push(String::new());
        }
        let state = state_from_evidence(
            AgentKind::Codex,
            &AgentEvidence {
                screen_tail: screen.join("\n"),
                osc_title: "tmp".to_owned(),
            },
        );
        assert_eq!(state, AgentState::Working);
    }

    #[test]
    fn the_npm_shim_command_line_names_codex() {
        // `ps args=` for an npm-installed Codex, which is what makes tmux report
        // the pane's command as a bare `node`.
        assert_eq!(
            agent_from_command_line("node /usr/local/bin/codex"),
            Some(AgentKind::Codex)
        );
        assert_eq!(
            agent_from_command_line("/usr/local/bin/codex app-server"),
            Some(AgentKind::Codex)
        );
        // An argument that merely *names* an agent is not one. Requiring a path
        // separator in argv[1] is what draws the line.
        assert_eq!(agent_from_command_line("rg codex"), None);
        assert_eq!(agent_from_command_line("git commit -m claude"), None);
        assert_eq!(agent_from_command_line("nvim codex.rs"), None);
        // A runtime hosting something else stays unrecognized.
        assert_eq!(
            agent_from_command_line("node --experimental-sqlite /opt/language-server.js"),
            None
        );
        assert_eq!(agent_from_command_line(""), None);
    }

    #[test]
    fn only_runtimes_are_worth_a_process_walk() {
        // The gate that keeps a workspace of plain shells from paying for a `ps`
        // on every poll, while still catching the npm-shim Codex install.
        assert!(may_host_agent("node"));
        assert!(may_host_agent("/usr/bin/node"));
        assert!(may_host_agent("bun"));
        assert!(may_host_agent("npx"));
        assert!(!may_host_agent("zsh"));
        assert!(!may_host_agent("bash"));
        assert!(!may_host_agent("nvim"));
        assert!(!may_host_agent(""));
    }

    #[test]
    fn a_spinner_glyph_without_a_trailing_space_is_not_a_spinner() {
        assert!(starts_with_spinner("⠋ thinking"));
        assert!(!starts_with_spinner("⠋thinking"));
        assert!(!starts_with_spinner("thinking ⠋"));
        assert!(!starts_with_spinner(""));
    }

    /// Claude 2.1.235 swapped its title spinner from braille to `◐`. Both
    /// families have to read as working, and the idle `✳` must not.
    #[test]
    fn reads_every_claude_spinner_family_as_working() {
        for frame in ['◐', '◑', '◒', '◓', '◴', '⠋'] {
            let title = format!("{frame} Spinner not appearing in tmux");
            assert!(starts_with_spinner(&title), "{title} should read as working");
            assert_eq!(
                state_from_title(AgentKind::Claude, &title),
                Some(AgentState::Working),
            );
        }
        assert!(!starts_with_spinner("✳ Spinner not appearing in tmux"));
        assert_eq!(
            state_from_title(AgentKind::Claude, "✳ Spinner not appearing in tmux"),
            None,
        );
    }

    /// The title is not the only witness: a glyph family we have not seen yet
    /// still leaves the interrupt hint on screen.
    #[test]
    fn reads_claudes_interrupt_hint_as_working_when_the_title_is_unhelpful() {
        let working = evidence(
            "✳ some task",
            &[
                "❯ ",
                "  ⏵⏵ bypass permissions on (shift+tab to cycle) · esc to interrupt · ← for agents",
            ],
        );
        assert_eq!(
            state_from_evidence(AgentKind::Claude, &working),
            AgentState::Working
        );

        let idle = evidence(
            "✳ some task",
            &[
                "❯ ",
                "  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents",
            ],
        );
        assert_eq!(
            state_from_evidence(AgentKind::Claude, &idle),
            AgentState::Idle
        );
    }
}
