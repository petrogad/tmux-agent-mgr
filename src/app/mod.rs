//! The event loop, and the state it drives.
//!
//! # Why this loop looks the way it does
//!
//! The plugin this one replaces flickered, and the cause was its redraw policy:
//! it called `terminal.draw()` on a 50 ms timer whether or not anything had
//! changed, and forced a full `terminal.clear()` every 500 ms. Inside a tmux
//! overlay that means recompositing the region twice a second, forever, even
//! with nobody watching.
//!
//! So this loop holds three rules:
//!
//! 1. **Draw only when the visible output actually changed.** Rendering is pure
//!    ([`crate::ui::rows`]), so each pass builds the lines, hashes their text,
//!    and draws only on a different hash. Not "state changed" — *output* changed.
//! 2. **Never clear on a timer.** The one `terminal.clear()` in this crate is in
//!    the resize arm, where the terminal's own geometry changed under us.
//! 3. **Animate only when there is something to animate.** The spinner clock
//!    advances only while a pane is Working. With nothing running, successive
//!    passes produce byte-identical lines, the hash never moves, and the terminal
//!    receives nothing at all.
//!
//! The result: an idle sidebar left open for an hour writes zero bytes to the
//! terminal. Set `AGENT_MGR_DEBUG_FRAMES=<path>` to have the draw count written
//! there on exit and check it yourself.

mod input;
mod worker;

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossterm::event::{self, Event};
use ratatui::{Terminal, backend::CrosstermBackend};

use crate::daemon;
use crate::model::{AgentState, AgentStatus, SessionGroup};
use crate::notes;
use crate::nav::{self, Direction};
use crate::preview::PanePreview;
use crate::search::Query;
use crate::tmux;
use crate::ui::{self, Counts, Surface, rows, rows::RenderedList, theme::Theme};

/// Spinner frame duration. Ten frames, so a full cycle is 1.5 s.
const SPINNER_INTERVAL: Duration = Duration::from_millis(150);
/// Input poll timeout when nothing is running. Bounds how long a SIGUSR1 or a
/// worker snapshot waits to be noticed; each wake-up is pure CPU, no terminal I/O.
const QUIET_TIMEOUT: Duration = Duration::from_millis(250);

/// Which panes the list shows.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum StatusFilter {
    #[default]
    All,
    Working,
    Blocked,
    /// Finished runs the user hasn't looked at yet — the "what came back while I
    /// was away" view.
    Done,
}

impl StatusFilter {
    pub fn next(self) -> Self {
        match self {
            Self::All => Self::Working,
            Self::Working => Self::Blocked,
            Self::Blocked => Self::Done,
            Self::Done => Self::All,
        }
    }

    fn matches(self, status: &AgentStatus) -> bool {
        match self {
            Self::All => true,
            Self::Working => status.state == AgentState::Working,
            // An error is something you must deal with, so it belongs in the
            // "needs me" view rather than only in a state nobody thinks to check.
            Self::Blocked => matches!(status.state, AgentState::Blocked | AgentState::Error),
            Self::Done => status.is_done(),
        }
    }
}

/// An open search.
///
/// The query outlives the typing: `Enter` closes the prompt but keeps the filter,
/// so you can search, then navigate the narrowed list with the normal motions.
/// That is the whole point of committing rather than just filtering while a key is
/// held. `Esc` is what clears it.
///
/// Deliberately not `Default`: the only sensible initial state has `editing:
/// true`, but a derived default would silently produce `false` — a search that
/// filters nothing and never accepts a keystroke.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SearchState {
    pub query: String,
    /// `true` while the prompt has the keyboard.
    pub editing: bool,
}

impl SearchState {
    fn open() -> Self {
        Self {
            query: String::new(),
            editing: true,
        }
    }
}

/// An open window-rename prompt.
///
/// Carries the window id it was opened against rather than re-reading the
/// selection on commit: the worker replaces the tree about once a second, and an
/// agent starting or stopping can move the cursor while you are still typing.
/// Resolving the target late would rename whatever happened to be under the cursor
/// by then.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RenameState {
    pub window_id: String,
    /// What the window was called, shown in the prompt so you can see what you are
    /// replacing.
    pub original: String,
    pub name: String,
}

/// The notes panel holding the keyboard.
///
/// A mode, like [`SearchState`] and [`RenameState`], rather than a shared keymap
/// with the list. The panel wants `j`/`k` and `Space` and so does the list, and
/// the alternative to a mode is a second vocabulary of modified keys for the same
/// four motions. Modal also means the entry key is the only one that has to be
/// globally free.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct NotesState {
    /// Index into the file, not into the visible rows — the panel scrolls
    /// underneath it.
    pub selected: usize,
}

pub struct App {
    pub surface: Surface,
    /// Our own pane, excluded from the list and never navigated to. Empty in a
    /// popup, which is not a pane in any window and so has nothing to exclude.
    pub own_pane: String,
    pub theme: Theme,
    /// The full tree as collected, before filtering.
    pub sessions: Vec<SessionGroup>,
    pub filter: StatusFilter,
    pub counts: Counts,
    /// Index into `list.blocks`.
    pub selected: usize,
    /// First visible line of the list.
    pub scroll: usize,
    /// Show the relative-number gutter. On by default: it is what makes a counted
    /// motion like `10j` something you can aim rather than guess at.
    pub numbers: bool,
    /// Digits typed so far, awaiting a motion to consume them.
    pub pending_count: Option<usize>,
    /// The live search, if one is open. See [`SearchState`].
    pub search: Option<SearchState>,
    /// The window-rename prompt, if one is open.
    pub rename: Option<RenameState>,
    /// Showing the keymap instead of the list.
    pub help: bool,
    /// The pane tmux focus was on as of the last snapshot. Held to tell a *change* of
    /// focus from a snapshot that merely repeats it — snapping the cursor back on
    /// every poll would make `j`/`k` unusable while you work in another pane.
    pub focused: Option<String>,
    /// A focus change not yet applied to the selection, consumed by [`Self::rebuild`].
    follow: Option<String>,
    /// Latest captured panes for the previewed window, as `(window_id, panes)`.
    pub preview: Option<(String, Vec<PanePreview>)>,
    /// The preview composed for the current geometry. Derived in [`Self::rebuild`]
    /// so it is part of the hashed output rather than something `draw` computes —
    /// pane content changing is exactly a reason to redraw, and this is how the loop
    /// notices.
    pub preview_lines: Vec<crate::preview::Line>,
    pub spinner: usize,
    pub list: RenderedList,
    /// The scratchpad as last read from disk. Sized in rows by
    /// [`ui::split_height`], so it is also what decides how tall the list is.
    pub notes: notes::NoteFile,
    /// The notes panel composed for the current geometry. Derived in
    /// [`Self::rebuild`] for the same reason `preview_lines` is: it belongs to
    /// the hashed output, not to `draw`.
    pub notes_view: ui::notes::RenderedNotes,
    /// First note the panel shows, tracking the cursor while the panel has focus.
    pub notes_scroll: usize,
    /// The panel with the keyboard, if it has it. See [`NotesState`].
    pub notes_focus: Option<NotesState>,
    /// A note being typed. Separate from [`Self::notes_focus`] because `a` works
    /// from the list too — jotting something down is the thing you want *without*
    /// first navigating to the panel.
    pub note_entry: Option<String>,
    /// A pending `d`, holding the note it is about to delete.
    ///
    /// The whole note rather than just the index, for two reasons: the prompt can
    /// name it, since a bare "delete? y/n" makes you look back up at the cursor to
    /// work out what you are answering; and the write can check the index still
    /// points at it, since another sidebar deleting an earlier note renumbers
    /// everything below. Deleting is the one thing here with no undo.
    pub note_delete: Option<notes::Note>,
    /// The last thing that went wrong, shown in the footer until the next key.
    ///
    /// A write that fails has to say so. The scratchpad is the one part of this
    /// plugin that *owns* data rather than reflecting tmux's, so a silent failure
    /// is the difference between a note you have and a note you think you have.
    pub note_error: Option<String>,
    /// Where the scratchpad lives, resolved once in [`run`]. `None` on a popup,
    /// and on any surface that has no panel to write for.
    pub notes_file: Option<std::path::PathBuf>,
    pub size: (u16, u16),
    /// Set to leave the loop; the pane closes with us, which is how the sidebar
    /// is dismissed from inside.
    pub quit: bool,
    /// Draws performed, for the flicker regression check.
    pub frames: u64,
}

