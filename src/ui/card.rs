//! The context card: where the focused pane is working, on the web.
//!
//! ```text
//! ──────────────────────────────
//!  petrogad/flugo · ⎇ ports
//!  ▸ ~/github/flugo-ports/apps
//!  ● #142 Derive ports fr…  FLU-88
//! ```
//!
//! Pinned above the footer, and it follows **tmux focus**, not the cursor: it
//! changes when you actually switch panes, never while you `j`/`k` through the
//! list. Repo and branch, then the working directory in full, then the work: the PR and the ticket it
//! is for — a Jira-style key from the branch or PR title, else the GitHub issue
//! the PR says it closes. Each is a link where there is somewhere to go. The card is always [`HEIGHT`] rows whatever it has to say, so
//! the list above it never changes height.
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
    push_rule(&mut card, total_width, theme);

    let Some(pane) = pane else {
        for _ in 1..HEIGHT {
            push(&mut card, total_width, Vec::new(), Vec::new());
        }
        return card;
    };

    let context = match lookup {
        Lookup::Done(context) => Some(context),
        Lookup::Pending => None,
    };
    let pr = context.and_then(|c| c.pr.as_ref());

    // Row 1: the repo as the remote names it, and the branch. `owner/` is muted
    // so the name reads first, and is the first thing given up for room.
    let repo_url = context.and_then(|c| c.web.clone());
    let branch = (!pane.branch.is_empty()).then(|| {
        let url = context.and_then(|c| c.branch_url(&pane.branch));
        vec![
            Segment::new(" · ", muted),
            Segment::new(format!("⎇ {}", pane.branch), link_style(theme.branch, url.is_some()))
                .link(url),
        ]
    });
    let bold = |color| link_style(color, true).add_modifier(Modifier::BOLD);
    let mut repo_row = match context.and_then(|c| c.web.as_deref()) {
        Some(web) => {
            let mut parts = web.rsplit('/');
            let name = parts.next().unwrap_or_default().to_owned();
            let owner = format!("{}/", parts.next().unwrap_or_default());
            let wanted = width(&owner) + width(&name) + 1 + branch.as_ref().map_or(0, |b| {
                b.iter().map(|s| width(&s.text)).sum::<usize>().min(16)
            });
            let mut row = Vec::new();
            if wanted <= total_width {
                row.push(Segment::new(owner, muted).link(repo_url.clone()));
            }
            row.push(Segment::new(name, bold(theme.text)).link(repo_url));
            row
        }
        None => vec![Segment::new(
            folder_name(&pane.current_path),
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        )],
    };
    repo_row.extend(branch.into_iter().flatten());
    push(&mut card, total_width, repo_row, Vec::new());

    // Row 2: the working directory, whole — trimmed from the left, since the
    // end of a path is the part that says where you are.
    let folder_url = context.and_then(|c| c.folder_url(&pane.branch, &pane.current_path));
    let path = truncate_left(&home_relative(&pane.current_path), total_width.saturating_sub(4));
    let place = vec![
        Segment::new("▸ ", Style::default().fg(theme.folder)),
        Segment::new(path, link_style(theme.folder, folder_url.is_some())).link(folder_url),
    ];
    push(&mut card, total_width, place, Vec::new());

    // Row 3: the pull request, or why there is none, with the ticket pinned right.
    let ticket = match tickets.find(
        [pane.branch.as_str()]
            .into_iter()
            .chain(pr.map(|pr| pr.title.as_str())),
    ) {
        Some(key) => {
            let url = tickets.link(&key);
            vec![Segment::new(key, link_style(theme.badge_plan, url.is_some())).link(url)]
        }
        None => match (pr.and_then(|pr| pr.closes), context) {
            (Some(issue), Some(context)) => {
                let url = context.issue_url(issue);
                vec![Segment::new(format!("◎ #{issue}"), link_style(theme.badge_plan, url.is_some())).link(url)]
            }
            _ => Vec::new(),
        },
    };
    let pr_row = match (pane.branch.is_empty(), lookup, pr) {
        (true, _, _) => vec![Segment::new("not a git repo", muted)],
        (false, Lookup::Pending, _) => vec![Segment::new("…", muted)],
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
    push(&mut card, total_width, pr_row, ticket);
    card
}

