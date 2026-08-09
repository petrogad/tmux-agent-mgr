//! Colouring a note's markdown for the terminal.
//!
//! This is what `note show` prints into the detail popup. It highlights the
//! *structure* of a note — headings, the checkbox, the origin comment, fences,
//! list markers, inline code, emphasis — and deliberately stops there. Tokenising
//! the contents of a `rust` or `sh` fence needs a grammar per language, which is
//! a dependency and a maintenance surface out of all proportion to a scratchpad.
//! A fence gets one colour, which is enough to say "this part is code".
//!
//! Colours come from the same [`Theme`] the sidebar draws with, so an
//! `@agent_mgr_color_*` override applies here too and the popup reads as part of
//! the same plugin rather than as a separate program that happens to be adjacent.
//!
//! Pure: markdown in, a `String` with ANSI escapes out. No terminal, no I/O, so
//! every rule below is testable by looking at the string.

use ratatui::style::Color;

use crate::ui::theme::Theme;

/// Whether to emit escapes at all.
///
/// The usual three, for the usual reason: `note show` is piped to a pager by the
/// popup, so an `IsTerminal` check on stdout would disable colour in exactly the
/// case that wants it. The popup passes `always` because it knows it is handing
/// the output to `less -R`; a human running `note show > file` gets `auto`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum When {
    #[default]
    Auto,
    Always,
    Never,
}

impl When {
    /// Parse a `--color=` value. `None` for anything unrecognised, so a typo is
    /// a usage error rather than a silent fallback to the opposite of what was
    /// asked for.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "always" | "yes" | "force" => Some(Self::Always),
            "never" | "no" | "none" => Some(Self::Never),
            _ => None,
        }
    }

    /// Resolve against whether stdout is actually a terminal.
    pub fn enabled(self, is_terminal: bool) -> bool {
        match self {
            Self::Auto => is_terminal,
            Self::Always => true,
            Self::Never => false,
        }
    }
}

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const ITALIC: &str = "\x1b[3m";

/// One colour as an SGR foreground sequence.
fn fg(color: Color) -> String {
    match color {
        Color::Indexed(index) => format!("\x1b[38;5;{index}m"),
        Color::Rgb(red, green, blue) => format!("\x1b[38;2;{red};{green};{blue}m"),
        // Every colour this crate produces is one of the two above; anything else
        // would be a theme default we did not write, and no escape is a safer
        // answer than a guessed one.
        _ => String::new(),
    }
}

/// Wrap `text` in a colour, closing with a full reset.
///
/// A full reset rather than `39m` because these spans also carry bold: leaving
/// the terminal bold after a heading would bleed into the note's body, and the
/// popup is a fresh screen every time so there is no prior state worth keeping.
fn paint(text: &str, color: Color, bold: bool) -> String {
    if text.is_empty() {
        return String::new();
    }
    let weight = if bold { BOLD } else { "" };
    format!("{weight}{}{text}{RESET}", fg(color))
}

/// Colour one note's markdown.
///
/// `enabled` false returns the input untouched rather than a stripped copy — the
/// input never had escapes in it, and passing it through is what makes
/// `--color=never` provably identical to the old plain output.
pub fn note(markdown: &str, theme: &Theme, enabled: bool) -> String {
    if !enabled {
        return markdown.to_owned();
    }
    let mut out = String::with_capacity(markdown.len() * 2);
    let mut fence: Option<Fence> = None;

    for line in markdown.lines() {
        match &fence {
            // Inside a fence everything is code, headings and backticks included.
            // Notes are mostly *about* code, so this is the common case and
            // getting it wrong would colour half a snippet as markup.
            Some(open) => {
                if open.closes(line) {
                    fence = None;
                    out.push_str(&paint(line, theme.muted, false));
                } else {
                    out.push_str(&paint(line, theme.branch, false));
                }
            }
            None => match Fence::opens(line) {
                Some(open) => {
                    fence = Some(open);
                    out.push_str(&paint(line, theme.muted, false));
                }
                None => out.push_str(&structural(line, theme)),
            },
        }
        out.push('\n');
    }
    out
}

