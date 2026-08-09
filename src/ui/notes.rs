//! Turning the scratchpad into lines.
//!
//! Pure, on the same contract as [`crate::ui::rows`]: a parsed file in, lines
//! out, no I/O and no tmux. That is what lets the event loop hash the panel's
//! text alongside the list's and keep the zero-frames-when-idle promise, and it
//! is why every test below runs without a filesystem or a terminal.
//!
//! The panel shows titles only. A note's body is read in the detail overlay, on
//! the grounds that a sidebar column is the wrong shape for prose and that the
//! title is the part you wrote in order to recognise it later.
//!
//! Checkboxes are rendered `[ ]` / `[x]`, exactly as [`crate::notes`] writes
//! them to the file. The panel is a view onto a markdown document three
//! different things edit, and matching its vocabulary is what makes the two
//! read as the same object rather than as a UI with a file behind it.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::notes::{Note, NoteFile};
use crate::ui::text::{pad_to, truncate, width};
use crate::ui::theme::Theme;

/// The checkbox, written the way the file writes it.
const OPEN_BOX: &str = "[ ]";
const DONE_BOX: &str = "[x]";

/// Marker in the header when notes sit below the fold. One cell, so the counts
/// beside it do not shift as the panel scrolls.
const MORE_BELOW: &str = "↓";

/// Fills the header between the label and the counts.
///
/// The panel butts straight up against the last pane row, and with a full list
/// the muted label alone is not enough to say "a different thing starts here".
/// A rule costs no rows, which is the only currency the sidebar is short of.
const RULE: char = '─';
/// Below this the rule is a stub that reads as a stray dash, so the header falls
/// back to plain spaces. Two cells of rule plus the space either side.
const MIN_RULE_ROOM: usize = 4;

/// The rendered panel: lines to draw, and their text to hash.
#[derive(Default)]
pub struct RenderedNotes {
    pub lines: Vec<Line<'static>>,
    /// Plain text of each line, the change fingerprint — same contract and same
    /// reasoning as [`crate::ui::rows::RenderedList::plain`].
    pub plain: Vec<String>,
}

impl RenderedNotes {
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Record one line under both representations at once, so the text the loop
    /// hashes cannot drift from the spans it draws.
    fn push(&mut self, spans: Vec<Span<'static>>) {
        self.plain
            .push(spans.iter().map(|span| span.content.as_ref()).collect());
        self.lines.push(Line::from(spans));
    }
}

/// Everything the panel needs beyond the notes themselves.
#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    pub total_width: usize,
    /// Rows the panel occupies, its header included. Zero renders nothing,
    /// which is how a collapsed or empty panel gives every row back.
    pub height: usize,
    /// First note shown. Clamped here rather than trusted, because it comes from
    /// a cursor a concurrent append may have moved out from under.
    pub scroll: usize,
    /// The highlighted note, when the panel has the keyboard. `None` draws no
    /// cursor at all — which is what the panel looks like while you are
    /// navigating panes, so it never claims a focus it does not have.
    pub selected: Option<usize>,
}

/// Render the panel.
///
/// Always returns exactly `opts.height` lines, each exactly `opts.total_width`
/// cells. Short panels are padded rather than left short: nothing in this crate
/// clears the screen, so a panel that shrank would otherwise leave its old
/// bottom rows on display.
pub fn build(file: &NoteFile, opts: &Options, theme: &Theme) -> RenderedNotes {
    let mut out = RenderedNotes::default();
    if opts.height == 0 {
        return out;
    }
    let total_width = opts.total_width;

    // One row goes to the header; the rest are notes.
    let rows = opts.height - 1;
    // Never scroll past the last screenful — a panel showing blank rows below
    // the final note reads as notes having been lost.
    let scroll = opts.scroll.min(file.len().saturating_sub(rows));
    let hidden = file.len().saturating_sub(scroll + rows);

    out.push(header(file, hidden, total_width, theme));
    for (offset, note) in file.notes.iter().skip(scroll).take(rows).enumerate() {
        let selected = opts.selected == Some(scroll + offset);
        out.push(note_line(note, selected, total_width, theme));
    }
    while out.lines.len() < opts.height {
        out.push(vec![Span::raw(" ".repeat(total_width))]);
    }
    out
}