/// `path` with the home directory written as `~`.
fn home_relative(path: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let home = home.trim_end_matches('/');
    match path.strip_prefix(home) {
        Some(rest) if !home.is_empty() && (rest.is_empty() || rest.starts_with('/')) => {
            format!("~{rest}")
        }
        _ => path.to_owned(),
    }
}

/// Keep the *end* of `value` within `budget` cells, marking the cut with `…`.
fn truncate_left(value: &str, budget: usize) -> String {
    if width(value) <= budget {
        return value.to_owned();
    }
    if budget == 0 {
        return String::new();
    }
    let mut kept: Vec<char> = Vec::new();
    let mut used = 1; // the ellipsis
    for ch in value.chars().rev() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > budget {
            break;
        }
        kept.push(ch);
        used += w;
    }
    kept.push('…');
    kept.into_iter().rev().collect()
}

/// The link under a card-local cell, if any.
pub fn link_at(card: &Card, col: u16, row: u16) -> Option<&str> {
    card.links
        .iter()
        .find(|l| l.row == row && col >= l.col && col < l.col + l.width)
        .map(|l| l.url.as_str())
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
    card.lines.push(Line::from(Span::styled(rule, Style::default().fg(theme.rule))));
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
            toplevel: Some("/w/flugo-ports".into()),
            pr: Some(PullRequest {
                number: 142,
                title: title.into(),
                url: "https://github.com/o/flugo/pull/142".into(),
                state: PrState::Open,
                closes: None,
            }),
        }
    }

    fn tickets() -> TicketConfig {
        TicketConfig::new(Some("FLU"), Some("https://t/{key}"))
    }

    fn card_for(path: &str, branch: &str, ctx: &RepoContext, tickets: &TicketConfig) -> Card {
        build(Some(&pane(path, branch)), Lookup::Done(ctx), tickets, 44, &Theme::default())
    }

    /// The URL under the first cell of `needle` on `row`.
    fn url_at<'a>(card: &'a Card, row: usize, needle: &str) -> Option<&'a str> {
        let line = &card.plain[row];
        let col = width(&line[..line.find(needle)?]) as u16;
        link_at(card, col, row as u16)
    }

    #[test]
    fn every_line_is_exactly_the_width_at_every_width() {
        // Invariant 5: one cell over wraps and shifts the whole list.
        let mut ctx = with_pr("Derive ports from the branch so worktrees never collide 漢字");
        ctx.pr.as_mut().unwrap().closes = Some(12);
        let p = pane("/w/flugo-ports/apps/android/src", "pete/flu-88-stack-ports-with-a-long-name");
        for w in 0..60 {
            for lookup in [Lookup::Pending, Lookup::Done(&ctx)] {
                for t in [tickets(), TicketConfig::default()] {
                    let card = build(Some(&p), lookup, &t, w, &Theme::default());
                    for line in &card.plain {
                        assert_eq!(width(line), w, "width {w}: {line:?}");
                    }
                    for link in &card.links {
                        assert!((link.col + link.width) as usize <= w, "link past the edge at {w}");
                    }
                }
            }
        }
    }

    #[test]
    fn the_card_is_the_same_height_whatever_it_knows() {
        // The list sits on it; a card that changed height would move every row.
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
    fn rows_read_repo_then_place_then_work() {
        let card = card_for("/w/flugo-ports/apps", "pete/flu-88-ports", &with_pr("Derive ports"), &tickets());
        assert!(card.plain[0].starts_with('─'));
        assert_eq!(card.plain[1].trim_end(), " o/flugo · ⎇ pete/flu-88-ports");
        assert_eq!(card.plain[2].trim_end(), " ▸ /w/flugo-ports/apps");
        assert!(card.plain[3].starts_with(" ● #142 Derive ports"));
        assert!(card.plain[3].trim_end().ends_with("FLU-88"));
    }

    #[test]
    fn each_piece_links_where_it_names() {
        let card = card_for("/w/flugo-ports/apps", "pete/flu-88-ports", &with_pr("Derive ports"), &tickets());
        assert_eq!(url_at(&card, 1, "flugo"), Some("https://github.com/o/flugo"));
        assert_eq!(
            url_at(&card, 2, "/w"),
            Some("https://github.com/o/flugo/tree/pete/flu-88-ports/apps")
        );
        // The branch goes to the PR, not the tree, once a PR exists.
        assert_eq!(url_at(&card, 1, "⎇"), Some("https://github.com/o/flugo/pull/142"));
        assert_eq!(url_at(&card, 3, "#142"), Some("https://github.com/o/flugo/pull/142"));
        assert_eq!(url_at(&card, 3, "FLU-88"), Some("https://t/FLU-88"));
        assert_eq!(link_at(&card, 0, 1), None, "the margin is not a link");
        assert_eq!(link_at(&card, 1, 0), None, "the rule is not a link");
    }

    #[test]
    fn a_github_issue_fills_the_ticket_slot_when_there_is_no_key() {
        let mut ctx = with_pr("Derive ports");
        ctx.pr.as_mut().unwrap().closes = Some(12);
        let card = card_for("/w/flugo-ports", "pete/ports", &ctx, &tickets());
        assert!(card.plain[3].trim_end().ends_with("◎ #12"));
        assert_eq!(url_at(&card, 3, "◎"), Some("https://github.com/o/flugo/issues/12"));
        // A Jira-style key wins over the issue.
        let card = card_for("/w/flugo-ports", "pete/flu-3-ports", &ctx, &tickets());
        assert!(card.plain[3].trim_end().ends_with("FLU-3"));
    }

    #[test]
    fn a_link_covers_exactly_the_text_it_names() {
        let card = card_for("/w/flugo-ports", "main", &with_pr("Derive ports"), &TicketConfig::default());
        for link in &card.links {
            let line = &card.plain[link.row as usize];
            let covered: String = line.chars().skip(link.col as usize).take(link.width as usize).collect();
            assert!(!covered.starts_with(' ') && !covered.ends_with(' '), "{covered:?}");
        }
    }

    #[test]
    fn a_ticket_in_the_pr_title_counts_when_the_branch_has_none() {
        let card = card_for("/w/flugo-ports", "pete/ports", &with_pr("FLU-91: ports"), &tickets());
        assert!(card.plain[3].contains("FLU-91"));
    }

    #[test]
    fn an_unconfigured_ticket_shows_but_does_not_link() {
        let card = build(
            Some(&pane("/r/flugo", "FLU-1-x")),
            Lookup::Done(&RepoContext::default()),
            &TicketConfig::new(None, None),
            44,
            &Theme::default(),
        );
        assert!(card.plain[3].contains("FLU-1"));
        assert!(card.links.iter().all(|l| !l.url.contains("FLU")));
    }

    #[test]
    fn before_the_lookup_lands_the_card_falls_back_to_the_directory() {
        let card = build(Some(&pane("/w/flugo-ports/apps", "b")), Lookup::Pending, &tickets(), 30, &Theme::default());
        assert_eq!(card.plain[1].trim_end(), " apps · ⎇ b");
        assert_eq!(card.plain[2].trim_end(), " ▸ /w/flugo-ports/apps");
        assert_eq!(card.plain[3].trim(), "…");
    }

    #[test]
    fn a_long_path_keeps_its_end() {
        // The tail is where you are; the head is which disk it is on.
        assert_eq!(truncate_left("/a/very/long/path/apps/android", 13), "…apps/android");
        assert_eq!(truncate_left("/short", 12), "/short");
        // A wide glyph that would overrun is dropped whole, never split.
        assert_eq!(truncate_left("/a/漢字/x", 4), "…/x");
    }

    #[test]
    fn home_is_written_as_a_tilde() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(home_relative(&format!("{home}/github/x")), "~/github/x");
        assert_eq!(home_relative(&home), "~");
        assert_eq!(home_relative(&format!("{home}-other/x")), format!("{home}-other/x"));
    }

    #[test]
    fn a_narrow_card_gives_up_the_owner_before_the_name() {
        let mut ctx = with_pr("t");
        ctx.web = Some("https://github.com/a-rather-long-org/flugo".into());
        let card = build(Some(&pane("/w/flugo-ports", "ports")), Lookup::Done(&ctx), &tickets(), 24, &Theme::default());
        assert!(card.plain[1].starts_with(" flugo"), "{:?}", card.plain[1]);
    }

    #[test]
    fn the_pr_row_says_why_it_is_empty() {
        let none = RepoContext::default();
        let done = build(Some(&pane("/r", "b")), Lookup::Done(&none), &tickets(), 30, &Theme::default());
        assert!(done.plain[3].contains("no pull request"));
    }
}
