//! The context card: where the selected pane is working, on the web.
//!
//! ```text
//!  flugo                  FLU-88
//!  ⎇ pete/stack-ports
//!  ● #142 Derive ports from the…
//! ──────────────────────────────
//! ```
//!
//! Folder and ticket, branch, pull request — each a terminal hyperlink where there
//! is somewhere to go. The card is always [`HEIGHT`] rows, whatever it has to say,
//! because the list below it is what you navigate: a card that grew a row when you
//! moved onto a pane with a PR would shift every row under the cursor you just
//! moved.
//!
//! Pure, like [`super::rows`]: a pane and what is known about it in, lines and
//! link spans out.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::forge::{PrState, RepoContext, TicketConfig};
use crate::model::PaneInfo;
use crate::ui::text::{pad_to, truncate, width};
use crate::ui::theme::Theme;

/// Rows the card occupies, rule included.
pub const HEIGHT: u16 = 4;

/// Below this many rows in the pane the card gives its space back to the list.
/// A sidebar this short is being used as a status strip, not a dashboard.
pub const MIN_PANE_HEIGHT: u16 = 16;

/// A clickable run of cells, in card-local coordinates.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Link {
    pub row: u16,
    pub col: u16,
    pub width: u16,
    pub url: String,
}

#[derive(Clone, Debug, Default)]
pub struct Card {
    pub lines: Vec<Line<'static>>,
    /// Plain text of each line, for the change fingerprint.
    pub plain: Vec<String>,
    pub links: Vec<Link>,
}

/// What the card knows about the PR for the selected pane.
#[derive(Clone, Copy, Debug)]
pub enum Lookup<'a> {
    /// Asked, not yet answered.
    Pending,
    Done(&'a RepoContext),
}

/// One styled run of a row, optionally a link.
struct Segment {
    text: String,
    style: Style,
    url: Option<String>,
}

impl Segment {
    fn new(text: impl Into<String>, style: Style) -> Self {
        Self { text: text.into(), style, url: None }
    }

    fn link(mut self, url: Option<String>) -> Self {
        self.url = url;
        self
    }
}

pub fn build(
    pane: Option<&PaneInfo>,
    lookup: Lookup,
    tickets: &TicketConfig,
    total_width: usize,
    theme: &Theme,
) -> Card {
    let mut card = Card::default();
    let muted = Style::default().fg(theme.muted);

    let Some(pane) = pane else {
        for _ in 0..HEIGHT - 1 {
            push(&mut card, total_width, Vec::new(), Vec::new());
        }
        push_rule(&mut card, total_width, theme);
        return card;
    };

    let context = match lookup {
        Lookup::Done(context) => Some(context),
        Lookup::Pending => None,
    };
    let pr = context.and_then(|c| c.pr.as_ref());

    // Row 1: folder, and the ticket pinned right where the eye lands after it.
    let folder = folder_name(&pane.current_path);
    let ticket = tickets.find(
        [pane.branch.as_str()]
            .into_iter()
            .chain(pr.map(|pr| pr.title.as_str())),
    );
    let right = match ticket {
        Some(key) => {
            let url = tickets.link(&key);
            let style = Style::default().fg(theme.badge_plan);
            vec![Segment::new(key, if url.is_some() { style.add_modifier(Modifier::UNDERLINED) } else { style }).link(url)]
        }
        None => Vec::new(),
    };
    let folder_url = context.and_then(|c| c.web.clone());
    let left = vec![Segment::new(
        folder,
        Style::default()
            .fg(theme.text)
            .add_modifier(Modifier::BOLD),
    )
    .link(folder_url)];
    push(&mut card, total_width, left, right);

    // Row 2: branch, linked to its PR when there is one.
    let branch_row = if pane.branch.is_empty() {
        vec![Segment::new("not a git repo", muted)]
    } else {
        let url = context.and_then(|c| c.branch_url(&pane.branch));
        let mut label = pane.branch.clone();
        if !pane.worktree.is_empty() {
            label = format!("{label} ~{}", pane.worktree);
        }
        vec![
            Segment::new("⎇ ", Style::default().fg(theme.branch)),
            Segment::new(label, link_style(theme.branch, url.is_some())).link(url),
        ]
    };
    push(&mut card, total_width, branch_row, Vec::new());

    // Row 3: the pull request, or why there is none to show.
    let pr_row = match (pane.branch.is_empty(), lookup, pr) {
        (true, _, _) => Vec::new(),
        (false, Lookup::Pending, _) => vec![Segment::new("… looking for a PR", muted)],
        (false, Lookup::Done(_), None) => vec![Segment::new("no pull request", muted)],
        (false, Lookup::Done(_), Some(pr)) => {
            let (glyph, color) = pr_glyph(pr.state, theme);
            let url = Some(pr.url.clone());
            vec![
                Segment::new(format!("{glyph} "), Style::default().fg(color)),
                Segment::new(format!("#{}", pr.number), link_style(theme.accent, true))
                    .link(url.clone()),
                Segment::new(" ", Style::default()),
                Segment::new(pr.title.clone(), Style::default().fg(theme.text)).link(url),
            ]
        }
    };
    push(&mut card, total_width, pr_row, Vec::new());

    push_rule(&mut card, total_width, theme);
    card
}