/// One line outside any fence.
fn structural(line: &str, theme: &Theme) -> String {
    // The origin stamp. Muted rather than hidden: it is the answer to "where was
    // I when I wrote this", which is worth having and not worth reading first.
    let trimmed = line.trim_start();
    if trimmed.starts_with("<!--") {
        return paint(line, theme.muted, false);
    }
    if let Some(heading) = heading(line, theme) {
        return heading;
    }
    if let Some((marker, rest)) = list_marker(line) {
        return format!(
            "{}{}",
            paint(marker, theme.accent, false),
            inline(rest, theme, None, false)
        );
    }
    inline(line, theme, None, false)
}

/// `## [x] title` and `### Sub` and the rest.
///
/// The note's own `##` is the accent, its subsections are the session colour —
/// two levels is all a note needs, and colouring them apart is what stops a long
/// body reading as one undifferentiated block.
fn heading(line: &str, theme: &Theme) -> Option<String> {
    let hashes = line.len() - line.trim_start_matches('#').len();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = line[hashes..].strip_prefix(' ')?;
    // The hashes are syntax, not content. Receding them is what lets the title
    // lead the line instead of competing with its own markup — and it is the
    // whole reason to colour a heading rather than just embolden it.
    let marks = paint(&format!("{} ", "#".repeat(hashes)), theme.muted, false);
    let color = if hashes <= 2 { theme.accent } else { theme.session };

    // A note heading carries its checkbox, coloured by state so a done note reads
    // as done in the popup exactly as it does in the panel.
    if let Some(tail) = rest.strip_prefix("[ ] ") {
        return Some(format!(
            "{marks}{}{}",
            paint("[ ] ", theme.accent, false),
            inline(tail, theme, Some(color), true),
        ));
    }
    if let Some(tail) = rest.strip_prefix("[x] ").or_else(|| rest.strip_prefix("[X] ")) {
        return Some(format!(
            "{marks}{}{}",
            paint("[x] ", theme.done, false),
            inline(tail, theme, Some(theme.muted), false),
        ));
    }
    Some(format!("{marks}{}", inline(rest, theme, Some(color), true)))
}

/// A leading `- `, `* ` or `1. `, returned with the text after it.
fn list_marker(line: &str) -> Option<(&str, &str)> {
    let indent = line.len() - line.trim_start().len();
    let rest = &line[indent..];
    for bullet in ["- ", "* ", "+ "] {
        if rest.starts_with(bullet) {
            return Some((&line[..indent + bullet.len()], &line[indent + bullet.len()..]));
        }
    }
    // `12. ` — digits then a dot then a space.
    let digits = rest.len() - rest.trim_start_matches(|ch: char| ch.is_ascii_digit()).len();
    if digits > 0 && rest[digits..].starts_with(". ") {
        let end = indent + digits + 2;
        return Some((&line[..end], &line[end..]));
    }
    None
}