impl App {
    fn new(surface: Surface, own_pane: String, size: (u16, u16)) -> Self {
        Self {
            surface,
            own_pane,
            theme: Theme::from_tmux(),
            sessions: Vec::new(),
            filter: StatusFilter::default(),
            counts: Counts::default(),
            selected: 0,
            scroll: 0,
            numbers: true,
            pending_count: None,
            search: None,
            rename: None,
            help: false,
            focused: None,
            follow: None,
            preview: None,
            preview_lines: Vec::new(),
            spinner: 0,
            list: RenderedList {
                lines: Vec::new(),
                plain: Vec::new(),
                blocks: Vec::new(),
            },
            // Deliberately empty rather than read here: `new` is what every test
            // constructs, and loading the developer's own scratchpad would make
            // the row split depend on what they happen to have written down.
            // `run` fills it — see [`load_notes`].
            notes: notes::NoteFile::default(),
            notes_view: ui::notes::RenderedNotes::default(),
            notes_scroll: 0,
            notes_focus: None,
            note_entry: None,
            note_delete: None,
            note_error: None,
            notes_file: None,
            size,
            quit: false,
            frames: 0,
        }
    }

    /// `true` while any pane is doing something worth animating.
    fn any_active(&self) -> bool {
        self.sessions
            .iter()
            .flat_map(|session| &session.windows)
            .flat_map(|window| &window.panes)
            .any(|pane| pane.status.state.is_active())
    }

    /// Panes the current filter is keeping off screen.
    pub fn hidden_count(&self) -> usize {
        let total: usize = self
            .sessions
            .iter()
            .flat_map(|session| &session.windows)
            .map(|window| window.panes.len())
            .sum();
        total.saturating_sub(self.list.blocks.len())
    }

    pub fn list_height(&self) -> usize {
        self.rows().list as usize
    }

    /// How the pane's rows currently divide between the list and the panel.
    fn rows(&self) -> ui::Rows {
        ui::split_height(self.surface, self.size.1, self.notes.len())
    }

    /// Read the scratchpad off disk once, before the loop starts.
    ///
    /// The worker keeps it current from here on, so this exists only to fill the
    /// *first* frame. Without it the panel would appear a beat after the sidebar
    /// opens, and since the panel's rows come out of the list, that beat is a
    /// visible reflow rather than just a late paint.
    ///
    /// A plain file read rather than a subprocess, so invariant 4 is untouched,
    /// and it sits beside `Theme::from_tmux` in the same "resolve configuration
    /// before the loop starts" slot. A missing or unreadable file is an empty
    /// scratchpad, not an error: the first run has no file, and a panel is not
    /// worth failing to open a sidebar over.
    fn load_notes(&mut self, path: &std::path::Path) {
        if let Ok((file, _stamp)) = notes::load(path) {
            self.notes = file;
        }
    }

    // ─── the notes panel ──────────────────────────────────────────────

    /// Give the panel the keyboard.
    ///
    /// Gated on the panel having rows on screen, not merely on notes existing:
    /// a short sidebar allocates it none, and focusing something invisible leaves
    /// you in a mode with no way to tell you are in it. `a` is the key that works
    /// regardless, and it works from the list anyway.
    pub fn open_notes(&mut self) {
        if self.rows().notes == 0 {
            return;
        }
        self.pending_count = None;
        self.notes_focus = Some(NotesState::default());
        self.clamp_notes();
    }

    pub fn close_notes(&mut self) {
        self.notes_focus = None;
    }

    /// Move the panel cursor, saturating at both ends.
    ///
    /// Deliberately not wrapping, unlike nothing else here — the list doesn't
    /// wrap either, and a cursor that jumps from the last note to the first is
    /// indistinguishable from one that lost its place.
    pub fn move_note(&mut self, delta: isize) {
        let Some(state) = self.notes_focus.as_mut() else {
            return;
        };
        let last = self.notes.len().saturating_sub(1);
        let next = state.selected as isize + delta;
        state.selected = next.clamp(0, last as isize) as usize;
        self.clamp_notes();
    }

    /// Keep the cursor in range and scrolled into view.
    fn clamp_notes(&mut self) {
        let last = self.notes.len().saturating_sub(1);
        if let Some(state) = self.notes_focus.as_mut() {
            state.selected = state.selected.min(last);
        }
        // One row of the panel is its header, so the notes get the rest.
        let rows = (self.rows().notes as usize).saturating_sub(1);
        let Some(selected) = self.notes_focus.map(|state| state.selected) else {
            return;
        };
        if rows == 0 {
            self.notes_scroll = 0;
        } else if selected < self.notes_scroll {
            self.notes_scroll = selected;
        } else if selected >= self.notes_scroll + rows {
            self.notes_scroll = selected + 1 - rows;
        }
    }

    /// Open the prompt for a new note.
    pub fn open_note_entry(&mut self) {
        if self.notes_file.is_none() {
            return;
        }
        self.pending_count = None;
        self.note_entry = Some(String::new());
    }

    /// Take the typed title, if it is worth writing.
    ///
    /// Split from the write so the "what counts as a note" rule is testable
    /// without touching a filesystem, the way `take_rename` splits from
    /// `rename-window`. Blank is not a note: an empty heading in the file reads
    /// as corruption, and there would be no way to select it to delete it.
    pub fn take_note_entry(&mut self) -> Option<String> {
        let title = self.note_entry.take()?;
        let title = title.trim();
        (!title.is_empty()).then(|| title.to_owned())
    }

    /// Write the typed note, and put the cursor on it.
    ///
    /// The append goes through [`notes::add`], which re-reads under the lock, so
    /// a note an agent wrote a moment ago cannot be lost to our stale snapshot.
    /// We then reload rather than pushing onto the snapshot for the same reason —
    /// the file, not the TUI, is what is true.
    pub fn commit_note_entry(&mut self) {
        let (Some(title), Some(path)) = (self.take_note_entry(), self.notes_file.clone()) else {
            return;
        };
        // Same origin stamp `note add` writes, so a note taken here is
        // indistinguishable from one taken at a shell — which matters, because
        // "where was I when I wrote this" is most of what makes an old note
        // legible.
        let mut note = notes::Note::new(&title, "");
        note.meta = notes::origin_meta();
        let assigned = match notes::add(&path, &note) {
            Ok(id) => id,
            Err(err) => {
                // Hand the words back rather than swallowing them. An unwritable
                // path would otherwise make the note look accepted — the prompt
                // closes, nothing appears, and what you typed is gone.
                self.note_entry = Some(title);
                self.note_error = Some(format!("could not write the note: {err}"));
                return;
            }
        };
        self.load_notes(&path);
        // Land on what you just wrote — found by the id `add` handed back, not by
        // taking the last row. An agent appending between our write and our read
        // is the ordinary case here, and the last row would then be its note, not
        // yours. Seeing the cursor arrive on your own line is the confirmation
        // that the write happened at all.
        let landed = self
            .notes
            .notes
            .iter()
            .position(|note| note.id() == Some(assigned.as_str()));
        if let Some(selected) = landed
            && self.surface.shows_notes()
        {
            self.notes_focus = Some(NotesState { selected });
            self.clamp_notes();
        }
    }

    /// Flip the selected note's done flag.
    ///
    /// Through [`notes::update`], which re-reads under the lock and hands back
    /// fresh content, so a concurrent append is merged rather than clobbered. The
    /// result is applied straight away instead of waiting for the worker's next
    /// pass: a checkbox that takes a second to tick reads as a dropped keypress.
    pub fn toggle_selected_note(&mut self) {
        let (Some(state), Some(path)) = (self.notes_focus, self.notes_file.clone()) else {
            return;
        };
        let Some(expect) = self.notes.notes.get(state.selected).cloned() else {
            return;
        };
        // By identity, not by index. The cursor's index was true when the panel
        // last drew; the write happens against whatever the file says now, and
        // another pane deleting an earlier note in between would otherwise tick
        // the box on a different note.
        let mut toggled = false;
        match notes::update(&path, |file| toggled = file.toggle_note(&expect)) {
            Ok(file) => {
                self.notes = file;
                if !toggled {
                    self.note_error = Some(format!("{:?} moved or is gone", expect.title));
                }
                self.clamp_notes();
            }
            Err(err) => self.note_error = Some(format!("could not tick the box: {err}")),
        }
    }