fn link_style(color: Color, linked: bool) -> Style {
    let style = Style::default().fg(color);
    if linked { style.add_modifier(Modifier::UNDERLINED) } else { style }
}

fn pr_glyph(state: PrState, theme: &Theme) -> (&'static str, Color) {
    match state {
        PrState::Open => ("●", theme.idle),
        PrState::Draft => ("○", theme.muted),
        PrState::Merged => ("✓", theme.badge_plan),
        PrState::Closed => ("✕", theme.blocked),
    }
}

/// The directory's own name, which is what you'd call the project out loud.
fn folder_name(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rsplit('/').next() {
        Some(name) if !name.is_empty() => name.to_owned(),
        _ => path.to_owned(),
    }
}

/// Lay out one row: a one-cell margin, `left` truncated to fit, `right` pinned to
/// the right edge. Exactly `total_width` cells, links recorded where they landed.
///
/// `right` wins the space it needs — it is a short key — and `left` gets the rest,
/// with the *last* left segment absorbing any truncation, since that is the long
/// free text (a branch, a PR title) rather than a glyph or a number.
fn push(card: &mut Card, total_width: usize, left: Vec<Segment>, right: Vec<Segment>) {
    let row = card.lines.len() as u16;
    let right_width: usize = right.iter().map(|s| width(&s.text)).sum();
    let right_room = if right_width > 0 { right_width + 1 } else { 0 };
    let margin = 1.min(total_width);
    let mut left_budget = total_width.saturating_sub(margin + right_room + 1);

    let mut spans = vec![Span::raw(" ".repeat(margin))];
    let mut plain = " ".repeat(margin);
    let mut col = margin;

    let mut place = |segment: Segment, budget: usize, spans: &mut Vec<Span<'static>>, plain: &mut String, col: &mut usize| {
        let text = truncate(&segment.text, budget);
        let w = width(&text);
        if w == 0 {
            return 0;
        }
        if let Some(url) = segment.url {
            card.links.push(Link { row, col: *col as u16, width: w as u16, url });
        }
        plain.push_str(&text);
        spans.push(Span::styled(text, segment.style));
        *col += w;
        w
    };

    for segment in left {
        let used = place(segment, left_budget, &mut spans, &mut plain, &mut col);
        left_budget -= used;
    }

    if right_width > 0 && total_width >= col + right_room {
        let gap = pad_to(col + right_width + 1, total_width);
        plain.push_str(&gap);
        spans.push(Span::raw(gap.clone()));
        col += gap.len();
        for segment in right {
            place(segment, right_width, &mut spans, &mut plain, &mut col);
        }
    }

    let tail = pad_to(col, total_width);
    plain.push_str(&tail);
    spans.push(Span::raw(tail));
    card.lines.push(Line::from(spans));
    card.plain.push(plain);
}