/// Inline spans: `` `code` ``, `**bold**` and `*italic*`.
///
/// `base` is what the surrounding text is painted in — `None` for body prose,
/// which is left unpainted so it takes the terminal's own foreground, and
/// `Some(colour)` inside a heading so the parts either side of a code span keep
/// the heading's colour instead of dropping back to plain.
///
/// A scanner rather than a regex, and a deliberately shallow one — unmatched
/// delimiters are left as literal text, because a stray backtick in a note is far
/// more likely than a note that meant to open a span and never close it.
fn inline(line: &str, theme: &Theme, base: Option<Color>, bold: bool) -> String {
    let mut out = String::with_capacity(line.len());
    let mut plain = String::new();
    let mut rest = line;

    // Runs of ordinary text are accumulated and painted together, so a heading
    // does not end up as one escape sequence per character.
    let flush = |plain: &mut String, out: &mut String| {
        if plain.is_empty() {
            return;
        }
        let text = std::mem::take(plain);
        match base {
            Some(color) => out.push_str(&paint(&text, color, bold)),
            None if bold => out.push_str(&format!("{BOLD}{text}{RESET}")),
            None => out.push_str(&text),
        }
    };

    let emphasis = |span: &str, attr: &str| {
        let color = base.map(fg).unwrap_or_default();
        format!("{attr}{color}{span}{RESET}")
    };

    while !rest.is_empty() {
        // Code first: inside backticks, asterisks are literal.
        if let Some((span, tail)) = span_at(rest, "`", false) {
            flush(&mut plain, &mut out);
            out.push_str(&paint(span, theme.branch, bold));
            rest = tail;
            continue;
        }
        // `**` before `*`, or every bold span parses as two empty italics.
        if let Some((span, tail)) = span_at(rest, "**", true) {
            flush(&mut plain, &mut out);
            out.push_str(&emphasis(span, BOLD));
            rest = tail;
            continue;
        }
        if let Some((span, tail)) = span_at(rest, "*", true) {
            flush(&mut plain, &mut out);
            out.push_str(&emphasis(span, ITALIC));
            rest = tail;
            continue;
        }
        // Advance one character, not one byte: a multibyte glyph must not be cut.
        let step = rest.chars().next().map_or(1, char::len_utf8);
        plain.push_str(&rest[..step]);
        rest = &rest[step..];
    }
    flush(&mut plain, &mut out);
    out
}

/// A delimited span at the very start of `rest`, delimiters included.
///
/// `tight` requires the opening delimiter to be followed by a non-space, which is
/// what stops `a * b * c` — a glob, a multiplication, a shell line — from
/// italicising the middle of a sentence. Code spans are not tight: `` ` foo ` ``
/// is legitimate and common.
fn span_at<'a>(rest: &'a str, delim: &str, tight: bool) -> Option<(&'a str, &'a str)> {
    let body = rest.strip_prefix(delim)?;
    if tight && body.starts_with(|ch: char| ch.is_whitespace()) {
        return None;
    }
    let end = body.find(delim)?;
    if end == 0 {
        return None;
    }
    let span_end = delim.len() + end + delim.len();
    Some((&rest[..span_end], &rest[span_end..]))
}

/// An open code fence, tracked precisely enough to know what closes it.
///
/// The same simplification [`crate::notes`] makes when parsing: the marker and
/// its length, ignoring info strings and indentation. Enough to keep a snippet
/// in one piece, and well short of being a markdown parser.
struct Fence {
    marker: char,
    length: usize,
}

impl Fence {
    fn opens(line: &str) -> Option<Self> {
        let trimmed = line.trim_start();
        let marker = trimmed.chars().next().filter(|ch| *ch == '`' || *ch == '~')?;
        let length = trimmed.chars().take_while(|ch| *ch == marker).count();
        (length >= 3).then_some(Self { marker, length })
    }

