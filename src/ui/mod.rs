//! Drawing the sidebar: a header, the scrollable list, and a footer.

pub mod help;
pub mod rows;
pub mod text;
pub mod theme;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{App, StatusFilter};
use crate::model::AgentState;
use text::{pad_to, truncate, width};

/// Where the TUI is being drawn.
///
/// The same list code serves both, but the two surfaces have genuinely different
/// jobs and the differences are worth naming rather than sprinkling `if` on a
/// bool. A sidebar is narrow, lives for hours beside your work, and is the reason
/// the redraw policy in [`crate::app`] exists. A popup is wide, covers the screen,
/// and exists to answer one question and get out of the way.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum Surface {
    #[default]
    Sidebar,
    Popup,
}

impl Surface {
    /// Whether jumping to a pane should also close the TUI.
    ///
    /// A popup is a transient chooser: leaving it open on top of the pane you just
    /// asked to see would hide the thing you navigated to. A sidebar is the
    /// opposite — you jump around *with* it open, which is the whole point of it
    /// being persistent.
    pub fn dismisses_on_activate(self) -> bool {
        self == Self::Popup
    }

    /// Whether this surface can afford a live window preview.
    ///
    /// Preview costs a `capture-pane` subprocess per pane per refresh, which is
    /// fine for the seconds a popup is up and not fine for a pane open all day. It
    /// also needs columns a 24-wide sidebar does not have.
    pub fn shows_preview(self) -> bool {
        self == Self::Popup
    }
}

/// Columns given to the list when a preview shares the width.
///
/// The list is the thing you are navigating, so it gets a stable, generous column
/// rather than a proportion — a list that reflows as the preview changes shape is
/// much harder to aim a counted motion at. The preview takes the rest.
const LIST_WIDTH: u16 = 44;
/// Below this the preview is too narrow to recognise anything in, so the list keeps
/// the whole width instead. A 12-column preview is decoration, not information.
const MIN_PREVIEW_WIDTH: u16 = 24;

/// Split a popup's width into (list, preview). `None` when there is no room.
pub fn split_width(surface: Surface, total_width: u16) -> (u16, Option<u16>) {
    if !surface.shows_preview() || total_width < LIST_WIDTH + MIN_PREVIEW_WIDTH {
        return (total_width, None);
    }
    (LIST_WIDTH, Some(total_width - LIST_WIDTH))
}

/// The rectangle the preview occupies, in preview-local coordinates.
///
/// Returns `None` when this surface or this geometry has no preview, which is what
/// [`crate::app::App::compose_preview`] uses to skip the work entirely.
pub fn preview_area(surface: Surface, size: (u16, u16)) -> Option<crate::preview::Rect> {
    let (_, preview_width) = split_width(surface, size.0);
    let width = preview_width? as usize;
    let height = list_height(size.1) as usize;
    (width > 0 && height > 0).then_some(crate::preview::Rect {
        x: 0,
        y: 0,
        width,
        height,
    })
}

/// Lines reserved at the top and bottom of the pane. Both are fixed so the list
/// viewport height is stable and scrolling doesn't jump when a count changes.
pub const HEADER_HEIGHT: u16 = 1;
pub const FOOTER_HEIGHT: u16 = 1;

/// Visible list height for a pane of `total_height` rows.
pub fn list_height(total_height: u16) -> u16 {
    total_height.saturating_sub(HEADER_HEIGHT + FOOTER_HEIGHT)
}

pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    if area.height == 0 || area.width == 0 {
        return;
    }

    let header = Rect { height: HEADER_HEIGHT.min(area.height), ..area };
    frame.render_widget(Paragraph::new(header_line(app, area.width as usize)), header);

    let (list_width, preview_width) = split_width(app.surface, area.width);
    let list_rows = list_height(area.height);

    if let Some(preview_width) = preview_width
        && list_rows > 0
    {
        let pane = Rect {
            x: area.x + list_width,
            y: area.y + HEADER_HEIGHT,
            width: preview_width,
            height: list_rows,
        };
        frame.render_widget(Paragraph::new(preview_lines(app, preview_width)), pane);
    }

    if list_rows > 0 {
        let list = Rect {
            y: area.y + HEADER_HEIGHT,
            width: list_width,
            height: list_rows,
            ..area
        };
        if app.help {
            let page = help::lines(list_width as usize, list_rows as usize, &app.theme);
            frame.render_widget(Paragraph::new(page), list);
        } else if app.list.is_empty() {
            frame.render_widget(Paragraph::new(empty_state(app, list_width as usize)), list);
        } else {
            let visible: Vec<Line<'static>> = app
                .list
                .lines
                .iter()
                .skip(app.scroll)
                .take(list_rows as usize)
                .cloned()
                .collect();
            frame.render_widget(Paragraph::new(visible), list);
        }
    }

    if area.height > HEADER_HEIGHT {
        let footer = Rect {
            y: area.y + area.height - FOOTER_HEIGHT,
            height: FOOTER_HEIGHT,
            ..area
        };
        frame.render_widget(Paragraph::new(footer_line(app, area.width as usize)), footer);
    }
}