fn push_rule(card: &mut Card, total_width: usize, theme: &Theme) {
    let rule = "─".repeat(total_width);
    card.plain.push(rule.clone());
    card.lines.push(Line::from(Span::styled(rule, Style::default().fg(theme.unknown))));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::PullRequest;
    use crate::model::AgentStatus;

    fn pane(path: &str, branch: &str) -> PaneInfo {
        PaneInfo {
            pane_id: "%1".into(),
            window_id: "@1".into(),
            pane_index: "1".into(),
            pane_active: true,
            current_command: "claude".into(),
            current_path: path.into(),
            title: String::new(),
            pane_pid: None,
            status: AgentStatus::default(),
            branch: branch.into(),
            worktree: String::new(),
        }
    }

    fn with_pr(title: &str) -> RepoContext {
        RepoContext {
            web: Some("https://github.com/o/flugo".into()),
            pr: Some(PullRequest {
                number: 142,
                title: title.into(),
                url: "https://github.com/o/flugo/pull/142".into(),
                state: PrState::Open,
            }),
        }
    }

    fn tickets() -> TicketConfig {
        TicketConfig::new(Some("FLU"), Some("https://t/{key}"))
    }

    #[test]
    fn every_line_is_exactly_the_width_at_every_width() {
        // Invariant 5: one cell over wraps and shifts the whole list.
        let ctx = with_pr("Derive ports from the branch so worktrees never collide 漢字");
        let p = pane("/home/pete/github/flugo", "pete/flu-88-stack-ports-with-a-long-name");
        for w in 0..60 {
            for lookup in [Lookup::Pending, Lookup::Done(&ctx)] {
                let card = build(Some(&p), lookup, &tickets(), w, &Theme::default());
                for line in &card.plain {
                    assert_eq!(width(line), w, "width {w}: {line:?}");
                }
                for link in &card.links {
                    assert!((link.col + link.width) as usize <= w, "link past the edge at {w}");
                }
            }
        }
    }

    #[test]
    fn the_card_is_the_same_height_whatever_it_knows() {
        // The list sits under it; a card that changed height would move the cursor.
        let ctx = with_pr("t");
        let empty = RepoContext::default();
        for (p, lookup) in [
            (None, Lookup::Pending),
            (Some(pane("/x", "")), Lookup::Done(&empty)),
            (Some(pane("/x", "b")), Lookup::Pending),
            (Some(pane("/x", "b")), Lookup::Done(&ctx)),
        ] {
            let card = build(p.as_ref(), lookup, &tickets(), 30, &Theme::default());
            assert_eq!(card.lines.len(), HEIGHT as usize);
        }
    }

    #[test]
    fn folder_branch_pr_and_ticket_are_each_linked() {
        let ctx = with_pr("Derive ports");
        let card = build(
            Some(&pane("/home/pete/flugo", "pete/flu-88-ports")),
            Lookup::Done(&ctx),
            &tickets(),
            40,
            &Theme::default(),
        );
        assert!(card.plain[0].starts_with(" flugo"));
        assert!(card.plain[0].trim_end().ends_with("FLU-88"));
        assert!(card.plain[2].contains("#142 Derive ports"));
        let urls: Vec<&str> = card.links.iter().map(|l| l.url.as_str()).collect();
        assert!(urls.contains(&"https://github.com/o/flugo"));
        assert!(urls.contains(&"https://t/FLU-88"));
        // The branch goes to the PR, not the tree, once a PR exists.
        let branch = card.links.iter().find(|l| l.row == 1).unwrap();
        assert_eq!(branch.url, "https://github.com/o/flugo/pull/142");
    }

    #[test]
    fn a_link_covers_exactly_the_text_it_names() {
        let ctx = with_pr("Derive ports");
        let card = build(
            Some(&pane("/r/flugo", "main")),
            Lookup::Done(&ctx),
            &TicketConfig::default(),
            40,
            &Theme::default(),
        );
        for link in &card.links {
            let line = &card.plain[link.row as usize];
            let covered: String = line.chars().skip(link.col as usize).take(link.width as usize).collect();
            assert!(!covered.starts_with(' ') && !covered.ends_with(' '), "{covered:?}");
        }
    }

    #[test]
    fn a_ticket_in_the_pr_title_counts_when_the_branch_has_none() {
        let ctx = with_pr("FLU-91: ports");
        let card = build(
            Some(&pane("/r/flugo", "pete/ports")),
            Lookup::Done(&ctx),
            &tickets(),
            40,
            &Theme::default(),
        );
        assert!(card.plain[0].contains("FLU-91"));
    }

    #[test]
    fn an_unconfigured_ticket_shows_but_does_not_link() {
        let card = build(
            Some(&pane("/r/flugo", "FLU-1-x")),
            Lookup::Pending,
            &TicketConfig::new(None, None),
            40,
            &Theme::default(),
        );
        assert!(card.plain[0].contains("FLU-1"));
        assert!(card.links.iter().all(|l| l.row != 0 || !l.url.contains("FLU")));
    }

    #[test]
    fn the_pr_row_says_why_it_is_empty() {
        let none = RepoContext::default();
        let pending = build(Some(&pane("/r", "b")), Lookup::Pending, &tickets(), 30, &Theme::default());
        let done = build(Some(&pane("/r", "b")), Lookup::Done(&none), &tickets(), 30, &Theme::default());
        assert!(pending.plain[2].contains("looking"));
        assert!(done.plain[2].contains("no pull request"));
    }
}
