//! Key and mouse handling.
//!
//! Vim movement is the default rather than an option, and every binding that
//! could mean two things resolves the same way it would in a pager: `j`/`k` move,
//! `g`/`G` jump to the ends, `Enter` acts. Counts work as they do in vim — `10j`
//! moves ten panes, `12G` goes to the twelfth — which is only aimable because the
//! relative-number gutter shows the distances (see [`crate::nav`]).

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};

use super::{App, worker::Worker};
use crate::nav::{self, Direction};
use crate::tmux;
use crate::ui;

pub fn handle(event: Event, app: &mut App, worker: &Worker) {
    match event {
        Event::Key(key) => handle_key(key, app, worker),
        Event::Mouse(mouse) => match mouse.kind {
            MouseEventKind::Down(button) => {
                if click(app, mouse.row, button) {
                    app.activate_selection();
                }
            }
            MouseEventKind::ScrollDown => app.move_selection(1),
            MouseEventKind::ScrollUp => app.move_selection(-1),
            _ => {}
        },
        _ => {}
    }
}

/// Feed one key into the app.
///
/// Split out so the whole keymap is testable without a terminal.
pub fn handle_key(key: KeyEvent, app: &mut App, worker: &Worker) {
    // Windows and some terminals report press *and* release; acting on both would
    // double every motion.
    if key.kind == KeyEventKind::Release {
        return;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    // While a prompt has the keyboard, letters are text rather than commands — `j`
    // has to type a `j`, not move the selection.
    if app.rename.is_some() {
        rename_key(key, app, worker, ctrl);
        return;
    }
    if app.search.as_ref().is_some_and(|search| search.editing) {
        search_key(key, app, ctrl);
        return;
    }
    if app.note_entry.is_some() {
        note_entry_key(key, app, ctrl);
        return;
    }
    // A confirmation swallows the keyboard like any other prompt, so the `j` you
    // press next cannot both answer it and move.
    if app.note_delete.is_some() {
        match key.code {
            KeyCode::Char('c') if ctrl => app.quit = true,
            // Only `y` deletes. Everything else — Esc, n, a mistyped letter, a
            // stray Enter — cancels, because the safe answer should be the one
            // you give by accident.
            code => app.resolve_note_delete(code == KeyCode::Char('y')),
        }
        return;
    }
    // Help is a page, not a prompt, but it still swallows keys: any key closing it
    // means you cannot accidentally act on a list you cannot currently see.
    if app.help {
        match key.code {
            KeyCode::Char('c') if ctrl => app.quit = true,
            _ => app.help = false,
        }
        return;
    }

    // The panel is a mode, not a second keymap: it wants `j`/`k` and `Space` and
    // so does the list, and the alternative is a modified-key vocabulary for the
    // same four motions.
    if app.notes_focus.is_some() {
        notes_key(key, app, ctrl);
        return;
    }

    match key.code {
        // Ctrl-C is the one binding that must work regardless of mode.
        KeyCode::Char('c') if ctrl => app.quit = true,
        KeyCode::Char('n') if ctrl => app.move_selection(1),
        KeyCode::Char('p') if ctrl => app.move_selection(-1),
        KeyCode::Char('d') if ctrl => app.move_selection(page(app)),
        KeyCode::Char('u') if ctrl => app.move_selection(-page(app)),

        // Esc unwinds state before it closes anything: a pending count and a
        // committed search are both modes you can be stuck in, and one you can only
        // leave by also losing the sidebar is a trap.
        KeyCode::Esc if app.pending_count.is_some() => app.pending_count = None,
        KeyCode::Esc if app.search.is_some() => app.search = None,
        KeyCode::Char('q') | KeyCode::Esc => app.quit = true,

        // Digits accumulate a count; `0` with nothing pending falls through and is
        // ignored rather than being swallowed as a no-op count.
        KeyCode::Char(ch) if !ctrl && nav::push_count(&mut app.pending_count, ch) => {}

        KeyCode::Char('j') | KeyCode::Down => app.move_counted(Direction::Down),
        KeyCode::Char('k') | KeyCode::Up => app.move_counted(Direction::Up),
        KeyCode::Char('H') | KeyCode::Left => app.jump_session(Direction::Up),
        KeyCode::Char('L') | KeyCode::Right => app.jump_session(Direction::Down),
        KeyCode::Char('K') | KeyCode::Char('J') => {
            let direction = if key.code == KeyCode::Char('K') {
                Direction::Up
            } else {
                Direction::Down
            };
            if app.move_session(direction) {
                tmux::persist_session_order(&app.sessions);
            }
        }
        KeyCode::Char('g') | KeyCode::Home => {
            app.pending_count = None;
            app.selected = 0;
        }
        KeyCode::Char('G') | KeyCode::End => {
            // `NG` in vim goes to line N; here it goes to pane N, counting from the
            // top, which is the only reading that makes `G` and the gutter agree.
            app.selected = match app.pending_count.take() {
                Some(number) => number.saturating_sub(1).min(last_block(app)),
                None => last_block(app),
            };
        }
        KeyCode::Enter | KeyCode::Char(' ') => app.activate_selection(),
        KeyCode::Tab => app.filter = app.filter.next(),
        KeyCode::Char('/') => app.open_search(),
        KeyCode::Char('R') => app.open_rename(),
        KeyCode::Char('?') => app.help = true,
        KeyCode::Char('r') => worker.request_refresh(),
        KeyCode::Char('n') => app.open_notes(),
        // Reachable from the list as well as the panel, because the moment you
        // want to write something down is the moment you are reading a pane, not
        // the moment you are already in the scratchpad.
        KeyCode::Char('a') => app.open_note_entry(),
        _ => {}
    }
}

/// Keys while the notes panel has the keyboard.
///
/// Everything here is scoped to the panel, which is what lets `Space` mean
/// "mark done" while it still means "jump to pane" in the list one row above.
fn notes_key(key: KeyEvent, app: &mut App, ctrl: bool) {
    match key.code {
        KeyCode::Char('c') if ctrl => app.quit = true,
        // Three ways out, all of them the ones a hand reaches for. `q` closes the
        // sidebar from the list, so inside the panel it has to mean "leave the
        // panel" — otherwise the mode you entered by accident costs you the pane.
        KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => app.close_notes(),
        KeyCode::Char('j') | KeyCode::Down => app.move_note(1),
        KeyCode::Char('k') | KeyCode::Up => app.move_note(-1),
        KeyCode::Char('g') | KeyCode::Home => app.move_note(isize::MIN / 2),
        KeyCode::Char('G') | KeyCode::End => app.move_note(isize::MAX / 2),
        KeyCode::Char(' ') => app.toggle_selected_note(),
        KeyCode::Char('a') => app.open_note_entry(),
        KeyCode::Enter => app.show_note_overlay(),
        // `a` gets the title down fast; `e` is where the body gets written, in a
        // real editor, in the markdown the file is already made of.
        KeyCode::Char('e') => app.edit_note_overlay(),
        KeyCode::Char('d') => app.open_note_delete(),
        KeyCode::Char('?') => app.help = true,
        _ => {}
    }
}

/// Keys while the new-note prompt is open.
///
/// The same short editor as the search box, for the same reason: this is one
/// line of text you are about to commit, not a place to compose in.
fn note_entry_key(key: KeyEvent, app: &mut App, ctrl: bool) {
    let Some(entry) = app.note_entry.as_mut() else {
        return;
    };
    match key.code {
        KeyCode::Esc => app.note_entry = None,
        KeyCode::Char('c') if ctrl => app.quit = true,
        KeyCode::Char('u') if ctrl => entry.clear(),
        KeyCode::Char('w') if ctrl => {
            let trimmed = entry.trim_end();
            let cut = trimmed.rfind(char::is_whitespace).map_or(0, |at| at + 1);
            entry.truncate(cut);
        }
        KeyCode::Enter => app.commit_note_entry(),
        // Backspacing past the start abandons the prompt, so an empty box is
        // never a mode with no visible way out — same rule as search.
        KeyCode::Backspace => {
            if entry.pop().is_none() {
                app.note_entry = None;
            }
        }
        KeyCode::Char(ch) if !ctrl => entry.push(ch),
        _ => {}
    }
}

/// Keys while the rename prompt is open.
fn rename_key(key: KeyEvent, app: &mut App, worker: &Worker, ctrl: bool) {
    let Some(rename) = app.rename.as_mut() else {
        return;
    };
    match key.code {
        KeyCode::Esc => app.rename = None,
        KeyCode::Char('c') if ctrl => app.quit = true,
        KeyCode::Char('u') if ctrl => rename.name.clear(),
        KeyCode::Enter => {
            if let Some((window_id, name)) = app.take_rename() {
                tmux::run_tmux_quiet(&["rename-window", "-t", &window_id, &name]);
                // tmux won't tell us; ask for the new name rather than showing the
                // old one until the next poll.
                worker.request_refresh();
            }
        }
        KeyCode::Backspace => {
            rename.name.pop();
        }
        KeyCode::Char(ch) if !ctrl => rename.name.push(ch),
        _ => {}
    }
}

/// Keys while the search prompt is open.
///
/// Deliberately minimal: this is a filter box, not a line editor. Ctrl-U to clear
/// and Ctrl-W to drop a word cover what anyone actually reaches for in a prompt
/// this short.
fn search_key(key: KeyEvent, app: &mut App, ctrl: bool) {
    let Some(search) = app.search.as_mut() else {
        return;
    };
    match key.code {
        // Abandon the search *and* its filter: leaving the list narrowed by a query
        // you just cancelled is how panes go missing.
        KeyCode::Esc => app.search = None,
        KeyCode::Char('c') if ctrl => app.quit = true,
        KeyCode::Char('u') if ctrl => search.query.clear(),
        KeyCode::Char('w') if ctrl => {
            let trimmed = search.query.trim_end();
            let cut = trimmed.rfind(char::is_whitespace).map_or(0, |at| at + 1);
            search.query.truncate(cut);
        }
        KeyCode::Enter => app.commit_search(),
        // Backspacing past the start leaves search entirely, so the prompt cannot
        // become a mode you are stuck in with nothing typed.
        KeyCode::Backspace => {
            if search.query.pop().is_none() {
                app.search = None;
            }
        }
        KeyCode::Char(ch) if !ctrl => search.query.push(ch),
        _ => {}
    }
}

fn last_block(app: &App) -> usize {
    app.list.blocks.len().saturating_sub(1)
}

/// One screenful of pane rows, for Ctrl-D / Ctrl-U.
///
/// Derived from the *average* block height rather than a fixed row count, so a
/// list of detail-heavy panes pages by a sensible number of panes instead of
/// flying past them.
fn page(app: &App) -> isize {
    let height = app.list_height();
    if height == 0 || app.list.blocks.is_empty() {
        return 1;
    }
    let average = (app.list.lines.len() / app.list.blocks.len()).max(1);
    ((height / average).max(1)) as isize
}

/// Move the cursor to the clicked row; `true` when it should also be activated.
///
/// Returns the decision rather than acting on it, like [`App::activation_target`], so
/// a click is testable without issuing the `switch-client` that would move the tmux
/// client running the test suite.
fn click(app: &mut App, row: u16, button: MouseButton) -> bool {
    if row < ui::HEADER_HEIGHT {
        return false;
    }
    let line = (row - ui::HEADER_HEIGHT) as usize + app.scroll;
    let Some(block) = app.list.block_at_line(line) else {
        return false;
    };
    app.selected = block;
    // Clicking a pane row means "take me there" — the list is a navigator, and making
    // you click and then press Enter is two gestures for one intention. Left button
    // only, so a right- or middle-click can move the cursor without moving the client.
    // Clicking a header or the empty space below the list lands on no block at all,
    // which is what leaves you a way to put focus in the sidebar with the mouse.
    button == MouseButton::Left
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::StatusFilter;
    use crate::model::{
        AgentKind, AgentState, AgentStatus, PaneInfo, SessionGroup, StatusSource, WindowInfo,
    };

    fn pane(pane_id: &str) -> PaneInfo {
        PaneInfo {
            pane_id: pane_id.to_owned(),
            window_id: "@1".to_owned(),
            pane_index: "0".to_owned(),
            pane_active: false,
            current_command: "claude".to_owned(),
            current_path: "/tmp".to_owned(),
            title: String::new(),
            pane_pid: None,
            status: AgentStatus {
                agent: Some(AgentKind::Claude),
                state: AgentState::Idle,
                source: StatusSource::Passive,
                seen: true,
                ..AgentStatus::default()
            },
            branch: String::new(),
            worktree: String::new(),
        }
    }

    /// An app with `count` selectable panes and a worker whose channel is live.
    fn fixture(count: usize) -> (App, Worker) {
        multi_session_fixture(&[("work", count)])
    }

    /// An app whose panes are spread across the named sessions, for session jumps.
    fn multi_session_fixture(sessions: &[(&str, usize)]) -> (App, Worker) {
        let mut app = App::new(crate::ui::Surface::Sidebar, "%99".to_owned(), (40, 40));
        let mut next_pane = 0;
        app.sessions = sessions
            .iter()
            .enumerate()
            .map(|(index, (name, count))| SessionGroup {
                session_name: (*name).to_owned(),
                session_attached: true,
                windows: vec![WindowInfo {
                    window_id: format!("@{index}"),
                    window_index: index.to_string(),
                    window_name: "w".to_owned(),
                    window_active: index == 0,
                    panes: (0..*count)
                        .map(|_| {
                            next_pane += 1;
                            pane(&format!("%{}", next_pane - 1))
                        })
                        .collect(),
                }],
            })
            .collect();
        app.rebuild();
        // No notes file: a test must not watch, read, or race the developer's own
        // scratchpad.
        (app, crate::app::worker::spawn(false, String::new(), None))
    }

    fn press(app: &mut App, worker: &Worker, code: KeyCode) {
        handle_key(KeyEvent::new(code, KeyModifiers::NONE), app, worker);
        app.rebuild();
    }

    fn press_ctrl(app: &mut App, worker: &Worker, ch: char) {
        handle_key(
            KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL),
            app,
            worker,
        );
        app.rebuild();
    }

    #[test]
    fn vim_and_arrow_motions_both_move_the_selection() {
        let (mut app, worker) = fixture(3);
        press(&mut app, &worker, KeyCode::Char('j'));
        assert_eq!(app.selected, 1);
        press(&mut app, &worker, KeyCode::Down);
        assert_eq!(app.selected, 2);
        press(&mut app, &worker, KeyCode::Char('k'));
        assert_eq!(app.selected, 1);
        press(&mut app, &worker, KeyCode::Up);
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn emacs_style_ctrl_n_and_ctrl_p_also_move() {
        let (mut app, worker) = fixture(3);
        press_ctrl(&mut app, &worker, 'n');
        assert_eq!(app.selected, 1);
        press_ctrl(&mut app, &worker, 'p');
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn g_and_shift_g_jump_to_the_ends() {
        let (mut app, worker) = fixture(5);
        press(&mut app, &worker, KeyCode::Char('G'));
        assert_eq!(app.selected, 4);
        press(&mut app, &worker, KeyCode::Char('g'));
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn shift_g_on_an_empty_list_does_not_underflow() {
        let (mut app, worker) = fixture(0);
        press(&mut app, &worker, KeyCode::Char('G'));
        assert_eq!(app.selected, 0);
    }

    // ─── counted motions ──────────────────────────────────────────────

    #[test]
    fn a_typed_count_moves_that_many_panes() {
        let (mut app, worker) = fixture(30);
        for ch in ['1', '2'] {
            press(&mut app, &worker, KeyCode::Char(ch));
        }
        assert_eq!(app.pending_count, Some(12));
        press(&mut app, &worker, KeyCode::Char('j'));
        assert_eq!(app.selected, 12);
        assert_eq!(app.pending_count, None, "the count is spent");
    }

    #[test]
    fn a_count_does_not_leak_into_the_next_motion() {
        let (mut app, worker) = fixture(30);
        press(&mut app, &worker, KeyCode::Char('5'));
        press(&mut app, &worker, KeyCode::Char('j'));
        assert_eq!(app.selected, 5);
        press(&mut app, &worker, KeyCode::Char('j'));
        assert_eq!(app.selected, 6, "second j must move exactly one");
    }

    #[test]
    fn esc_cancels_a_pending_count_before_it_closes_anything() {
        // Otherwise a mistyped count is a mode you cannot leave without also
        // losing the sidebar.
        let (mut app, worker) = fixture(5);
        press(&mut app, &worker, KeyCode::Char('9'));
        press(&mut app, &worker, KeyCode::Esc);
        assert_eq!(app.pending_count, None);
        assert!(!app.quit, "the first Esc only cancels the count");
        press(&mut app, &worker, KeyCode::Esc);
        assert!(app.quit, "a second Esc closes as usual");
    }

    #[test]
    fn a_bare_zero_is_not_swallowed_as_a_count() {
        let (mut app, worker) = fixture(5);
        press(&mut app, &worker, KeyCode::Char('0'));
        assert_eq!(app.pending_count, None);
        // But it is a digit once a count is under way.
        press(&mut app, &worker, KeyCode::Char('2'));
        press(&mut app, &worker, KeyCode::Char('0'));
        assert_eq!(app.pending_count, Some(20));
    }

    #[test]
    fn counted_shift_g_goes_to_that_pane_from_the_top() {
        let (mut app, worker) = fixture(30);
        for ch in ['1', '5'] {
            press(&mut app, &worker, KeyCode::Char(ch));
        }
        press(&mut app, &worker, KeyCode::Char('G'));
        // 15G is the fifteenth pane, which is index 14.
        assert_eq!(app.selected, 14);

        // Bare G still means "last".
        press(&mut app, &worker, KeyCode::Char('G'));
        assert_eq!(app.selected, 29);
    }

    #[test]
    fn a_counted_jump_past_the_end_lands_on_the_last_pane() {
        let (mut app, worker) = fixture(5);
        for ch in ['9', '9'] {
            press(&mut app, &worker, KeyCode::Char(ch));
        }
        press(&mut app, &worker, KeyCode::Char('G'));
        assert_eq!(app.selected, 4);
    }

    // ─── session jumps ────────────────────────────────────────────────

    #[test]
    fn shift_l_and_shift_h_jump_between_sessions() {
        let (mut app, worker) = multi_session_fixture(&[("work", 3), ("ops", 2)]);
        press(&mut app, &worker, KeyCode::Char('L'));
        assert_eq!(app.list.blocks[app.selected].target.session_name, "ops");
        press(&mut app, &worker, KeyCode::Char('H'));
        assert_eq!(app.list.blocks[app.selected].target.session_name, "work");
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn a_session_jump_discards_a_pending_count() {
        // "3L" has no sensible meaning, and leaving the count pending would make it
        // silently apply to whatever motion came next.
        let (mut app, worker) = multi_session_fixture(&[("work", 2), ("ops", 2)]);
        press(&mut app, &worker, KeyCode::Char('3'));
        press(&mut app, &worker, KeyCode::Char('L'));
        assert_eq!(app.pending_count, None);
        press(&mut app, &worker, KeyCode::Char('j'));
        assert_eq!(app.selected, 3, "must have moved one, not three");
    }

    // ─── search ───────────────────────────────────────────────────────

    fn type_str(app: &mut App, worker: &Worker, text: &str) {
        for ch in text.chars() {
            press(app, worker, KeyCode::Char(ch));
        }
    }

    #[test]
    fn slash_opens_a_prompt_where_letters_are_text_not_motions() {
        let (mut app, worker) = fixture(5);
        press(&mut app, &worker, KeyCode::Char('/'));
        type_str(&mut app, &worker, "jjk");
        assert_eq!(app.search.as_ref().unwrap().query, "jjk");
        assert_eq!(app.selected, 0, "j must type, not move");
    }

    #[test]
    fn enter_commits_the_query_and_hands_motions_back() {
        let (mut app, worker) = multi_session_fixture(&[("work", 2), ("ops", 2)]);
        press(&mut app, &worker, KeyCode::Char('/'));
        type_str(&mut app, &worker, "ops");
        press(&mut app, &worker, KeyCode::Enter);

        let search = app.search.as_ref().unwrap();
        assert!(!search.editing, "the prompt has closed");
        assert_eq!(search.query, "ops", "but the filter remains");
        // The list is narrowed, and j moves again rather than typing.
        assert_eq!(app.list.blocks.len(), 2);
        press(&mut app, &worker, KeyCode::Char('j'));
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn esc_while_typing_abandons_the_search_and_its_filter() {
        // Leaving the list narrowed by a query you just cancelled is how panes go
        // missing.
        let (mut app, worker) = multi_session_fixture(&[("work", 2), ("ops", 2)]);
        press(&mut app, &worker, KeyCode::Char('/'));
        type_str(&mut app, &worker, "ops");
        assert_eq!(app.list.blocks.len(), 2);
        press(&mut app, &worker, KeyCode::Esc);
        assert!(app.search.is_none());
        assert_eq!(app.list.blocks.len(), 4);
        assert!(!app.quit, "cancelling a search must not close the sidebar");
    }

    #[test]
    fn esc_clears_a_committed_search_before_it_closes() {
        let (mut app, worker) = fixture(3);
        press(&mut app, &worker, KeyCode::Char('/'));
        type_str(&mut app, &worker, "work");
        press(&mut app, &worker, KeyCode::Enter);
        press(&mut app, &worker, KeyCode::Esc);
        assert!(app.search.is_none());
        assert!(!app.quit);
        press(&mut app, &worker, KeyCode::Esc);
        assert!(app.quit);
    }

    #[test]
    fn backspacing_past_the_start_leaves_search_entirely() {
        // Otherwise an empty prompt is a mode with no visible way out.
        let (mut app, worker) = fixture(3);
        press(&mut app, &worker, KeyCode::Char('/'));
        type_str(&mut app, &worker, "ab");
        for _ in 0..3 {
            press(&mut app, &worker, KeyCode::Backspace);
        }
        assert!(app.search.is_none());
    }

    #[test]
    fn committing_an_empty_query_is_not_a_filter() {
        let (mut app, worker) = fixture(3);
        press(&mut app, &worker, KeyCode::Char('/'));
        press(&mut app, &worker, KeyCode::Enter);
        assert!(
            app.search.is_none(),
            "an empty committed search would show a stale / forever"
        );
    }

    #[test]
    fn ctrl_u_clears_the_query_and_ctrl_w_drops_a_word() {
        let (mut app, worker) = fixture(3);
        press(&mut app, &worker, KeyCode::Char('/'));
        type_str(&mut app, &worker, "claude auth");
        press_ctrl(&mut app, &worker, 'w');
        assert_eq!(app.search.as_ref().unwrap().query, "claude ");
        press_ctrl(&mut app, &worker, 'u');
        assert_eq!(app.search.as_ref().unwrap().query, "");
    }

    #[test]
    fn ctrl_c_still_quits_from_inside_the_prompt() {
        let (mut app, worker) = fixture(3);
        press(&mut app, &worker, KeyCode::Char('/'));
        press_ctrl(&mut app, &worker, 'c');
        assert!(app.quit);
    }

    #[test]
    fn search_and_the_status_filter_narrow_together() {
        let (mut app, worker) = multi_session_fixture(&[("work", 2), ("ops", 2)]);
        // Every fixture pane is Idle+seen, so the Working filter empties the list
        // regardless of the query — the two must compose, not override.
        press(&mut app, &worker, KeyCode::Char('/'));
        type_str(&mut app, &worker, "ops");
        press(&mut app, &worker, KeyCode::Enter);
        assert_eq!(app.list.blocks.len(), 2);
        press(&mut app, &worker, KeyCode::Tab);
        assert_eq!(app.filter, StatusFilter::Working);
        assert_eq!(app.list.blocks.len(), 0);
    }

    #[test]
    fn a_search_keeps_the_cursor_on_its_pane_when_the_list_narrows() {
        let (mut app, worker) = multi_session_fixture(&[("work", 2), ("ops", 2)]);
        app.selected = 2;
        app.rebuild();
        let anchored = app.list.blocks[2].target.pane_id.clone();

        press(&mut app, &worker, KeyCode::Char('/'));
        type_str(&mut app, &worker, "ops");
        assert_eq!(
            app.list.blocks[app.selected].target.pane_id, anchored,
            "the cursor should follow its pane into the narrowed list"
        );
    }

    // ─── help ─────────────────────────────────────────────────────────

    #[test]
    fn question_mark_opens_help_and_any_key_closes_it() {
        let (mut app, worker) = fixture(3);
        press(&mut app, &worker, KeyCode::Char('?'));
        assert!(app.help);
        press(&mut app, &worker, KeyCode::Char('x'));
        assert!(!app.help);
    }

    #[test]
    fn keys_pressed_over_the_help_page_do_not_also_act() {
        // The list is not visible, so acting on it would be acting blind.
        let (mut app, worker) = fixture(5);
        press(&mut app, &worker, KeyCode::Char('?'));
        press(&mut app, &worker, KeyCode::Char('j'));
        assert!(!app.help, "j closed the page");
        assert_eq!(app.selected, 0, "and did not also move");
    }

    #[test]
    fn ctrl_c_still_quits_from_the_help_page() {
        let (mut app, worker) = fixture(3);
        press(&mut app, &worker, KeyCode::Char('?'));
        press_ctrl(&mut app, &worker, 'c');
        assert!(app.quit);
    }

    // ─── rename ───────────────────────────────────────────────────────

    #[test]
    fn rename_opens_seeded_with_the_current_window_name() {
        // Renaming is usually an edit, not a retype.
        let (mut app, worker) = fixture(2);
        press(&mut app, &worker, KeyCode::Char('R'));
        let rename = app.rename.as_ref().unwrap();
        assert_eq!(rename.name, "w");
        assert_eq!(rename.original, "w");
        assert_eq!(rename.window_id, "@0");
    }

    #[test]
    fn rename_captures_its_target_when_opened_not_when_committed() {
        // The worker replaces the tree about once a second; resolving the target on
        // commit would rename whatever the cursor had drifted onto.
        let (mut app, worker) = multi_session_fixture(&[("work", 1), ("ops", 1)]);
        press(&mut app, &worker, KeyCode::Char('R'));
        assert_eq!(app.rename.as_ref().unwrap().window_id, "@0");

        app.selected = 1;
        app.rebuild();
        press(&mut app, &worker, KeyCode::Char('x'));
        assert_eq!(
            app.rename.as_ref().unwrap().window_id, "@0",
            "the target must not follow the cursor"
        );
    }

    #[test]
    fn typing_in_the_rename_prompt_does_not_move_the_selection() {
        let (mut app, worker) = fixture(5);
        press(&mut app, &worker, KeyCode::Char('R'));
        press_ctrl(&mut app, &worker, 'u');
        type_str(&mut app, &worker, "jjkk");
        assert_eq!(app.rename.as_ref().unwrap().name, "jjkk");
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn esc_abandons_a_rename() {
        let (mut app, worker) = fixture(2);
        press(&mut app, &worker, KeyCode::Char('R'));
        type_str(&mut app, &worker, "zzz");
        press(&mut app, &worker, KeyCode::Esc);
        assert!(app.rename.is_none());
        assert!(!app.quit);
    }

    #[test]
    fn an_unchanged_or_blank_rename_is_a_no_op() {
        // tmux treats an empty name as "go back to automatic naming", which reads as
        // the rename having silently failed.
        let (mut app, _worker) = fixture(2);
        app.open_rename();
        assert_eq!(app.take_rename(), None, "unchanged name");

        app.open_rename();
        app.rename.as_mut().unwrap().name = "   ".to_owned();
        assert_eq!(app.take_rename(), None, "blank name");

        app.open_rename();
        app.rename.as_mut().unwrap().name = " built ".to_owned();
        assert_eq!(
            app.take_rename(),
            Some(("@0".to_owned(), "built".to_owned())),
            "a real change is trimmed and applied"
        );
    }

    #[test]
    fn rename_on_an_empty_list_opens_nothing() {
        let (mut app, worker) = fixture(0);
        press(&mut app, &worker, KeyCode::Char('R'));
        assert!(app.rename.is_none());
    }

    // ─── the notes panel ──────────────────────────────────────────────

    /// A fixture whose scratchpad is a real file in a temp dir, because `a` and
    /// `Space` write through `notes::add` / `notes::update` and the point of
    /// those is that they re-read under a lock. Named per test so a parallel run
    /// cannot have two of them on the same file.
    fn notes_fixture(name: &str, titles: &[&str]) -> (App, Worker, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!("agent-mgr-input-{name}.md"));
        let _ = std::fs::remove_file(&path);
        let body: String = titles
            .iter()
            .map(|title| format!("## [ ] {title}\n\n"))
            .collect();
        if !body.is_empty() {
            std::fs::write(&path, &body).expect("write scratch notes");
        }
        let (mut app, worker) = fixture(3);
        app.notes_file = Some(path.clone());
        app.notes = crate::notes::parse(&body);
        app.rebuild();
        (app, worker, path)
    }

    #[test]
    fn n_gives_the_panel_the_keyboard_and_esc_gives_it_back() {
        let (mut app, worker, path) = notes_fixture("focus", &["one", "two"]);
        press(&mut app, &worker, KeyCode::Char('n'));
        assert_eq!(app.notes_focus.map(|state| state.selected), Some(0));
        press(&mut app, &worker, KeyCode::Esc);
        assert!(app.notes_focus.is_none());
        assert!(!app.quit, "leaving the panel must not close the sidebar");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn q_inside_the_panel_leaves_the_panel_rather_than_the_sidebar() {
        // `q` closes the sidebar from the list, so a mode entered by accident
        // would otherwise cost you the pane.
        let (mut app, worker, path) = notes_fixture("q-leaves", &["one"]);
        press(&mut app, &worker, KeyCode::Char('n'));
        press(&mut app, &worker, KeyCode::Char('q'));
        assert!(app.notes_focus.is_none());
        assert!(!app.quit);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn focusing_an_empty_scratchpad_does_nothing() {
        // The panel is zero rows tall with no notes; focusing something invisible
        // is a mode with no way to tell you are in it.
        let (mut app, worker, _path) = notes_fixture("empty", &[]);
        press(&mut app, &worker, KeyCode::Char('n'));
        assert!(app.notes_focus.is_none());
    }

    #[test]
    fn motions_in_the_panel_move_notes_and_not_the_pane_list() {
        // The whole reason the panel is a mode: `j` has to mean two things one
        // row apart.
        let (mut app, worker, path) = notes_fixture("motion", &["one", "two", "three"]);
        press(&mut app, &worker, KeyCode::Char('n'));
        press(&mut app, &worker, KeyCode::Char('j'));
        assert_eq!(app.notes_focus.unwrap().selected, 1);
        assert_eq!(app.selected, 0, "the pane cursor must not have moved");
        press(&mut app, &worker, KeyCode::Char('G'));
        assert_eq!(app.notes_focus.unwrap().selected, 2);
        press(&mut app, &worker, KeyCode::Char('g'));
        assert_eq!(app.notes_focus.unwrap().selected, 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_panel_cursor_saturates_instead_of_wrapping() {
        // A cursor that jumps from the last note to the first is indistinguishable
        // from one that lost its place.
        let (mut app, worker, path) = notes_fixture("saturate", &["one", "two"]);
        press(&mut app, &worker, KeyCode::Char('n'));
        for _ in 0..5 {
            press(&mut app, &worker, KeyCode::Char('k'));
        }
        assert_eq!(app.notes_focus.unwrap().selected, 0);
        for _ in 0..5 {
            press(&mut app, &worker, KeyCode::Char('j'));
        }
        assert_eq!(app.notes_focus.unwrap().selected, 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn space_marks_a_note_done_in_the_file_not_just_on_screen() {
        let (mut app, worker, path) = notes_fixture("toggle", &["one", "two"]);
        press(&mut app, &worker, KeyCode::Char('n'));
        press(&mut app, &worker, KeyCode::Char('j'));
        press(&mut app, &worker, KeyCode::Char(' '));

        assert!(app.notes.notes[1].done, "the snapshot updated immediately");
        let (fresh, _) = crate::notes::load(&path).expect("reread");
        assert!(fresh.notes[1].done, "and so did the file");
        assert!(!fresh.notes[0].done, "and only that one");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn space_in_the_list_still_jumps_to_a_pane() {
        // The collision the mode exists to resolve: outside the panel, Space must
        // keep meaning what it always did.
        let (mut app, worker, path) = notes_fixture("space-list", &["one"]);
        app.own_pane = "%99".to_owned();
        assert!(
            app.activation_target().is_some(),
            "Space in the list is still an activation, not a toggle"
        );
        press(&mut app, &worker, KeyCode::Char(' '));
        assert!(!app.notes.notes[0].done, "the note must not have been touched");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_opens_a_prompt_from_the_list_without_focusing_the_panel_first() {
        // The moment you want to write something down is the moment you are
        // reading a pane, not the moment you are already in the scratchpad.
        let (mut app, worker, path) = notes_fixture("add-from-list", &["one"]);
        press(&mut app, &worker, KeyCode::Char('a'));
        assert_eq!(app.note_entry.as_deref(), Some(""));
        assert!(app.notes_focus.is_none());
        type_str(&mut app, &worker, "jjk");
        assert_eq!(
            app.note_entry.as_deref(),
            Some("jjk"),
            "letters must type, not move"
        );
        assert_eq!(app.selected, 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn committing_the_prompt_appends_to_the_file_and_lands_on_it() {
        let (mut app, worker, path) = notes_fixture("commit", &["one"]);
        press(&mut app, &worker, KeyCode::Char('a'));
        type_str(&mut app, &worker, "written from the panel");
        press(&mut app, &worker, KeyCode::Enter);

        assert!(app.note_entry.is_none());
        let (fresh, _) = crate::notes::load(&path).expect("reread");
        assert_eq!(fresh.len(), 2);
        assert_eq!(fresh.notes[1].title, "written from the panel");
        // Appends never renumber, so the new note is the last one — and the
        // cursor arriving on it is the confirmation the write happened.
        assert_eq!(app.notes_focus.map(|state| state.selected), Some(1));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_blank_note_is_not_written() {
        // An empty heading in the file reads as corruption, and there would be no
        // way to select it in order to delete it.
        let (mut app, _worker, path) = notes_fixture("blank", &["one"]);
        app.note_entry = Some("   ".to_owned());
        assert_eq!(app.take_note_entry(), None);
        app.note_entry = Some(String::new());
        assert_eq!(app.take_note_entry(), None);
        app.note_entry = Some("  real  ".to_owned());
        assert_eq!(app.take_note_entry(), Some("real".to_owned()));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn backspacing_past_the_start_abandons_the_note_prompt() {
        let (mut app, worker, path) = notes_fixture("backspace", &["one"]);
        press(&mut app, &worker, KeyCode::Char('a'));
        type_str(&mut app, &worker, "ab");
        for _ in 0..3 {
            press(&mut app, &worker, KeyCode::Backspace);
        }
        assert!(app.note_entry.is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn enter_on_a_note_returns_a_show_overlay_target_rather_than_spawning_it() {
        // Split from the `display-popup` the way `activation_target` is split from
        // `switch-client`: spawning one in a test run would put a popup over the
        // developer's screen.
        let (mut app, worker, path) = notes_fixture("overlay", &["one", "two"]);
        assert_eq!(app.overlay_target(), None, "no target without focus");
        press(&mut app, &worker, KeyCode::Char('n'));
        press(&mut app, &worker, KeyCode::Char('j'));
        assert_eq!(app.overlay_target(), Some(1));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn d_asks_before_deleting_and_names_what_it_would_delete() {
        // The one action here with no undo — the file is the only copy — so it is
        // worth a keypress, and the prompt has to say which note or you are
        // answering blind.
        let (mut app, worker, path) = notes_fixture("delete-asks", &["one", "two", "three"]);
        press(&mut app, &worker, KeyCode::Char('n'));
        press(&mut app, &worker, KeyCode::Char('j'));
        press(&mut app, &worker, KeyCode::Char('d'));
        assert_eq!(app.note_delete, Some((1, "two".to_owned())));
        assert_eq!(app.notes.len(), 3, "nothing gone yet");

        press(&mut app, &worker, KeyCode::Char('y'));
        assert!(app.note_delete.is_none());
        let (fresh, _) = crate::notes::load(&path).expect("reread");
        assert_eq!(fresh.len(), 2);
        assert_eq!(fresh.notes[0].title, "one");
        assert_eq!(fresh.notes[1].title, "three");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn anything_but_y_cancels_the_delete() {
        // The safe answer should be the one you give by accident.
        for code in [
            KeyCode::Esc,
            KeyCode::Char('n'),
            KeyCode::Enter,
            KeyCode::Char('x'),
            KeyCode::Char('Y'),
        ] {
            let (mut app, worker, path) = notes_fixture("delete-cancel", &["one", "two"]);
            press(&mut app, &worker, KeyCode::Char('n'));
            press(&mut app, &worker, KeyCode::Char('d'));
            press(&mut app, &worker, code);
            assert!(app.note_delete.is_none(), "{code:?} left the prompt open");
            assert_eq!(app.notes.len(), 2, "{code:?} deleted something");
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn the_confirmation_swallows_the_keyboard() {
        // Otherwise the key you answer with also moves the cursor underneath.
        let (mut app, worker, path) = notes_fixture("delete-swallow", &["one", "two", "three"]);
        press(&mut app, &worker, KeyCode::Char('n'));
        press(&mut app, &worker, KeyCode::Char('d'));
        press(&mut app, &worker, KeyCode::Char('j'));
        assert_eq!(
            app.notes_focus.unwrap().selected,
            0,
            "j answered the prompt, it must not also have moved"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn deleting_the_last_note_leaves_the_panel_rather_than_focusing_nothing() {
        // The panel collapses to zero rows with no notes, and focus on an
        // invisible panel is a mode with no way out.
        let (mut app, worker, path) = notes_fixture("delete-last", &["only"]);
        press(&mut app, &worker, KeyCode::Char('n'));
        press(&mut app, &worker, KeyCode::Char('d'));
        press(&mut app, &worker, KeyCode::Char('y'));
        assert!(app.notes.is_empty());
        assert!(app.notes_focus.is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_cursor_stays_in_range_after_deleting_the_last_row() {
        // Deletion is the one operation that renumbers.
        let (mut app, worker, path) = notes_fixture("delete-clamp", &["one", "two"]);
        press(&mut app, &worker, KeyCode::Char('n'));
        press(&mut app, &worker, KeyCode::Char('G'));
        assert_eq!(app.notes_focus.unwrap().selected, 1);
        press(&mut app, &worker, KeyCode::Char('d'));
        press(&mut app, &worker, KeyCode::Char('y'));
        assert_eq!(app.notes.len(), 1);
        assert_eq!(app.notes_focus.unwrap().selected, 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn d_outside_the_panel_does_nothing() {
        // `d` is panel-only; in the list it must not arm anything.
        let (mut app, worker, path) = notes_fixture("delete-list", &["one"]);
        press(&mut app, &worker, KeyCode::Char('d'));
        assert!(app.note_delete.is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ctrl_c_still_quits_from_the_panel_and_from_its_prompt() {
        let (mut app, worker, path) = notes_fixture("ctrl-c", &["one"]);
        press(&mut app, &worker, KeyCode::Char('n'));
        press_ctrl(&mut app, &worker, 'c');
        assert!(app.quit);

        let (mut app, worker, _) = notes_fixture("ctrl-c", &["one"]);
        press(&mut app, &worker, KeyCode::Char('a'));
        press_ctrl(&mut app, &worker, 'c');
        assert!(app.quit);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn tab_cycles_the_filter() {
        let (mut app, worker) = fixture(2);
        assert_eq!(app.filter, StatusFilter::All);
        press(&mut app, &worker, KeyCode::Tab);
        assert_eq!(app.filter, StatusFilter::Working);
    }

    #[test]
    fn q_esc_and_ctrl_c_all_close_the_sidebar() {
        for code in [KeyCode::Char('q'), KeyCode::Esc] {
            let (mut app, worker) = fixture(1);
            press(&mut app, &worker, code);
            assert!(app.quit, "{code:?} should quit");
        }
        let (mut app, worker) = fixture(1);
        press_ctrl(&mut app, &worker, 'c');
        assert!(app.quit);
    }

    #[test]
    fn a_key_release_is_ignored_so_motions_do_not_double() {
        let (mut app, worker) = fixture(3);
        handle_key(
            KeyEvent::new_with_kind(KeyCode::Char('j'), KeyModifiers::NONE, KeyEventKind::Release),
            &mut app,
            &worker,
        );
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn ctrl_d_and_ctrl_u_page_by_several_panes_at_once() {
        let (mut app, worker) = fixture(30);
        press_ctrl(&mut app, &worker, 'd');
        assert!(app.selected > 1, "a page should be more than one row");
        let after_down = app.selected;
        press_ctrl(&mut app, &worker, 'u');
        assert!(app.selected < after_down);
    }

    #[test]
    fn paging_an_empty_list_is_a_no_op() {
        let (mut app, worker) = fixture(0);
        press_ctrl(&mut app, &worker, 'd');
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn clicking_a_pane_row_selects_it_and_asks_to_jump() {
        let (mut app, worker) = fixture(3);
        let _ = &worker;
        // Row 0 is the app header; the list starts below it. Session and window
        // headers take the first two list lines, so the first pane is at row 3.
        let first_pane_line = app.list.block_line(0);
        let row = ui::HEADER_HEIGHT + first_pane_line as u16 + 2;
        assert!(
            click(&mut app, row, MouseButton::Left),
            "a left click on a pane row is a jump, not just a cursor move"
        );
        assert_eq!(app.selected, 2);
    }

    #[test]
    fn a_right_click_moves_the_cursor_without_moving_the_client() {
        // So a stray or non-primary button cannot switch the client out from under
        // you — and a right-click stays available for selecting without jumping.
        let (mut app, worker) = fixture(3);
        let _ = &worker;
        let row = ui::HEADER_HEIGHT + app.list.block_line(0) as u16 + 2;
        assert!(!click(&mut app, row, MouseButton::Right));
        assert_eq!(app.selected, 2);
    }

    #[test]
    fn clicking_a_header_or_empty_space_leaves_the_selection_alone() {
        // Also the one way left to put focus in the sidebar with the mouse, now that
        // clicking a pane row jumps: land on a row that owns no block.
        let (mut app, worker) = fixture(2);
        let _ = &worker;
        app.selected = 1;
        // The app header.
        assert!(!click(&mut app, 0, MouseButton::Left));
        assert_eq!(app.selected, 1);
        // The session header, which owns no block.
        assert!(!click(&mut app, ui::HEADER_HEIGHT, MouseButton::Left));
        assert_eq!(app.selected, 1);
        // Far below the last row.
        assert!(!click(&mut app, 200, MouseButton::Left));
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn clicking_accounts_for_the_scroll_offset() {
        let (mut app, worker) = fixture(20);
        let _ = &worker;
        app.scroll = 5;
        let target_line = 7;
        let expected = app.list.block_at_line(app.scroll + target_line);
        click(
            &mut app,
            ui::HEADER_HEIGHT + target_line as u16,
            MouseButton::Left,
        );
        if let Some(expected) = expected {
            assert_eq!(app.selected, expected);
        }
    }

    #[test]
    fn activating_our_own_pane_is_refused() {
        // Switching tmux to the sidebar itself would move focus into a pane the
        // user cannot type into usefully. Asserted on the decision rather than by
        // calling activate_selection, which would issue a real switch-client and
        // move the tmux client running the test suite.
        let (mut app, worker) = fixture(1);
        let _ = &worker;
        app.own_pane = app.list.blocks[0].target.pane_id.clone();
        assert!(app.activation_target().is_none());
    }
}