/// `agent-mgr        2● 1◉` — the name, then a count per attention-worthy state.
///
/// Counts are omitted when zero so the common case reads as calm rather than as a
/// row of zeroes.
fn header_line(app: &App, total_width: usize) -> Line<'static> {
    let theme = &app.theme;
    let mut right: Vec<Span<'static>> = Vec::new();
    let mut right_width = 0;

    for (count, glyph, color) in [
        (app.counts.blocked, "◉", theme.blocked),
        (app.counts.working, "●", theme.working),
        (app.counts.done, "●", theme.done),
        (app.counts.error, "✕", theme.error),
    ] {
        if count == 0 {
            continue;
        }
        let label = format!("{count}{glyph} ");
        right_width += width(&label);
        right.push(Span::styled(label, Style::default().fg(color)));
    }

    let title = truncate("agent-mgr", total_width.saturating_sub(right_width + 1));
    let mut spans = vec![Span::styled(
        title.clone(),
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
    )];
    spans.push(Span::raw(pad_to(width(&title) + right_width, total_width)));
    spans.extend(right);
    Line::from(spans)
}

/// What to say when the list is empty.
///
/// An empty list under a filter is a very different situation from an empty list
/// with no filter, and saying which one it is saves the user hunting for panes
/// that were never missing.
fn empty_state(app: &App, total_width: usize) -> Line<'static> {
    // Name whichever narrowing emptied the list, and how to undo that specific one.
    // "no panes" when a search is active sends you looking for panes that were
    // never missing.
    let text = match (&app.search, app.filter) {
        (Some(search), _) => format!("no match for {:?} — Esc to clear", search.query),
        (None, StatusFilter::All) => "no panes".to_owned(),
        (None, filter) => format!("nothing {} — Tab to change", filter_label(filter)),
    };
    Line::from(Span::styled(
        truncate(&text, total_width),
        Style::default().fg(app.theme.muted),
    ))
}

fn filter_label(filter: StatusFilter) -> &'static str {
    match filter {
        StatusFilter::All => "all",
        StatusFilter::Working => "working",
        StatusFilter::Blocked => "blocked",
        StatusFilter::Done => "done",
    }
}

/// The footer carries the active filter and, when it is hiding panes, how many.
/// A filter you forgot you set is the classic "why is my agent missing" bug.
fn footer_line(app: &App, total_width: usize) -> Line<'static> {
    let theme = &app.theme;
    if app.help {
        return prompt_line("any key to close", "", total_width, theme.muted, false);
    }
    // A rename is destructive-ish and mode-y, so it takes the footer outright and
    // says what it is renaming rather than showing a bare text box.
    if let Some(rename) = &app.rename {
        return prompt_line("rename: ", &rename.name, total_width, theme.accent, true);
    }
    // A half-typed count has to be visible: without an echo you cannot tell a
    // pending `1` from a keystroke that was dropped, and you find out only by
    // pressing `j` and going somewhere unexpected.
    if let Some(count) = app.pending_count {
        let text = truncate(&format!("{count}"), total_width);
        let content_width = width(&text);
        return Line::from(vec![
            Span::styled(
                text,
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(pad_to(content_width, total_width)),
        ]);
    }

    // While typing, the prompt owns the footer and shows a cursor; once committed it
    // shares the line with the filter, because at that point it is just another
    // thing narrowing the list and you need to see both.
    if let Some(search) = &app.search
        && search.editing
    {
        return prompt_line("/", &search.query, total_width, theme.accent, true);
    }

    let label = filter_label(app.filter).to_owned();
    let hidden = app.hidden_count();
    let mut text = match &app.search {
        Some(search) => format!("/{}", search.query),
        None => label,
    };
    if hidden > 0 {
        text = format!("{text} · {hidden} hidden");
    }
    let text = truncate(&text, total_width);
    let content_width = width(&text);
    let color = if app.filter == StatusFilter::All && app.search.is_none() {
        theme.muted
    } else {
        theme.accent
    };
    Line::from(vec![
        Span::styled(text, Style::default().fg(color)),
        Span::raw(pad_to(content_width, total_width)),
    ])
}