/// `notes ───────────── ↓ 2/5` — the label, a rule to divide the panel off from
/// the list above it, then how much is open and how much of it you are looking
/// at.
///
/// The counts are always shown rather than only when they are interesting: a
/// number that appears and disappears makes the row jump, and "how many of these
/// are still open" is the question the panel exists to answer at a glance.
fn header(file: &NoteFile, hidden: usize, total_width: usize, theme: &Theme) -> Vec<Span<'static>> {
    let open = file.notes.iter().filter(|note| !note.done).count();
    let counts = format!("{open}/{}", file.len());
    let right = if hidden > 0 {
        format!("{MORE_BELOW} {counts}")
    } else {
        counts
    };

    let indent = usize::from(total_width >= 1);
    let avail = total_width - indent;
    // The counts are the information; the word "notes" is only a label, so it is
    // what gives way when the sidebar is too narrow for both.
    let right = truncate(&right, avail);
    let label = truncate("notes", avail.saturating_sub(width(&right) + 1));

    // Whatever is left between the two, spaces included. A narrow sidebar spends
    // it on spaces rather than on a two-cell stub that reads as a typo.
    let room = avail - width(&label) - width(&right);
    let rule = if room >= MIN_RULE_ROOM {
        format!(" {} ", String::from(RULE).repeat(room - 2))
    } else {
        " ".repeat(room)
    };

    let muted = Style::default().fg(theme.muted);
    vec![
        Span::raw(" ".repeat(indent)),
        Span::styled(label, muted),
        // Dimmer than the label it follows: the rule is structure, not content,
        // and at full strength it out-shouts the note titles below it.
        Span::styled(rule, muted.add_modifier(Modifier::DIM)),
        Span::styled(right, muted),
    ]
}

/// ` [ ] auth redirect drops ?next`
fn note_line(
    note: &Note,
    selected: bool,
    total_width: usize,
    theme: &Theme,
) -> Vec<Span<'static>> {
    let bg = selected.then_some(theme.selection_bg);
    let base = match bg {
        Some(color) => Style::default().bg(color),
        None => Style::default(),
    };

    let indent = usize::from(total_width >= 1);
    let avail = total_width - indent;
    // Truncating the checkbox rather than assuming it fits: the whole row must
    // come to exactly `total_width`, and a sidebar clamped down to a handful of
    // columns must clip instead of overflowing and wrapping the row below.
    let checkbox = truncate(if note.done { DONE_BOX } else { OPEN_BOX }, avail);
    let gap = usize::from(avail > width(&checkbox));
    let title = truncate(
        &note.title,
        avail.saturating_sub(width(&checkbox) + gap),
    );

    // A done note stays visible — crossing things off is most of the point — but
    // recedes, so the open ones are what the eye lands on.
    let (box_color, title_style) = if note.done {
        (theme.muted, base.fg(theme.muted).add_modifier(Modifier::DIM))
    } else {
        (theme.accent, base.fg(theme.text))
    };

    let used = indent + width(&checkbox) + gap + width(&title);
    let mut spans = vec![
        Span::styled(" ".repeat(indent), base),
        Span::styled(checkbox, base.fg(box_color)),
        Span::styled(" ".repeat(gap), base),
    ];
    spans.extend(title_spans(&title, title_style, base, theme, note.done));
    spans.push(Span::styled(pad_to(used, total_width), base));
    spans
}