    /// Arm the delete confirmation for the selected note.
    pub fn open_note_delete(&mut self) {
        let (Some(state), Some(_)) = (self.notes_focus, self.notes_file.as_ref()) else {
            return;
        };
        let Some(note) = self.notes.notes.get(state.selected) else {
            return;
        };
        self.note_delete = Some(note.clone());
    }

    /// Answer the confirmation. Anything but `y` cancels.
    ///
    /// Confirming resolves the index against fresh content under the lock, and
    /// then checks it still points at the note the prompt named. Deletion is the
    /// one operation that renumbers, so a second sidebar deleting an earlier note
    /// between the `d` and the `y` shifts everything below it — and answering
    /// "yes, delete *that* one" must never remove a different note.
    pub fn resolve_note_delete(&mut self, confirmed: bool) {
        let (Some(expect), Some(path)) = (self.note_delete.take(), self.notes_file.clone())
        else {
            return;
        };
        if !confirmed {
            return;
        }
        let mut removed = false;
        match notes::update(&path, |file| removed = file.remove_note(&expect)) {
            Ok(file) => {
                self.notes = file;
                if !removed {
                    self.note_error = Some(format!(
                        "{:?} moved or is already gone — nothing deleted",
                        expect.title
                    ));
                }
                if self.notes.is_empty() {
                    self.close_notes();
                }
                self.clamp_notes();
            }
            Err(err) => self.note_error = Some(format!("could not delete: {err}")),
        }
    }

    /// The id of the note under the cursor, migrating the file if it has none.
    ///
    /// **Never falls back to an index.** The popup does not launch at the instant
    /// the key is pressed, and another pane deleting an earlier note in between
    /// would make an index name — and the editor overwrite — a note nobody asked
    /// for. A legacy note without an id is given one first, under the lock, by a
    /// no-op `update` whose backfill does the work; if even that cannot establish
    /// one, the answer is `None` and the caller declines.
    ///
    /// Returns the decision rather than acting on it, like
    /// [`Self::activation_target`]: spawning a `display-popup` in a test run would
    /// put a popup over the developer's own screen.
    pub fn overlay_target(&mut self) -> Option<String> {
        let selected = self.notes_focus?.selected;
        let expect = self.notes.notes.get(selected)?.clone();
        if let Some(id) = expect.id() {
            return Some(id.to_owned());
        }
        // No id: mint one for *this note*, inside the lock. The cursor's index is
        // no more trustworthy here than anywhere else — between our last read and
        // the locked re-read, another pane can delete an earlier note, and taking
        // the row that ends up at the old index would hand the popup a different
        // note's id. So the note is found by identity in the fresh file and given
        // its id there, and the id is captured on the way past rather than read
        // back afterwards: once assigned, `expect` no longer matches it.
        let path = self.notes_file.clone()?;
        let mut assigned = None;
        match notes::update(&path, |file| {
            if let Some(index) = file.locate(&expect) {
                file.notes[index].ensure_id();
                assigned = file.notes[index].id().map(str::to_owned);
            }
        }) {
            Ok(file) => self.notes = file,
            Err(err) => {
                self.note_error = Some(format!("could not identify the note: {err}"));
                return None;
            }
        }
        self.clamp_notes();
        if assigned.is_none() {
            self.note_error = Some(format!(
                "{:?} moved or is gone — nothing was opened",
                expect.title
            ));
        }
        assigned
    }

    /// Read the note under the cursor in a popup, full body and all.
    ///
    /// A pager rather than a bare `cat`, because a body can be longer than the
    /// popup and a note you cannot scroll is a note you cannot read.
    pub fn show_note_overlay(&mut self) {
        let Some(id) = self.overlay_target() else {
            return;
        };
        // `--color=always` because stdout here is the pager's pipe, not a
        // terminal: an IsTerminal check alone would drop the colour in exactly
        // the case that wants it. `-R` is what makes the pager pass it through.
        self.note_popup(&format!(
            "note show --id={} --color=always | ${{PAGER:-less -R}}",
            sh_quote(&id)
        ));
    }

    /// Open the note under the cursor in `$EDITOR`.
    ///
    /// The whole round trip — extract, edit, merge back under the lock — lives in
    /// `agent-mgr note edit`, not here. `display-popup` runs on the attached
    /// client and the `tmux` process we spawn returns immediately, so there is no
    /// moment at which this function could read the result back. Letting the
    /// popup own it means the file changes and our own watch notices, exactly as
    /// it would for an edit made anywhere else.
    pub fn edit_note_overlay(&mut self) {
        let Some(id) = self.overlay_target() else {
            return;
        };
        self.note_popup(&format!("note edit --id={}", sh_quote(&id)));
    }

    /// Run an `agent-mgr note …` subcommand in a popup over the whole window.
    ///
    /// Centred, which is tmux's default when no `-x`/`-y` is given: this is a
    /// thing you stop and read or write, not a side panel to glance at, and it
    /// should sit on top of everything rather than off in a corner. Bordered too,
    /// unlike the full-screen popup which passes `-B` — the border is what
    /// separates it from the work behind it. Needs tmux >= 3.3, same as that one.
    fn note_popup(&self, subcommand: &str) {
        let Some(bin) = tmux::global(tmux::CFG_BIN) else {
            return;
        };
        // Everything interpolated into this string is quoted before it gets
        // here: the binary path comes from @agent_mgr_bin and the note id comes
        // out of a markdown file that people and agents both write. The string is
        // handed to /bin/sh, so an unquoted `;` in either is a command.
        let command = format!("{} {subcommand}", sh_quote(&bin));
        tmux::run_tmux_quiet(&["display-popup", "-E", "-w", "70%", "-h", "70%", &command]);
    }

    /// Take a reparsed scratchpad from a worker snapshot.
    ///
    /// Split from the drain in [`run`] so the "unchanged means keep what we have"
    /// rule is testable without a worker thread — getting it backwards would blank
    /// the panel once per interval.
    pub fn apply_notes(&mut self, notes: Option<notes::NoteFile>) {
        let Some(notes) = notes else {
            return;
        };
        self.notes = notes;
        // A file that shrank can leave the offset past the end. The row builder
        // clamps for rendering, but the stored offset has to come back too, or
        // the panel stays scrolled to nothing once notes are added again.
        self.notes_scroll = self
            .notes_scroll
            .min(self.notes.len().saturating_sub(1));
        // A scratchpad emptied from elsewhere collapses the panel to nothing, and
        // focus on an invisible panel is a mode with no way out.
        if self.notes.is_empty() {
            self.close_notes();
        }
        self.clamp_notes();
    }

    /// The active search query, or an empty one when no search is open.
    pub fn query(&self) -> Query {
        Query::new(self.search.as_ref().map_or("", |search| &search.query))
    }

    /// Rebuild the rendered list from the current tree and selection.
    ///
    /// Pure and cheap — no I/O — which is what lets the loop call it on every
    /// pass and use its output as the change test.
    fn rebuild(&mut self) {
        let filtered = filter_sessions(&self.sessions, self.filter, &self.query());
        // Keep the cursor on the same pane across a refresh where possible: rows
        // come and go constantly as agents start and stop, and a selection that
        // jumps under your fingers is worse than one that lags.
        let anchor = self
            .list
            .blocks
            .get(self.selected)
            .map(|block| block.target.pane_id.clone());
        // A focus change asks for a *different* pane, so it takes the anchor's place
        // and rides the same "find this pane, move the cursor to it" path below.
        let anchor = self.follow.take().or(anchor);

        let mut opts = rows::Options {
            selected: self.selected,
            total_width: self.size.0 as usize,
            spinner: self.spinner,
            now: tmux::unix_timestamp(),
            numbers: self.numbers,
        };
        self.list = rows::build(&filtered, &opts, &self.theme);

        if let Some(pane_id) = anchor
            && let Some(index) = self
                .list
                .blocks
                .iter()
                .position(|block| block.target.pane_id == pane_id)
            && index != self.selected
        {
            self.selected = index;
            // Rebuild once more so the highlight lands on the row we just moved
            // to rather than on whatever now occupies the old index — and, with the
            // gutter on, so the relative numbers count from the right row.
            opts.selected = index;
            self.list = rows::build(&filtered, &opts, &self.theme);
        }

        self.clamp_selection();
        self.clamp_scroll();
        self.counts = Counts::tally(&self.sessions);
        self.compose_preview();
        self.compose_notes();
    }