/// The preview, in the colours the mirrored panes were drawn in.
///
/// Padded out to the full height so an empty or short preview overwrites the
/// previous one instead of leaving its bottom rows on screen — the one place a
/// stale row would otherwise survive, since we never clear.
fn preview_lines(app: &App, width: u16) -> Vec<Line<'static>> {
    let height = list_height(app.size.1) as usize;
    (0..height)
        .map(|row| match app.preview_lines.get(row) {
            Some(line) => Line::from(
                line.spans
                    .iter()
                    .map(|span| {
                        Span::styled(span.text.clone(), preview_style(span.attrs, &app.theme))
                    })
                    .collect::<Vec<_>>(),
            ),
            None => Line::from(Span::raw(" ".repeat(width as usize))),
        })
        .collect()
}

/// Map one captured cell's attributes onto a ratatui style.
///
/// Text the pane coloured keeps its colour, which is the whole point: recognising
/// the window at a glance is mostly recognising its palette. Text it *didn't*
/// colour falls back to the muted tone the entire preview used to be drawn in, so
/// the mirror still reads as a mirror rather than as content you can interact with —
/// and a pane that emits no colour at all previews exactly as it did before.
///
/// The one attribute that is not the capture's is `selected`: the border of the pane
/// the cursor is on, drawn in the accent colour so it reads as *ours* against a
/// mirror of somebody else's colours.
fn preview_style(attrs: crate::preview::Attrs, theme: &theme::Theme) -> Style {
    let mut style = Style::default().fg(match (attrs.selected, attrs.fg) {
        (true, _) => theme.accent,
        (false, Some(colour)) => captured_colour(colour),
        (false, None) => theme.muted,
    });
    if let Some(colour) = attrs.bg {
        style = style.bg(captured_colour(colour));
    }

    let mut modifiers = Modifier::empty();
    // Bold is deliberately dropped: at preview scale it adds weight without adding
    // information, and on terminals that render it as a brighter colour it fights
    // the muted fallback above.
    if attrs.dim {
        modifiers |= Modifier::DIM;
    }
    if attrs.italic {
        modifiers |= Modifier::ITALIC;
    }
    if attrs.underline {
        modifiers |= Modifier::UNDERLINED;
    }
    if attrs.reverse {
        modifiers |= Modifier::REVERSED;
    }
    style.add_modifier(modifiers)
}

fn captured_colour(colour: crate::preview::Colour) -> Color {
    match colour {
        crate::preview::Colour::Indexed(index) => Color::Indexed(index),
        crate::preview::Colour::Rgb(red, green, blue) => Color::Rgb(red, green, blue),
    }
}

/// A footer prompt: a label, whatever has been typed, and optionally a cursor.
///
/// The cursor is a real cell reserved out of the width rather than a terminal
/// cursor, because the pane's actual cursor is wherever ratatui last left it and
/// moving it would be one more thing to keep in sync on every draw.
fn prompt_line(
    label: &str,
    text: &str,
    total_width: usize,
    color: Color,
    cursor: bool,
) -> Line<'static> {
    let cursor_width = usize::from(cursor);
    let body = truncate(
        &format!("{label}{text}"),
        total_width.saturating_sub(cursor_width),
    );
    let content_width = width(&body) + cursor_width;
    let mut spans = vec![Span::styled(body, Style::default().fg(color))];
    if cursor {
        spans.push(Span::styled("▏", Style::default().fg(color)));
    }
    spans.push(Span::raw(pad_to(content_width, total_width)));
    Line::from(spans)
}

/// Per-state tallies for the header.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct Counts {
    pub working: usize,
    pub blocked: usize,
    pub done: usize,
    pub error: usize,
}

impl Counts {
    /// Tally the *unfiltered* tree, so the header keeps telling you an agent
    /// needs attention even while a filter hides it.
    pub fn tally(sessions: &[crate::model::SessionGroup]) -> Self {
        let mut counts = Self::default();
        for pane in sessions
            .iter()
            .flat_map(|session| &session.windows)
            .flat_map(|window| &window.panes)
        {
            let status = &pane.status;
            if status.agent.is_none() {
                continue;
            }
            match status.state {
                AgentState::Working => counts.working += 1,
                AgentState::Blocked => counts.blocked += 1,
                AgentState::Error => counts.error += 1,
                AgentState::Idle if !status.seen => counts.done += 1,
                AgentState::Idle | AgentState::Unknown => {}
            }
        }
        counts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        AgentKind, AgentStatus, PaneInfo, SessionGroup, StatusSource, WindowInfo,
    };