    fn closes(&self, line: &str) -> bool {
        let trimmed = line.trim_start();
        let length = trimmed.chars().take_while(|ch| *ch == self.marker).count();
        // A closing fence is at least as long as the one that opened it and
        // carries nothing else, which is what lets ``` sit inside a ````.
        length >= self.length && trimmed[length..].trim().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coloured(markdown: &str) -> String {
        note(markdown, &Theme::default(), true)
    }

    /// The text with every escape removed — what a reader actually sees.
    fn visible(painted: &str) -> String {
        let mut out = String::new();
        let mut rest = painted;
        while let Some(start) = rest.find('\x1b') {
            out.push_str(&rest[..start]);
            let after = &rest[start..];
            match after.find('m') {
                Some(end) => rest = &after[end + 1..],
                None => break,
            }
        }
        out.push_str(rest);
        out
    }

    #[test]
    fn colour_never_returns_the_markdown_untouched() {
        // The guarantee that `--color=never` is exactly the old plain output, not
        // a best-effort strip of escapes we just added.
        let source = "## [ ] one\n`code` and **bold**\n";
        assert_eq!(note(source, &Theme::default(), false), source);
    }

    #[test]
    fn highlighting_changes_no_visible_character() {
        // The one property that matters: this is decoration. If a rule ever eats
        // or duplicates text, the note is being rewritten rather than coloured.
        let source = "\
# Notes

## [x] auth redirect drops ?next
<!-- t=1770000000 from=blueberry:3 -->
The 302 loses the `next` param, **badly**.

### Repro

1. log out
2. hit `/private`
- and a bullet

```sh
curl -i localhost:3000/callback   ## not a heading
```

日本語と絵文字 🎉 も `code` も
";
        assert_eq!(visible(&coloured(source)), source);
    }

    #[test]
    fn a_fence_is_one_colour_and_swallows_markup_inside_it() {
        // Notes are mostly about code, so a `##` or a backtick inside a snippet
        // is the common case, not an exotic one.
        let out = coloured("```sh\n## not a heading\n`not inline code`\n```\n");
        let theme = Theme::default();
        assert!(out.contains(&fg(theme.branch)), "fence body is code-coloured");
        assert!(
            !out.contains(&fg(theme.session)),
            "a ## inside a fence must not be painted as a subheading"
        );
    }

    #[test]
    fn a_longer_fence_can_contain_a_shorter_one() {
        let out = coloured("````\n```\n## still code\n````\n\n## a real heading\n");
        let theme = Theme::default();
        // The heading after the outer fence closed is a heading again.
        assert!(out.contains(&fg(theme.accent)));
        assert_eq!(visible(&out).matches("## ").count(), 2);
    }

    #[test]
    fn a_note_heading_and_its_subheadings_are_coloured_apart() {
        // Two levels is all a note needs, and telling them apart is what stops a
        // long body reading as one block.
        let theme = Theme::default();
        let out = coloured("## [ ] title\n\n### section\n");
        assert!(out.contains(&fg(theme.accent)), "the note heading");
        assert!(out.contains(&fg(theme.session)), "the subheading");
    }

    #[test]
    fn a_done_note_reads_as_done_in_the_popup_too() {
        let theme = Theme::default();
        let done = coloured("## [x] finished\n");
        assert!(done.contains(&fg(theme.muted)));
        assert!(!done.contains(&fg(theme.accent)), "not the open accent");

        let open = coloured("## [ ] not finished\n");
        assert!(open.contains(&fg(theme.accent)));
    }

    #[test]
    fn the_origin_comment_is_present_but_recedes() {
        let theme = Theme::default();
        let out = coloured("<!-- t=1770000000 from=work:2 -->\n");
        assert!(out.contains(&fg(theme.muted)));
        assert!(visible(&out).contains("from=work:2"), "still readable");
    }

    #[test]
    fn inline_code_is_coloured_and_a_stray_backtick_is_left_alone() {
        // A note is far more likely to contain one loose backtick than to have
        // meant to open a span and never closed it.
        let theme = Theme::default();
        assert!(coloured("a `span` here\n").contains(&fg(theme.branch)));
        let stray = coloured("a lone ` backtick\n");
        assert!(!stray.contains(&fg(theme.branch)));
        assert_eq!(visible(&stray), "a lone ` backtick\n");
    }

    #[test]
    fn a_heading_keeps_its_colour_either_side_of_a_code_span() {
        // The panel highlights inline code in a title; the popup showing the same
        // title without it would read as a different program.
        let theme = Theme::default();
        let out = coloured("## [ ] should `##` require a checkbox?\n");
        assert!(out.contains(&fg(theme.branch)), "the code span");
        assert!(out.contains(&fg(theme.accent)), "and the words around it");
        assert_eq!(visible(&out), "## [ ] should `##` require a checkbox?\n");
    }

    #[test]
    fn the_hashes_recede_so_the_title_leads() {
        // Markup at the same weight as the content is markup competing with it.
        let theme = Theme::default();
        let out = coloured("### The commits\n");
        assert!(out.starts_with(&format!("{}### ", fg(theme.muted))), "{out:?}");
    }

    #[test]
    fn emphasis_needs_a_non_space_after_the_opener() {
        // Otherwise a glob or a multiplication italicises the middle of a
        // sentence — `a * b * c` is a line a note about shell will really have.
        let plain = coloured("run a * b * c through it\n");
        assert!(!plain.contains(ITALIC), "{plain:?}");
        assert!(coloured("a *real* emphasis\n").contains(ITALIC));
        assert!(coloured("and **strong** too\n").contains(BOLD));
    }

    #[test]
    fn a_bold_span_is_not_read_as_two_empty_italics() {
        let out = coloured("**strong**\n");
        assert!(out.contains(BOLD));
        assert_eq!(visible(&out), "**strong**\n");
    }

    #[test]
    fn asterisks_inside_a_code_span_stay_literal() {
        let out = coloured("the glob `*.rs` matches\n");
        assert!(!out.contains(ITALIC), "{out:?}");
        assert_eq!(visible(&out), "the glob `*.rs` matches\n");
    }

    #[test]
    fn list_markers_are_picked_out_of_both_kinds_of_list() {
        let theme = Theme::default();
        for line in ["- bullet\n", "* bullet\n", "+ bullet\n", "12. ordered\n"] {
            assert!(
                coloured(line).contains(&fg(theme.accent)),
                "no marker found in {line:?}"
            );
        }
        // A bare number is not a list, and neither is a hyphenated word.
        assert!(!coloured("2026 was a year\n").contains(&fg(theme.accent)));
        assert!(!coloured("well-known\n").contains(&fg(theme.accent)));
    }

    #[test]
    fn an_indented_list_marker_still_counts() {
        assert!(coloured("    - nested\n").contains(&fg(Theme::default().accent)));
        assert_eq!(visible(&coloured("    - nested\n")), "    - nested\n");
    }

    #[test]
    fn every_span_closes_the_attributes_it_opened() {
        // Bold or a colour left in force would bleed down the popup into the rest
        // of the note.
        let out = coloured("## [ ] title\n**bold** and `code`\n");
        for line in out.lines().filter(|line| line.contains('\x1b')) {
            assert!(line.ends_with(RESET), "unterminated: {line:?}");
        }
    }

    #[test]
    fn a_theme_override_reaches_the_popup() {
        // The popup is part of the plugin, not a separate program that happens to
        // sit next to it.
        let theme = Theme {
            accent: Color::Rgb(0x89, 0xb4, 0xfa),
            ..Theme::default()
        };
        let out = note("## [ ] title\n", &theme, true);
        assert!(out.contains("\x1b[38;2;137;180;250m"), "{out:?}");
    }

    #[test]
    fn an_empty_note_produces_nothing_rather_than_a_stray_reset() {
        assert_eq!(coloured(""), "");
    }

    #[test]
    fn when_parses_the_usual_three_and_rejects_a_typo() {
        assert_eq!(When::parse("auto"), Some(When::Auto));
        assert_eq!(When::parse("always"), Some(When::Always));
        assert_eq!(When::parse("never"), Some(When::Never));
        assert_eq!(When::parse("sometimes"), None);
    }

    #[test]
    fn auto_follows_the_terminal_and_the_other_two_do_not() {
        // `note show` is piped to a pager by the popup, so an IsTerminal check
        // alone would disable colour in exactly the case that wants it.
        assert!(When::Auto.enabled(true));
        assert!(!When::Auto.enabled(false));
        assert!(When::Always.enabled(false));
        assert!(!When::Never.enabled(true));
    }
}