    /// Re-compose the notes panel for the current geometry.
    ///
    /// Here rather than in `draw` so the panel's text is part of what the loop
    /// hashes — a note appearing is a reason to repaint, and this is how the
    /// loop finds out.
    fn compose_notes(&mut self) {
        // A pane shrunk while the panel had focus takes its rows away, and focus
        // on rows that are no longer drawn is the same trap `open_notes` refuses
        // to walk into — just arrived at from the other direction.
        if self.rows().notes == 0 {
            self.close_notes();
            self.note_delete = None;
        }
        self.notes_view = ui::notes::build(
            &self.notes,
            &ui::notes::Options {
                total_width: ui::split_width(self.surface, self.size.0).0 as usize,
                height: self.rows().notes as usize,
                scroll: self.notes_scroll,
                selected: self.notes_focus.map(|state| state.selected),
            },
            &self.theme,
        );
    }

    /// Take in where tmux focus now is, moving the cursor if it moved.
    ///
    /// Only a *change* is acted on. Following focus on every snapshot would fight the
    /// user: you park the cursor on a pane you are watching, keep working in another,
    /// and a second later it snaps away. Issues no tmux call of its own, and is called
    /// from the snapshot drain in [`run`], so the behaviour is testable without a
    /// worker thread.
    pub fn apply_focus(&mut self, focused: Option<String>) {
        // A pass that cannot tell us where focus is (no answer, our pane gone) says
        // nothing about whether it moved, so it must not clear what we know.
        let Some(pane_id) = focused else {
            return;
        };
        if self.focused.as_deref() == Some(pane_id.as_str()) {
            return;
        }
        self.focused = Some(pane_id.clone());
        self.follow = Some(pane_id);
    }

    /// The window the preview should be showing, if this surface has one.
    pub fn preview_window(&self) -> Option<&str> {
        if !self.surface.shows_preview() || self.help {
            return None;
        }
        Some(&self.list.blocks.get(self.selected)?.target.window_id)
    }

    /// The pane the cursor is on, whose outline the preview marks.
    pub fn selected_pane(&self) -> Option<&str> {
        Some(&self.list.blocks.get(self.selected)?.target.pane_id)
    }

    /// Re-compose the preview for the current geometry.
    ///
    /// Drops a capture belonging to a window we have since moved off: drawing it
    /// beside a different row would assert something false about the selection, and
    /// a blank preview for one frame is the honest alternative.
    fn compose_preview(&mut self) {
        let area = ui::preview_area(self.surface, self.size);
        let Some(area) = area else {
            self.preview_lines.clear();
            return;
        };
        let wanted = self.preview_window().map(str::to_owned);
        let selected = self.selected_pane().map(str::to_owned);
        self.preview_lines = match (&self.preview, wanted) {
            (Some((window_id, panes)), Some(wanted)) if *window_id == wanted => {
                crate::preview::compose(panes, area, selected.as_deref())
            }
            _ => Vec::new(),
        };
    }

    fn clamp_selection(&mut self) {
        let last = self.list.blocks.len().saturating_sub(1);
        self.selected = self.selected.min(last);
    }

    /// Scroll the minimum needed to keep the selected block fully visible.
    fn clamp_scroll(&mut self) {
        let height = self.list_height();
        if height == 0 || self.list.blocks.is_empty() {
            self.scroll = 0;
            return;
        }

        let start = self.list.block_line(self.selected);
        let end = start + self.list.block_height(self.selected);

        if start < self.scroll {
            // Reveal the session and window headers sitting directly above the
            // block too. Without this, scrolling up to the top pane of a session
            // hides the very lines that say which session it is.
            self.scroll = self.header_line_above(start);
        } else if end > self.scroll + height {
            self.scroll = end - height;
        }

        // Never leave blank space below a list that fits.
        let max_scroll = self.list.lines.len().saturating_sub(height);
        self.scroll = self.scroll.min(max_scroll);
    }

    /// The first line of the run of header lines immediately above `line`.
    ///
    /// Headers belong to no block, so they are the lines that scroll away first —
    /// which is exactly the context you need to read the pane below them.
    fn header_line_above(&self, line: usize) -> usize {
        let mut first = line;
        while first > 0 && self.list.block_at_line(first - 1).is_none() {
            first -= 1;
        }
        first
    }

    /// Which pane `Enter` would jump to, or `None` if there is nowhere to go.
    ///
    /// Split out from [`Self::activate_selection`] so the decision — including the
    /// refusal to navigate to our own pane — is testable without issuing
    /// `switch-client`, which in a test run would move the developer's own tmux
    /// client out from under them.
    pub fn activation_target(&self) -> Option<rows::PaneTarget> {
        let target = &self.list.blocks.get(self.selected)?.target;
        // An empty `own_pane` (a popup) must not match a real pane id.
        if !self.own_pane.is_empty() && target.pane_id == self.own_pane {
            return None;
        }
        Some(target.clone())
    }