    fn pane(state: AgentState, seen: bool, agent: Option<AgentKind>) -> PaneInfo {
        PaneInfo {
            pane_id: "%1".to_owned(),
            window_id: "@1".to_owned(),
            pane_index: "0".to_owned(),
            pane_active: false,
            current_command: "zsh".to_owned(),
            current_path: "/tmp".to_owned(),
            title: String::new(),
            pane_pid: None,
            status: AgentStatus {
                agent,
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

    // ─── popup layout ─────────────────────────────────────────────────

    #[test]
    fn a_sidebar_never_gives_width_to_a_preview() {
        // However wide the pane gets, a sidebar has no preview to put there.
        for width in [24, 80, 200] {
            assert_eq!(split_width(Surface::Sidebar, width), (width, None));
        }
    }

    #[test]
    fn a_wide_popup_splits_into_list_and_preview() {
        let (list, preview) = split_width(Surface::Popup, 200);
        assert_eq!(list, LIST_WIDTH);
        assert_eq!(preview, Some(200 - LIST_WIDTH));
    }

    #[test]
    fn a_narrow_popup_keeps_the_whole_width_for_the_list() {
        // A 12-column preview is decoration; the list is the thing being navigated,
        // so it takes the space rather than both being unusable.
        let (list, preview) = split_width(Surface::Popup, LIST_WIDTH + MIN_PREVIEW_WIDTH - 1);
        assert_eq!(list, LIST_WIDTH + MIN_PREVIEW_WIDTH - 1);
        assert_eq!(preview, None);
    }

    #[test]
    fn the_split_never_exceeds_the_width_it_was_given() {
        for width in 0..=240u16 {
            let (list, preview) = split_width(Surface::Popup, width);
            assert_eq!(list + preview.unwrap_or(0), width, "at width {width}");
        }
    }

    #[test]
    fn preview_area_is_absent_without_room_or_without_a_preview_surface() {
        assert!(preview_area(Surface::Sidebar, (200, 50)).is_none());
        assert!(preview_area(Surface::Popup, (40, 50)).is_none(), "too narrow");
        // Height 2 is entirely header plus footer.
        assert!(preview_area(Surface::Popup, (200, 2)).is_none(), "no rows");
        let area = preview_area(Surface::Popup, (200, 50)).unwrap();
        assert_eq!(area.width, (200 - LIST_WIDTH) as usize);
        assert_eq!(area.height, list_height(50) as usize);
    }

    #[test]
    fn list_height_reserves_the_header_and_footer_without_underflowing() {
        assert_eq!(list_height(40), 38);
        assert_eq!(list_height(2), 0);
        assert_eq!(list_height(1), 0);
        assert_eq!(list_height(0), 0);
    }

    // ─── preview styling ──────────────────────────────────────────────

    #[test]
    fn a_marked_border_is_drawn_in_the_accent_and_everything_else_is_not() {
        // The selection mark is the one attribute that is ours rather than the
        // capture's, so it has to win over both the muted fallback and a colour the
        // pane happened to leave in force.
        let theme = theme::Theme::default();
        let marked = crate::preview::Attrs {
            selected: true,
            ..Default::default()
        };
        assert_eq!(preview_style(marked, &theme).fg, Some(theme.accent));

        let plain = crate::preview::Attrs::default();
        assert_eq!(preview_style(plain, &theme).fg, Some(theme.muted));

        let coloured = crate::preview::Attrs {
            fg: Some(crate::preview::Colour::Indexed(2)),
            ..Default::default()
        };
        assert_eq!(preview_style(coloured, &theme).fg, Some(Color::Indexed(2)));
    }

    #[test]
    fn counts_only_tally_panes_that_actually_host_an_agent() {
        let counts = Counts::tally(&tree(vec![
            pane(AgentState::Working, true, Some(AgentKind::Claude)),
            pane(AgentState::Blocked, true, Some(AgentKind::Codex)),
            pane(AgentState::Idle, false, Some(AgentKind::Claude)),
            pane(AgentState::Idle, true, Some(AgentKind::Claude)),
            pane(AgentState::Error, true, Some(AgentKind::Claude)),
            // A plain shell is not an agent, whatever state it carries.
            pane(AgentState::Working, true, None),
        ]));
        assert_eq!(
            counts,
            Counts {
                working: 1,
                blocked: 1,
                done: 1,
                error: 1
            }
        );
    }

    #[test]
    fn counts_of_a_quiet_workspace_are_all_zero() {
        assert_eq!(
            Counts::tally(&tree(vec![pane(
                AgentState::Idle,
                true,
                Some(AgentKind::Claude)
            )])),
            Counts::default()
        );
        assert_eq!(Counts::tally(&[]), Counts::default());
    }
}