/// Split a title on its inline `` `code` `` spans and colour them.
///
/// Titles are full of identifiers — `?next`, `@agent_mgr_bin`, `##` — and the
/// backticks are already in the file because the format is markdown. Colouring
/// them costs no columns and turns punctuation that was noise into the part of
/// the title you were looking for.
///
/// Deliberately only this one span type. Bold and links are worth having in the
/// overlay, where there is room to read; in a one-line title they would be four
/// more ways for a row to look busy.
fn title_spans(
    title: &str,
    text_style: Style,
    base: Style,
    theme: &Theme,
    done: bool,
) -> Vec<Span<'static>> {
    if !title.contains('`') {
        return vec![Span::styled(title.to_owned(), text_style)];
    }
    // A done note stays uniformly receded — picking code out of a crossed-off
    // row would give it more weight than the open ones above it.
    let code_style = if done {
        text_style
    } else {
        base.fg(theme.branch)
    };

    let mut spans = Vec::new();
    let mut plain = String::new();
    let mut rest = title;
    while !rest.is_empty() {
        // An unmatched backtick is left as literal text: a truncated title ends
        // mid-span far more often than someone means a lone one.
        if let Some(body) = rest.strip_prefix('`')
            && let Some(end) = body.find('`')
        {
            if !plain.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut plain), text_style));
            }
            spans.push(Span::styled(rest[..end + 2].to_owned(), code_style));
            rest = &rest[end + 2..];
            continue;
        }
        let step = rest.chars().next().map_or(1, char::len_utf8);
        plain.push_str(&rest[..step]);
        rest = &rest[step..];
    }
    if !plain.is_empty() {
        spans.push(Span::styled(plain, text_style));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(titles: &[(&str, bool)]) -> NoteFile {
        NoteFile {
            preamble: String::new(),
            notes: titles
                .iter()
                .map(|(title, done)| Note {
                    done: *done,
                    title: (*title).to_owned(),
                    meta: Vec::new(),
                    body: String::new(),
                })
                .collect(),
        }
    }

    fn text_of(line: &Line<'static>) -> String {
        line.spans.iter().map(|span| span.content.as_ref()).collect()
    }

    fn render(file: &NoteFile, opts: Options) -> Vec<String> {
        build(file, &opts, &Theme::default())
            .lines
            .iter()
            .map(text_of)
            .collect()
    }

    #[test]
    fn every_notes_line_is_exactly_the_requested_width() {
        // Invariant 5, and the one that bites: a row one cell too wide wraps and
        // shifts every row below it, including the footer. CJK and emoji titles
        // are here because they are the case where counting chars and counting
        // cells disagree.
        let notes = file(&[
            ("auth redirect drops ?next", false),
            ("日本語のタイトルはここにある", false),
            ("🎉 shipped the thing", true),
            ("x", false),
        ]);
        for total_width in 0..=80usize {
            for height in 0..=8usize {
                let lines = render(
                    &notes,
                    Options {
                        total_width,
                        height,
                        ..Options::default()
                    },
                );
                for line in &lines {
                    assert_eq!(
                        width(line),
                        total_width,
                        "at width {total_width} height {height}: {line:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_panel_always_fills_the_rows_it_was_given() {
        // Nothing in this crate clears the screen, so a panel that returned fewer
        // lines than its rect would leave the previous frame's notes on display
        // underneath it.
        let notes = file(&[("one", false)]);
        for height in 0..=6usize {
            let lines = render(
                &notes,
                Options {
                    total_width: 30,
                    height,
                    ..Options::default()
                },
            );
            assert_eq!(lines.len(), height, "height {height}");
        }
    }

    #[test]
    fn a_zero_height_panel_renders_nothing_rather_than_a_bare_header() {
        assert!(
            build(
                &file(&[("one", false)]),
                &Options {
                    total_width: 30,
                    height: 0,
                    ..Options::default()
                },
                &Theme::default(),
            )
            .is_empty()
        );
    }

    #[test]
    fn the_header_counts_open_notes_against_the_total() {
        let notes = file(&[("a", false), ("b", true), ("c", false)]);
        let lines = render(
            &notes,
            Options {
                total_width: 30,
                height: 4,
                ..Options::default()
            },
        );
        assert!(lines[0].contains("2/3"), "{:?}", lines[0]);
        assert!(lines[0].contains("notes"), "{:?}", lines[0]);
    }

    #[test]
    fn the_header_rules_the_panel_off_from_the_list_above_it() {
        // With a full pane list the panel starts immediately under the last pane
        // row, and a muted label alone does not say "a different thing begins
        // here". The rule costs no rows, which is the currency the sidebar is
        // short of.
        let notes = file(&[("a", false)]);
        let wide = render(
            &notes,
            Options {
                total_width: 40,
                height: 2,
                ..Options::default()
            },
        );
        assert!(wide[0].contains(RULE), "{:?}", wide[0]);
        // The label and the counts still keep their own space around it.
        assert!(wide[0].starts_with(" notes "), "{:?}", wide[0]);
        assert!(wide[0].ends_with(" 1/1"), "{:?}", wide[0]);
    }

    #[test]
    fn a_narrow_header_drops_the_rule_rather_than_stubbing_it() {
        // Two dashes between a label and a number reads as a typo, not as a
        // divider.
        for total_width in 0..=(1 + width("notes") + MIN_RULE_ROOM + width("1/1") - 1) {
            let line = &render(
                &file(&[("a", false)]),
                Options {
                    total_width,
                    height: 2,
                    ..Options::default()
                },
            )[0];
            assert!(!line.contains(RULE), "at width {total_width}: {line:?}");
        }
    }

    #[test]
    fn the_header_says_when_notes_are_below_the_fold() {
        // Without this the panel silently lies about how much is in the file, and
        // a note you added a moment ago looks like it was dropped.
        let notes = file(&[("a", false), ("b", false), ("c", false), ("d", false)]);
        let cramped = render(
            &notes,
            Options {
                total_width: 30,
                height: 3,
                ..Options::default()
            },
        );
        assert!(cramped[0].contains(MORE_BELOW), "{:?}", cramped[0]);

        let roomy = render(
            &notes,
            Options {
                total_width: 30,
                height: 5,
                ..Options::default()
            },
        );
        assert!(!roomy[0].contains(MORE_BELOW), "{:?}", roomy[0]);
    }

    #[test]
    fn scrolling_past_the_end_still_shows_a_full_panel_of_notes() {
        // The scroll offset trails a cursor that a concurrent append or a delete
        // can move; clamping here is what stops a stale offset rendering a panel
        // of blank rows.
        let notes = file(&[("a", false), ("b", false), ("c", false), ("d", false)]);
        let lines = render(
            &notes,
            Options {
                total_width: 30,
                height: 3,
                scroll: 99,
                ..Options::default()
            },
        );
        assert!(lines[1].contains('c'), "{:?}", lines[1]);
        assert!(lines[2].contains('d'), "{:?}", lines[2]);
    }

    #[test]
    fn a_scrolled_panel_shows_the_notes_the_offset_asked_for() {
        let notes = file(&[("a", false), ("b", false), ("c", false), ("d", false)]);
        let lines = render(
            &notes,
            Options {
                total_width: 30,
                height: 3,
                scroll: 1,
                ..Options::default()
            },
        );
        assert!(lines[1].contains('b'), "{:?}", lines[1]);
        assert!(lines[2].contains('c'), "{:?}", lines[2]);
    }

    #[test]
    fn checkboxes_read_the_way_the_file_writes_them() {
        let notes = file(&[("open one", false), ("closed one", true)]);
        let lines = render(
            &notes,
            Options {
                total_width: 30,
                height: 3,
                ..Options::default()
            },
        );
        assert!(lines[1].contains(OPEN_BOX), "{:?}", lines[1]);
        assert!(lines[2].contains(DONE_BOX), "{:?}", lines[2]);
    }

    #[test]
    fn no_note_is_highlighted_until_the_panel_has_focus() {
        // A cursor drawn in an unfocused panel claims keys it will not receive.
        let notes = file(&[("a", false), ("b", false)]);
        let theme = Theme::default();
        let unfocused = build(
            &notes,
            &Options {
                total_width: 30,
                height: 3,
                ..Options::default()
            },
            &theme,
        );
        assert!(
            unfocused.lines[1]
                .spans
                .iter()
                .all(|span| span.style.bg.is_none()),
            "an unfocused panel drew a selection"
        );

        let focused = build(
            &notes,
            &Options {
                total_width: 30,
                height: 3,
                selected: Some(0),
                ..Options::default()
            },
            &theme,
        );
        assert!(
            focused.lines[1]
                .spans
                .iter()
                .all(|span| span.style.bg == Some(theme.selection_bg)),
            "the selection background must cover the whole row"
        );
    }

    #[test]
    fn a_selection_the_scroll_has_moved_past_highlights_nothing() {
        // The index is into the file, not into the visible rows; getting that
        // wrong would highlight an unrelated note after any scroll.
        let notes = file(&[("a", false), ("b", false), ("c", false)]);
        let rendered = build(
            &notes,
            &Options {
                total_width: 30,
                height: 3,
                scroll: 1,
                selected: Some(0),
            },
            &Theme::default(),
        );
        assert!(
            rendered
                .lines
                .iter()
                .flat_map(|line| &line.spans)
                .all(|span| span.style.bg.is_none())
        );
    }

    #[test]
    fn the_plain_text_matches_the_spans_that_are_drawn() {
        // The loop hashes `plain` and draws `lines`; if they can disagree, the
        // change test stops being a change test.
        let notes = file(&[("a", false), ("b", true)]);
        let rendered = build(
            &notes,
            &Options {
                total_width: 24,
                height: 4,
                ..Options::default()
            },
            &Theme::default(),
        );
        let drawn: Vec<String> = rendered.lines.iter().map(text_of).collect();
        assert_eq!(rendered.plain, drawn);
    }

    #[test]
    fn reparsing_identical_content_does_not_change_the_fingerprint() {
        // Protects the zero-frames-when-idle claim: without it an idle sidebar
        // would start redrawing on every poll that re-read the notes file.
        let notes = file(&[("a", false), ("b", true)]);
        let opts = Options {
            total_width: 30,
            height: 4,
            ..Options::default()
        };
        let first = build(&notes, &opts, &Theme::default());
        let second = build(&file(&[("a", false), ("b", true)]), &opts, &Theme::default());
        assert_eq!(first.plain, second.plain);
    }

    #[test]
    fn inline_code_in_a_title_is_coloured_without_costing_a_column() {
        // The backticks are already in the file because the format is markdown;
        // colouring them turns punctuation that was noise into the part of the
        // title you were looking for. It must not change the row's width.
        let theme = Theme::default();
        let notes = file(&[("set `@agent_mgr_bin` first", false)]);
        let rendered = build(
            &notes,
            &Options {
                total_width: 40,
                height: 2,
                ..Options::default()
            },
            &theme,
        );
        assert_eq!(width(&rendered.plain[1]), 40);
        assert!(
            rendered.lines[1]
                .spans
                .iter()
                .any(|span| span.style.fg == Some(theme.branch)),
            "no code span was coloured"
        );
    }

    #[test]
    fn a_done_title_stays_uniformly_receded() {
        // Picking code out of a crossed-off row would give it more weight than
        // the open notes above it.
        let theme = Theme::default();
        let rendered = build(
            &file(&[("ship `notes.rs`", true)]),
            &Options {
                total_width: 40,
                height: 2,
                ..Options::default()
            },
            &theme,
        );
        assert!(
            rendered.lines[1]
                .spans
                .iter()
                .all(|span| span.style.fg != Some(theme.branch))
        );
    }

    #[test]
    fn an_unmatched_backtick_in_a_title_is_left_as_text() {
        // A truncated title ends mid-span far more often than anyone means a
        // lone backtick.
        let theme = Theme::default();
        let rendered = build(
            &file(&[("a lone ` backtick", false)]),
            &Options {
                total_width: 40,
                height: 2,
                ..Options::default()
            },
            &theme,
        );
        assert!(rendered.plain[1].contains("a lone ` backtick"));
        assert!(
            rendered.lines[1]
                .spans
                .iter()
                .all(|span| span.style.fg != Some(theme.branch))
        );
    }

    #[test]
    fn a_long_title_is_clipped_rather_than_wrapped() {
        let notes = file(&[("a title far longer than the sidebar is wide", false)]);
        let lines = render(
            &notes,
            Options {
                total_width: 20,
                height: 2,
                ..Options::default()
            },
        );
        assert!(lines[1].contains('…'), "{:?}", lines[1]);
    }
}