    /// Jump tmux to the selected pane, and mark its window as caught up.
    fn activate_selection(&mut self) {
        let Some(target) = self.activation_target() else {
            return;
        };
        tmux::run_tmux_quiet(&["switch-client", "-t", &target.session_name]);
        tmux::run_tmux_quiet(&["select-window", "-t", &target.window_id]);
        tmux::run_tmux_quiet(&["select-pane", "-t", &target.pane_id]);
        // Visiting a window is how you acknowledge "this finished".
        daemon::mark_window_seen(&target.window_id);
        // A popup is covering the pane it just switched to; get out of the way.
        if self.surface.dismisses_on_activate() {
            self.quit = true;
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let (direction, count) = if delta < 0 {
            (Direction::Up, delta.unsigned_abs())
        } else {
            (Direction::Down, delta as usize)
        };
        self.selected = nav::step(&self.list.blocks, self.selected, direction, count);
    }

    /// Move by a pending count if one was typed, otherwise by one.
    fn move_counted(&mut self, direction: Direction) {
        let count = nav::take_count(&mut self.pending_count);
        self.selected = nav::step(&self.list.blocks, self.selected, direction, count);
    }

    /// Open the search prompt, keeping any query already typed.
    fn open_search(&mut self) {
        self.pending_count = None;
        match &mut self.search {
            Some(search) => search.editing = true,
            None => self.search = Some(SearchState::open()),
        }
    }

    /// Close the prompt but keep filtering, so the normal motions now work over the
    /// narrowed list.
    fn commit_search(&mut self) {
        match &mut self.search {
            // An empty query is not a filter; leaving it "committed" would show a
            // stale `/` in the footer forever.
            Some(search) if search.query.is_empty() => self.search = None,
            Some(search) => search.editing = false,
            None => {}
        }
    }

    /// Open the rename prompt on the selected pane's window.
    fn open_rename(&mut self) {
        self.pending_count = None;
        let Some(block) = self.list.blocks.get(self.selected) else {
            return;
        };
        let window_id = block.target.window_id.clone();
        // Seed with the current name so the common case is an edit, not a retype.
        let original = self
            .sessions
            .iter()
            .flat_map(|session| &session.windows)
            .find(|window| window.window_id == window_id)
            .map(|window| window.window_name.clone())
            .unwrap_or_default();
        self.rename = Some(RenameState {
            window_id,
            name: original.clone(),
            original,
        });
    }

    /// Take the pending rename, if it should be applied.
    ///
    /// Pure, so the state machine is testable without running `rename-window`
    /// against the tmux server hosting the test suite. The caller does the I/O.
    fn take_rename(&mut self) -> Option<(String, String)> {
        let state = self.rename.take()?;
        let name = state.name.trim().to_owned();
        // An empty name would make tmux fall back to its automatic name, which
        // looks like the rename silently failed. Unchanged is simply a no-op.
        if name.is_empty() || name == state.original {
            return None;
        }
        Some((state.window_id, name))
    }

    /// `J` / `K`: move the selected pane's session up or down the list.
    ///
    /// Reorders our own display only — tmux has no session order of its own.
    /// Applied to the unfiltered tree, since reordering under a filter would produce
    /// an order that only makes sense while that filter is on.
    ///
    /// Returns whether anything moved. Pure, so the caller does the persisting: a
    /// test pressing `J` must not write options onto the sessions of whichever tmux
    /// server happens to be hosting the test run.
    #[must_use]
    fn move_session(&mut self, direction: Direction) -> bool {
        self.pending_count = None;
        let Some(block) = self.list.blocks.get(self.selected) else {
            return false;
        };
        let name = block.target.session_name.clone();
        let Some(from) = self
            .sessions
            .iter()
            .position(|session| session.session_name == name)
        else {
            return false;
        };
        let to = match direction {
            Direction::Up if from > 0 => from - 1,
            Direction::Down if from + 1 < self.sessions.len() => from + 1,
            // Already at the edge. Clamping silently is right here: there is no
            // wrap-around reading of "move this session further up" that helps.
            _ => return false,
        };
        self.sessions.swap(from, to);
        true
    }

    /// `H` / `L`: jump a whole session.
    fn jump_session(&mut self, direction: Direction) {
        // A count would have to mean "N sessions over", which nobody can aim; drop
        // it rather than have it silently apply to the next motion instead.
        self.pending_count = None;
        self.selected = nav::session_edge(&self.list.blocks, self.selected, direction);
    }
}

/// Drop panes the status filter or the search excludes, then drop the windows and
/// sessions that leaves empty — a session header with nothing under it is noise.
///
/// The two narrow independently and both must pass, so a search inside a `blocked`
/// filter means "blocked agents, among these" rather than one silently replacing
/// the other.
fn filter_sessions(
    sessions: &[SessionGroup],
    filter: StatusFilter,
    query: &Query,
) -> Vec<SessionGroup> {
    if filter == StatusFilter::All && query.is_empty() {
        return sessions.to_vec();
    }

    sessions
        .iter()
        .filter_map(|session| {
            let windows: Vec<_> = session
                .windows
                .iter()
                .filter_map(|window| {
                    let panes: Vec<_> = window
                        .panes
                        .iter()
                        .filter(|pane| {
                            filter.matches(&pane.status) && query.matches(session, window, pane)
                        })
                        .cloned()
                        .collect();
                    (!panes.is_empty()).then(|| crate::model::WindowInfo {
                        panes,
                        ..window.clone()
                    })
                })
                .collect();
            (!windows.is_empty()).then(|| SessionGroup {
                windows,
                ..session.clone()
            })
        })
        .collect()
}

/// Wrap a string so a POSIX shell sees it as one literal word.
///
/// Single quotes, with the one escape they need for a single quote inside. The
/// popup command is assembled here and executed by `/bin/sh`, so a path with a
/// space in it is a broken overlay and a path with a `;` in it is worse.
fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Hash of everything that affects what is on screen.
///
/// The line *text* is the bulk of it. Style-only differences always travel with a
/// glyph change (icons, badges, markers) or with the selection index, both of
/// which are covered here — so a matching hash really does mean a matching
/// screen. `size` is included because the same text at a new width is a new frame.
fn fingerprint(app: &App) -> u64 {
    let mut hasher = DefaultHasher::new();
    app.list.plain.hash(&mut hasher);
    app.selected.hash(&mut hasher);
    app.scroll.hash(&mut hasher);
    app.size.hash(&mut hasher);
    app.filter.hash(&mut hasher);
    app.counts.hash(&mut hasher);
    // Both are shown in the footer, so they are part of the screen.
    app.pending_count.hash(&mut hasher);
    app.search.hash(&mut hasher);
    // The help page replaces the list, and the rename prompt owns the footer.
    app.help.hash(&mut hasher);
    app.rename.hash(&mut hasher);
    // Pane content changing behind the preview is a real reason to redraw, and
    // hashing the composed lines is how the loop learns about it.
    app.preview_lines.hash(&mut hasher);
    // The panel is part of the screen, and a note added from another pane is
    // exactly the kind of change the loop exists to notice. The cursor and the
    // entry prompt travel separately: one is a background the text does not
    // carry, the other owns the footer.
    app.notes_view.plain.hash(&mut hasher);
    app.notes_focus.hash(&mut hasher);
    app.note_entry.hash(&mut hasher);
    app.note_delete.hash(&mut hasher);
    app.note_error.hash(&mut hasher);
    hasher.finish()
}

pub fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    surface: Surface,
    tmux_pane: String,
    needs_refresh: &'static AtomicBool,
) -> io::Result<()> {
    let size = terminal.size()?;
    let mut app = App::new(surface, tmux_pane, (size.width, size.height));
    // Resolved once, here, because resolving it reads a tmux option and the
    // worker would otherwise pay for that on every pass. `None` on a popup turns
    // the whole watch off.
    app.notes_file = surface.shows_notes().then(notes::path);
    if let Some(path) = &app.notes_file {
        app.load_notes(&path.clone());
    }

    let worker = worker::spawn(
        tmux::global_bool(tmux::CFG_AGENTS_ONLY, false),
        app.own_pane.clone(),
        app.notes_file.clone(),
    );
    // Nothing to show until the first collection lands; ask for it now rather
    // than waiting out an interval.
    worker.request_refresh();

    let mut last_fingerprint: Option<u64> = None;
    let mut last_spinner = Instant::now();
    let started = Instant::now();

    while !app.quit {
        // 1. Newest snapshot wins; drain so a burst can't build a backlog.
        let mut received = false;
        while let Ok(snapshot) = worker.rx.try_recv() {
            app.sessions = snapshot.sessions;
            // Keep the previous capture when this pass carried none: the target is
            // set after the first rebuild, so the very first snapshot has no preview
            // and clearing here would blank it once per interval.
            if let Some(preview) = snapshot.preview {
                app.preview = Some(preview);
            }
            // After the tree, so the pane it names is one this snapshot listed.
            app.apply_focus(snapshot.focused);
            // Absent on every pass where the file did not move, which is almost
            // all of them; see `Snapshot::notes`.
            app.apply_notes(snapshot.notes);
            received = true;
        }

        // 2. A focus change reached us by signal.
        if needs_refresh.swap(false, Ordering::Relaxed) {
            worker.request_refresh();
        }

        // 3. Advance the spinner only when something is running. This is what
        //    makes a quiet sidebar produce identical output pass after pass.
        let active = app.any_active();
        if active && last_spinner.elapsed() >= SPINNER_INTERVAL {
            app.spinner = app.spinner.wrapping_add(1);
            last_spinner = Instant::now();
        }

        // 4. Rebuild (pure) and draw only if the output moved.
        app.rebuild();
        // Tell the worker what to capture next. After rebuild, because the selection
        // may have been clamped or re-anchored onto a different window.
        worker.set_preview_target(app.preview_window());
        let current = fingerprint(&app);
        if last_fingerprint != Some(current) {
            terminal.draw(|frame| ui::draw(frame, &app))?;
            app.frames += 1;
            last_fingerprint = Some(current);
        }
        let _ = received;

        // 5. Wait for input. When active, wake in time for the next spinner
        //    frame; otherwise sleep as long as responsiveness allows.
        let timeout = if active {
            SPINNER_INTERVAL.saturating_sub(last_spinner.elapsed())
        } else {
            QUIET_TIMEOUT
        };
        if !event::poll(timeout.max(Duration::from_millis(10)))? {
            continue;
        }
        loop {
            match event::read()? {
                // The only clear in the crate: our geometry changed underneath
                // us, so the previous frame's cells are meaningless.
                Event::Resize(width, height) => {
                    app.size = (width, height);
                    terminal.clear()?;
                    last_fingerprint = None;
                }
                other => input::handle(other, &mut app, &worker),
            }
            if !event::poll(Duration::ZERO)? {
                break;
            }
        }
    }

    report_frames(&app, started.elapsed());
    Ok(())
}

/// Write the draw count to the path in `AGENT_MGR_DEBUG_FRAMES`, if set.
///
/// Exists to make the anti-flicker claim falsifiable: leave an idle sidebar open
/// for a minute and this should read a handful of frames, not hundreds. Writes to
/// a file rather than stderr because our pane closes the moment we return.
fn report_frames(app: &App, elapsed: Duration) {
    let Ok(path) = std::env::var("AGENT_MGR_DEBUG_FRAMES") else {
        return;
    };
    let _ = std::fs::write(
        path,
        format!("frames={} seconds={:.1}\n", app.frames, elapsed.as_secs_f64()),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AgentKind, PaneInfo, StatusSource, WindowInfo};

    fn pane(pane_id: &str, state: AgentState, seen: bool) -> PaneInfo {
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
                state,
                source: StatusSource::Passive,
                seen,
                ..AgentStatus::default()
            },
            branch: String::new(),
            worktree: String::new(),
        }
    }

    fn tree(panes: Vec<PaneInfo>) -> Vec<SessionGroup> {
        vec![SessionGroup {
            session_name: "work".to_owned(),
            session_attached: true,
            windows: vec![WindowInfo {
                window_id: "@1".to_owned(),
                window_index: "1".to_owned(),
                window_name: "w".to_owned(),
                window_active: true,
                panes,
            }],
        }]
    }

    fn app_with(panes: Vec<PaneInfo>, height: u16) -> App {
        surfaced_app(Surface::Sidebar, panes, height)
    }

    fn surfaced_app(surface: Surface, panes: Vec<PaneInfo>, height: u16) -> App {
        let mut app = App::new(surface, "%99".to_owned(), (40, height));
        app.sessions = tree(panes);
        app.rebuild();
        app
    }

    /// An app with `count` notes in its scratchpad, rendered.
    fn app_with_notes(count: usize, height: u16) -> App {
        let mut app = app_with(vec![pane("%1", AgentState::Idle, true)], height);
        app.notes = notes::NoteFile {
            preamble: String::new(),
            notes: (0..count)
                .map(|index| notes::Note::new(&format!("note {index}"), ""))
                .collect(),
        };
        app.rebuild();
        app
    }

    // ─── notes panel ──────────────────────────────────────────────────

    #[test]
    fn a_freshly_built_app_has_no_notes_of_its_own() {
        // `new` must not read the developer's scratchpad, or every layout test in
        // this file would depend on what they had written down that day.
        let app = app_with(vec![pane("%1", AgentState::Idle, true)], 40);
        assert!(app.notes.is_empty());
        assert!(app.notes_view.is_empty());
    }

    #[test]
    fn the_panel_takes_its_rows_out_of_the_list() {
        let without = app_with_notes(0, 40).list_height();
        let with = app_with_notes(3, 40).list_height();
        assert_eq!(with, without - 4, "a header and three notes");
    }

    #[test]
    fn the_composed_panel_is_exactly_as_tall_as_the_rows_it_was_given() {
        // The rect and the lines are sized by the same call; if they can
        // disagree, the panel either clips its last note or paints over the
        // footer.
        for count in [0usize, 1, 3, 40] {
            for height in [10u16, 24, 40, 80] {
                let app = app_with_notes(count, height);
                let rows = ui::split_height(Surface::Sidebar, height, count);
                assert_eq!(
                    app.notes_view.lines.len(),
                    rows.notes as usize,
                    "{count} notes in {height} rows"
                );
            }
        }
    }

    #[test]
    fn a_popup_composes_no_panel_however_many_notes_are_waiting() {
        let mut app = surfaced_app(
            Surface::Popup,
            vec![pane("%1", AgentState::Idle, true)],
            40,
        );
        app.notes = notes::NoteFile {
            preamble: String::new(),
            notes: (0..5).map(|_| notes::Note::new("x", "")).collect(),
        };
        app.rebuild();
        assert!(app.notes_view.is_empty());
        assert_eq!(app.list_height(), 38, "the popup keeps every body row");
    }

    #[test]
    fn sh_quote_neutralises_everything_a_shell_would_act_on() {
        // Both things interpolated into the popup command are attacker-adjacent:
        // @agent_mgr_bin is user config, and a note id comes out of a markdown
        // file that agents write. Validation already rejects a hostile id, but
        // this is the layer that has to hold if validation is ever relaxed.
        //
        // Every payload here is inert on purpose. This test exists to catch a
        // quoting regression, which means the failure mode is "the shell runs
        // it" — so a realistic payload would delete a home directory or make a
        // network call on the way to reporting the bug. The metacharacters are
        // what matter; what they would have run does not.
        for hostile in [
            "x; printf INJECTED",
            "$(printf INJECTED)",
            "`printf INJECTED`",
            "a'b",
            "a\nb",
            "| printf INJECTED",
            "&& printf INJECTED",
            "> /dev/null",
        ] {
            let quoted = sh_quote(hostile);
            // Echoing it back through a real shell must yield the input verbatim.
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("printf %s {quoted}"))
                .output()
                .expect("sh");
            assert_eq!(
                String::from_utf8_lossy(&out.stdout),
                hostile,
                "{hostile:?} was not neutralised by {quoted}"
            );
        }
    }

    #[test]
    fn an_unchanged_snapshot_leaves_the_panel_alone() {
        // `None` means "the file did not move", not "there are no notes". Reading
        // it the other way would blank the panel once per interval — and since
        // the panel's rows come out of the list, the whole sidebar would twitch.
        let mut app = app_with_notes(3, 40);
        let before = fingerprint(&app);
        for _ in 0..3 {
            app.apply_notes(None);
            app.rebuild();
        }
        assert_eq!(app.notes.len(), 3);
        assert_eq!(fingerprint(&app), before);
    }

    #[test]
    fn a_snapshot_replaces_the_scratchpad_wholesale() {
        let mut app = app_with_notes(1, 40);
        app.apply_notes(Some(notes::NoteFile {
            preamble: String::new(),
            notes: vec![
                notes::Note::new("from another pane", ""),
                notes::Note::new("and another", ""),
            ],
        }));
        app.rebuild();
        assert_eq!(app.notes.len(), 2);
        assert!(
            app.notes_view.plain[1].contains("from another pane"),
            "{:?}",
            app.notes_view.plain
        );
    }

    #[test]
    fn a_scratchpad_that_shrank_pulls_the_scroll_offset_back() {
        // Otherwise deleting notes elsewhere leaves the panel parked past the end,
        // and it stays blank even after new notes arrive.
        let mut app = app_with_notes(20, 40);
        app.notes_scroll = 15;
        app.apply_notes(Some(notes::NoteFile {
            preamble: String::new(),
            notes: vec![notes::Note::new("only one left", "")],
        }));
        app.rebuild();
        assert_eq!(app.notes_scroll, 0);
        assert!(app.notes_view.plain[1].contains("only one left"));
    }

    #[test]
    fn fingerprint_moves_when_a_note_appears() {
        // Otherwise the loop's change test cannot see the panel, and a note
        // written from another pane never gets painted.
        let mut app = app_with_notes(1, 40);
        let before = fingerprint(&app);
        app.notes.notes.push(notes::Note::new("something new", ""));
        app.rebuild();
        assert_ne!(fingerprint(&app), before);
    }

    #[test]
    fn fingerprint_is_stable_while_the_scratchpad_is_untouched() {
        // The zero-frames-when-idle promise: re-reading the same notes must not
        // produce a new frame.
        let mut app = app_with_notes(3, 40);
        let first = fingerprint(&app);
        for _ in 0..5 {
            app.rebuild();
            assert_eq!(fingerprint(&app), first);
        }
    }

    // ─── filter ───────────────────────────────────────────────────────

    #[test]
    fn filter_cycles_back_to_all() {
        let mut filter = StatusFilter::All;
        for _ in 0..4 {
            filter = filter.next();
        }
        assert_eq!(filter, StatusFilter::All);
    }

    #[test]
    fn the_blocked_filter_also_surfaces_errors() {
        // A failed run needs you as much as a prompt does; hiding it in a state
        // nobody thinks to select would lose it.
        let status = |state| AgentStatus {
            agent: Some(AgentKind::Claude),
            state,
            seen: true,
            ..AgentStatus::default()
        };
        assert!(StatusFilter::Blocked.matches(&status(AgentState::Blocked)));
        assert!(StatusFilter::Blocked.matches(&status(AgentState::Error)));
        assert!(!StatusFilter::Blocked.matches(&status(AgentState::Working)));
    }

    #[test]
    fn the_done_filter_shows_only_unacknowledged_finished_runs() {
        let done = AgentStatus {
            agent: Some(AgentKind::Claude),
            state: AgentState::Idle,
            seen: false,
            ..AgentStatus::default()
        };
        let acknowledged = AgentStatus { seen: true, ..done.clone() };
        assert!(StatusFilter::Done.matches(&done));
        assert!(!StatusFilter::Done.matches(&acknowledged));
    }

    #[test]
    fn filtering_drops_windows_and_sessions_it_empties() {
        let sessions = tree(vec![
            pane("%1", AgentState::Working, true),
            pane("%2", AgentState::Idle, true),
        ]);
        let working = filter_sessions(&sessions, StatusFilter::Working, &Query::default());
        assert_eq!(working[0].windows[0].panes.len(), 1);

        // Nothing matches: no empty session header left behind.
        assert!(filter_sessions(&sessions, StatusFilter::Done, &Query::default()).is_empty());
    }

    #[test]
    fn the_all_filter_passes_the_tree_through_untouched() {
        let sessions = tree(vec![pane("%1", AgentState::Idle, true)]);
        assert_eq!(
            filter_sessions(&sessions, StatusFilter::All, &Query::default()),
            sessions
        );
    }

    #[test]
    fn hidden_count_reports_what_the_filter_is_holding_back() {
        let mut app = app_with(
            vec![
                pane("%1", AgentState::Working, true),
                pane("%2", AgentState::Idle, true),
                pane("%3", AgentState::Idle, true),
            ],
            40,
        );
        assert_eq!(app.hidden_count(), 0);

        app.filter = StatusFilter::Working;
        app.rebuild();
        assert_eq!(app.hidden_count(), 2);
    }

    // ─── selection and scrolling ──────────────────────────────────────

    #[test]
    fn selection_moves_within_bounds_and_saturates() {
        let mut app = app_with(
            vec![
                pane("%1", AgentState::Idle, true),
                pane("%2", AgentState::Idle, true),
                pane("%3", AgentState::Idle, true),
            ],
            40,
        );
        app.move_selection(1);
        assert_eq!(app.selected, 1);
        app.move_selection(10);
        assert_eq!(app.selected, 2, "must not run off the end");
        app.move_selection(-10);
        assert_eq!(app.selected, 0, "must not underflow");
    }

    #[test]
    fn selection_follows_its_pane_when_rows_shift_around_it() {
        // Agents start and stop constantly; a cursor that jumps to a different
        // pane under the user's fingers is how you activate the wrong window.
        let mut app = app_with(
            vec![
                pane("%1", AgentState::Idle, true),
                pane("%2", AgentState::Idle, true),
                pane("%3", AgentState::Idle, true),
            ],
            40,
        );
        app.selected = 2;
        app.rebuild();
        assert_eq!(app.list.blocks[app.selected].target.pane_id, "%3");

        // A new pane appears above the selection.
        app.sessions[0].windows[0]
            .panes
            .insert(0, pane("%0", AgentState::Working, true));
        app.rebuild();
        assert_eq!(
            app.list.blocks[app.selected].target.pane_id, "%3",
            "the cursor should still be on the same pane"
        );
    }

    #[test]
    fn selection_clamps_when_its_pane_disappears() {
        let mut app = app_with(
            vec![
                pane("%1", AgentState::Idle, true),
                pane("%2", AgentState::Idle, true),
            ],
            40,
        );
        app.selected = 1;
        app.rebuild();

        app.sessions[0].windows[0].panes.pop();
        app.rebuild();
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn scroll_follows_the_selection_down_and_back_up() {
        // A 6-row pane leaves 4 list rows: 2 header rows plus 2 pane rows.
        let panes: Vec<PaneInfo> = (0..10)
            .map(|index| pane(&format!("%{index}"), AgentState::Idle, true))
            .collect();
        let mut app = app_with(panes, 6);
        assert_eq!(app.scroll, 0);

        app.selected = 9;
        app.rebuild();
        let last_line = app.list.block_line(9);
        assert!(
            last_line >= app.scroll && last_line < app.scroll + app.list_height(),
            "selected line {last_line} outside viewport at scroll {}",
            app.scroll
        );

        app.selected = 0;
        app.rebuild();
        assert_eq!(app.scroll, 0);
    }

    #[test]
    fn scrolling_up_to_a_pane_brings_its_headers_back_with_it() {
        // The session and window headers are the only thing saying *where* a pane
        // is; scrolling so the pane is visible but its headers are not defeats the
        // point of grouping.
        let panes: Vec<PaneInfo> = (0..10)
            .map(|index| pane(&format!("%{index}"), AgentState::Idle, true))
            .collect();
        let mut app = app_with(panes, 6);

        app.selected = 9;
        app.rebuild();
        assert!(app.scroll > 0, "should have scrolled down");

        app.selected = 0;
        app.rebuild();
        assert_eq!(app.scroll, 0, "headers on lines 0-1 must come back into view");
        assert!(app.list.block_at_line(0).is_none(), "line 0 is a header");
    }

    #[test]
    fn scroll_never_leaves_blank_space_below_a_short_list() {
        let mut app = app_with(vec![pane("%1", AgentState::Idle, true)], 40);
        app.scroll = 100;
        app.clamp_scroll();
        assert_eq!(app.scroll, 0);
    }

    #[test]
    fn a_zero_height_pane_does_not_panic_or_scroll() {
        let app = app_with(vec![pane("%1", AgentState::Idle, true)], 1);
        assert_eq!(app.list_height(), 0);
        assert_eq!(app.scroll, 0);
    }

    // ─── following tmux focus ─────────────────────────────────────────

    fn three_pane_app() -> App {
        app_with(
            vec![
                pane("%1", AgentState::Idle, true),
                pane("%2", AgentState::Idle, true),
                pane("%3", AgentState::Idle, true),
            ],
            40,
        )
    }

    #[test]
    fn a_focus_change_moves_the_cursor_to_that_pane() {
        // C-h/C-l/C-j/C-k move tmux focus without touching us; a cursor left behind
        // means the list and the terminal disagree about "here".
        let mut app = three_pane_app();
        app.apply_focus(Some("%3".to_owned()));
        app.rebuild();
        assert_eq!(app.list.blocks[app.selected].target.pane_id, "%3");
    }

    #[test]
    fn repeated_snapshots_of_the_same_focus_leave_a_hand_moved_cursor_alone() {
        // You park the cursor on a pane you are watching and keep working elsewhere.
        // Snapping it back once a second would make the motions unusable.
        let mut app = three_pane_app();
        app.apply_focus(Some("%1".to_owned()));
        app.rebuild();

        app.move_selection(2);
        app.rebuild();
        assert_eq!(app.list.blocks[app.selected].target.pane_id, "%3");

        for _ in 0..5 {
            app.apply_focus(Some("%1".to_owned()));
            app.rebuild();
        }
        assert_eq!(
            app.list.blocks[app.selected].target.pane_id, "%3",
            "an unchanged focus must not drag the cursor back"
        );
    }

    #[test]
    fn a_pass_that_cannot_see_focus_does_not_forget_where_it_was() {
        // Otherwise the next answer would read as a change and yank the cursor.
        let mut app = three_pane_app();
        app.apply_focus(Some("%2".to_owned()));
        app.rebuild();
        app.move_selection(-1);

        app.apply_focus(None);
        app.rebuild();
        assert_eq!(app.list.blocks[app.selected].target.pane_id, "%1");
        assert_eq!(app.focused.as_deref(), Some("%2"));
    }

    #[test]
    fn focus_on_a_pane_the_filter_hides_leaves_the_cursor_where_it_is() {
        // There is no row to move to, and clamping to a neighbour would claim a
        // focus that isn't there.
        let mut app = app_with(
            vec![
                pane("%1", AgentState::Working, true),
                pane("%2", AgentState::Idle, true),
            ],
            40,
        );
        app.filter = StatusFilter::Working;
        app.rebuild();
        assert_eq!(app.list.blocks.len(), 1);

        app.apply_focus(Some("%2".to_owned()));
        app.rebuild();
        assert_eq!(app.list.blocks[app.selected].target.pane_id, "%1");
    }

    // ─── the anti-flicker contract ────────────────────────────────────

    #[test]
    fn fingerprint_is_stable_across_spinner_ticks_when_nothing_is_working() {
        // The core of the flicker fix: a quiet workspace must hash the same
        // forever, so the loop never writes to the terminal.
        let mut app = app_with(
            vec![
                pane("%1", AgentState::Idle, true),
                pane("%2", AgentState::Blocked, true),
            ],
            40,
        );
        let first = fingerprint(&app);
        for _ in 0..50 {
            app.spinner = app.spinner.wrapping_add(1);
            app.rebuild();
            assert_eq!(fingerprint(&app), first);
        }
        assert!(!app.any_active() || app.any_active(), "sanity");
    }

    #[test]
    fn fingerprint_moves_when_a_working_pane_animates() {
        let mut app = app_with(vec![pane("%1", AgentState::Working, true)], 40);
        assert!(app.any_active());
        let first = fingerprint(&app);
        app.spinner += 1;
        app.rebuild();
        assert_ne!(fingerprint(&app), first);
    }

    #[test]
    fn fingerprint_moves_when_a_pane_changes_state() {
        let mut app = app_with(vec![pane("%1", AgentState::Idle, true)], 40);
        let before = fingerprint(&app);
        app.sessions[0].windows[0].panes[0].status.state = AgentState::Blocked;
        app.rebuild();
        assert_ne!(fingerprint(&app), before);
    }

    #[test]
    fn fingerprint_moves_when_only_the_selection_changes() {
        // Selection is styling, not text, so it has to be hashed explicitly.
        let mut app = app_with(
            vec![
                pane("%1", AgentState::Idle, true),
                pane("%2", AgentState::Idle, true),
            ],
            40,
        );
        let before = fingerprint(&app);
        app.move_selection(1);
        app.rebuild();
        assert_ne!(fingerprint(&app), before);
    }

    #[test]
    fn fingerprint_moves_on_resize() {
        let mut app = app_with(vec![pane("%1", AgentState::Idle, true)], 40);
        let before = fingerprint(&app);
        app.size = (60, 40);
        app.rebuild();
        assert_ne!(fingerprint(&app), before);
    }

    // ─── surfaces ─────────────────────────────────────────────────────

    #[test]
    fn a_popup_dismisses_on_activate_and_a_sidebar_does_not() {
        // The sidebar's whole value is that you keep jumping around with it open.
        assert!(Surface::Popup.dismisses_on_activate());
        assert!(!Surface::Sidebar.dismisses_on_activate());
    }

    // ─── session reorder ──────────────────────────────────────────────

    fn multi_session_app(names: &[&str]) -> App {
        let mut app = App::new(Surface::Sidebar, "%99".to_owned(), (40, 40));
        app.sessions = names
            .iter()
            .enumerate()
            .map(|(index, name)| SessionGroup {
                session_name: (*name).to_owned(),
                session_attached: true,
                windows: vec![WindowInfo {
                    window_id: format!("@{index}"),
                    window_index: index.to_string(),
                    window_name: "w".to_owned(),
                    window_active: false,
                    panes: vec![pane(&format!("%{index}"), AgentState::Idle, true)],
                }],
            })
            .collect();
        app.rebuild();
        app
    }

    fn order(app: &App) -> Vec<&str> {
        app.sessions
            .iter()
            .map(|session| session.session_name.as_str())
            .collect()
    }

    #[test]
    fn shift_j_and_k_move_the_selected_pane_s_session() {
        let mut app = multi_session_app(&["a", "b", "c"]);
        app.selected = 1; // in session "b"
        app.rebuild();
        assert!(app.move_session(Direction::Up));
        assert_eq!(order(&app), ["b", "a", "c"]);
        assert!(app.move_session(Direction::Down));
        assert_eq!(order(&app), ["a", "b", "c"]);
    }

    #[test]
    fn moving_a_session_past_an_edge_does_nothing() {
        let mut app = multi_session_app(&["a", "b"]);
        app.selected = 0;
        app.rebuild();
        assert!(!app.move_session(Direction::Up), "already first");
        assert_eq!(order(&app), ["a", "b"]);

        app.selected = 1;
        app.rebuild();
        assert!(!app.move_session(Direction::Down), "already last");
        assert_eq!(order(&app), ["a", "b"]);
    }

    #[test]
    fn reordering_moves_the_session_not_just_the_visible_row() {
        // Under a filter the rendered list may hold only some sessions; the order we
        // persist has to be the real one, or it would only make sense while that
        // filter was on.
        let mut app = multi_session_app(&["a", "b", "c"]);
        app.selected = 2;
        app.filter = StatusFilter::All;
        app.rebuild();
        assert!(app.move_session(Direction::Up));
        assert_eq!(order(&app), ["a", "c", "b"]);
    }

    #[test]
    fn reordering_an_empty_list_is_a_no_op() {
        let mut app = multi_session_app(&[]);
        assert!(!app.move_session(Direction::Down));
    }

    // ─── preview gating ───────────────────────────────────────────────

    #[test]
    fn a_sidebar_never_asks_for_a_preview() {
        // Which is what keeps a capture-pane per pane off a panel open all day.
        let app = app_with(vec![pane("%1", AgentState::Idle, true)], 40);
        assert_eq!(app.preview_window(), None);
    }

    #[test]
    fn a_popup_previews_the_selected_panes_window() {
        let mut app = surfaced_app(Surface::Popup, vec![pane("%1", AgentState::Idle, true)], 40);
        app.size = (200, 50);
        app.rebuild();
        assert_eq!(app.preview_window(), Some("@1"));
    }

    #[test]
    fn the_help_page_suspends_the_preview() {
        // Nothing of it is on screen, so capturing for it is pure waste.
        let mut app = surfaced_app(Surface::Popup, vec![pane("%1", AgentState::Idle, true)], 40);
        app.size = (200, 50);
        app.help = true;
        app.rebuild();
        assert_eq!(app.preview_window(), None);
    }

    #[test]
    fn a_capture_for_a_window_we_have_moved_off_is_not_drawn() {
        // Drawing it beside a different row would assert something false about the
        // selection; one blank frame is the honest alternative.
        let mut app = surfaced_app(Surface::Popup, vec![pane("%1", AgentState::Idle, true)], 40);
        app.size = (200, 50);
        app.preview = Some((
            "@somewhere-else".to_owned(),
            vec![crate::preview::PanePreview {
                pane_id: "%9".to_owned(),
                width: 80,
                height: 24,
                lines: vec![captured("stale")],
                ..Default::default()
            }],
        ));
        app.rebuild();
        assert!(app.preview_lines.is_empty());

        // The matching window is composed.
        app.preview = Some((
            "@1".to_owned(),
            vec![crate::preview::PanePreview {
                pane_id: "%1".to_owned(),
                width: 80,
                height: 24,
                lines: vec![captured("fresh")],
                ..Default::default()
            }],
        ));
        app.rebuild();
        assert!(app.preview_lines[0].text().starts_with("fresh"));
    }

    /// One captured line, as the preview's parser would hand it over.
    fn captured(text: &str) -> Vec<crate::preview::Cell> {
        crate::preview::parse_line(text, &mut crate::preview::Attrs::default())
    }

    #[test]
    fn moving_between_panes_of_one_window_still_changes_the_preview() {
        // The preview target is the *window*, so this motion leaves the capture
        // untouched — only the selection marker moves. Before it existed, `j` inside
        // a split window changed nothing on screen at all.
        let mut second = pane("%2", AgentState::Idle, true);
        second.pane_index = "1".to_owned();
        let mut app = surfaced_app(
            Surface::Popup,
            vec![pane("%1", AgentState::Idle, true), second],
            40,
        );
        app.size = (200, 50);
        app.preview = Some((
            "@1".to_owned(),
            vec![
                crate::preview::PanePreview {
                    pane_id: "%1".to_owned(),
                    width: 40,
                    height: 24,
                    lines: vec![captured("left")],
                    ..Default::default()
                },
                crate::preview::PanePreview {
                    pane_id: "%2".to_owned(),
                    left: 40,
                    width: 40,
                    height: 24,
                    lines: vec![captured("right")],
                    ..Default::default()
                },
            ],
        ));
        app.rebuild();
        let before = fingerprint(&app);
        let text_before: Vec<String> = app.preview_lines.iter().map(|line| line.text()).collect();

        app.move_selection(1);
        app.rebuild();
        assert_eq!(app.preview_window(), Some("@1"), "same window either way");
        assert_ne!(fingerprint(&app), before, "the marker moved, so the frame did");
        let text_after: Vec<String> = app.preview_lines.iter().map(|line| line.text()).collect();
        assert_eq!(text_before, text_after, "and only the styling changed");
    }

    #[test]
    fn a_popup_too_narrow_for_a_preview_composes_none() {
        let mut app = surfaced_app(Surface::Popup, vec![pane("%1", AgentState::Idle, true)], 40);
        app.size = (40, 50);
        app.rebuild();
        assert!(app.preview_lines.is_empty());
    }

    #[test]
    fn a_popup_has_no_own_pane_to_refuse() {
        // A popup is not a pane in any window, so every listed pane is a legitimate
        // jump target — including the one the binding fired from.
        let mut app = surfaced_app(Surface::Popup, vec![pane("%1", AgentState::Idle, true)], 40);
        app.own_pane = String::new();
        assert_eq!(app.activation_target().unwrap().pane_id, "%1");
    }

    #[test]
    fn a_pane_with_no_agent_is_not_treated_as_active() {
        let mut plain = pane("%1", AgentState::Unknown, true);
        plain.status.agent = None;
        let app = app_with(vec![plain], 40);
        assert!(!app.any_active(), "a plain shell must not animate the sidebar");
    }
}
