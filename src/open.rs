//! `open pr|ticket <path>`: the context card's links, from the keyboard.
//!
//! Bound to `prefix g` / `prefix G` in `agent-mgr.conf`, run against the pane you
//! are in, so it works from anywhere without going through the sidebar. Looks the
//! checkout up afresh rather than asking a sidebar — there may be none open, and
//! a keypress is rare enough that one `gh` call is the right price.
//!
//! The URL is opened where tmux runs (`open` / `xdg-open`). On a remote tmux that
//! is the wrong machine; the card's OSC 8 links are the answer there.

use crate::forge::{self, RepoContext, TicketConfig};
use crate::git::GitCache;
use crate::tmux;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    Pr,
    Ticket,
}

pub fn cmd_open(args: &[&str]) -> i32 {
    let (kind, path) = match args {
        ["pr", path] => (Kind::Pr, *path),
        ["ticket", path] => (Kind::Ticket, *path),
        _ => {
            eprintln!("usage: agent-mgr open pr|ticket <path>");
            return 2;
        }
    };
    let branch = GitCache::new().get(path).branch;
    let context = if branch.is_empty() {
        RepoContext::default()
    } else {
        forge::lookup(path, &branch)
    };
    let tickets = TicketConfig::new(
        tmux::global(tmux::CFG_TICKET_PREFIXES).as_deref(),
        tmux::global(tmux::CFG_TICKET_URL).as_deref(),
    );
    // Said in tmux's message line either way: run-shell -b discards our output,
    // and a key that silently does nothing reads as a key that is not bound.
    match target(kind, &branch, &context, &tickets) {
        Ok((url, label)) => {
            forge::open_url(&url);
            tmux::run_tmux_quiet(&["display-message", &format!("agent-mgr: opening {label}")]);
        }
        Err(why) => tmux::run_tmux_quiet(&["display-message", &format!("agent-mgr: {why}")]),
    }
    0
}

/// What a keypress opens, and how to name it in the message line; or why there
/// is nothing to open. Pure, so the fallbacks are testable without a browser.
///
/// `pr` falls back from the PR to the branch to the repo — "take me to this work
/// on GitHub", whatever exists of it. `ticket` follows the card: a Jira-style key,
/// else the issue the PR closes.
pub fn target(
    kind: Kind,
    branch: &str,
    context: &RepoContext,
    tickets: &TicketConfig,
) -> Result<(String, String), String> {
    let pr = context.pr.as_ref();
    match kind {
        Kind::Pr => {
            if let Some(pr) = pr {
                return Ok((pr.url.clone(), format!("#{}", pr.number)));
            }
            if let Some(url) = context.branch_url(branch) {
                return Ok((url, branch.to_owned()));
            }
            match &context.web {
                Some(web) => Ok((web.clone(), "the repo".to_owned())),
                None if branch.is_empty() => Err("not a git repo".to_owned()),
                None => Err("no GitHub remote for this repo".to_owned()),
            }
        }
        Kind::Ticket => {
            let key = tickets.find([branch].into_iter().chain(pr.map(|pr| pr.title.as_str())));
            if let Some(key) = key {
                return match tickets.link(&key) {
                    Some(url) => Ok((url, key)),
                    None => Err(format!("{key} — set @agent_mgr_ticket_url to open it")),
                };
            }
            match pr.and_then(|pr| pr.closes).and_then(|n| Some((context.issue_url(n)?, n))) {
                Some((url, n)) => Ok((url, format!("issue #{n}"))),
                None => Err("no ticket for this branch".to_owned()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::{PrState, PullRequest};

    fn ctx(pr: bool) -> RepoContext {
        RepoContext {
            web: Some("https://github.com/o/r".into()),
            toplevel: Some("/r".into()),
            pr: pr.then(|| PullRequest {
                number: 7,
                title: "FLU-9: ports".into(),
                url: "https://github.com/o/r/pull/7".into(),
                state: PrState::Open,
                closes: Some(3),
                checks: None,
                review: None,
            }),
        }
    }

    #[test]
    fn pr_falls_back_to_the_branch_then_the_repo() {
        let none = TicketConfig::default();
        assert_eq!(target(Kind::Pr, "b", &ctx(true), &none).unwrap().0, "https://github.com/o/r/pull/7");
        assert_eq!(target(Kind::Pr, "b", &ctx(false), &none).unwrap().0, "https://github.com/o/r/tree/b");
        assert_eq!(target(Kind::Pr, "", &ctx(false), &none).unwrap().0, "https://github.com/o/r");
        assert!(target(Kind::Pr, "", &RepoContext::default(), &none).is_err());
    }

    #[test]
    fn ticket_prefers_the_key_then_the_closed_issue() {
        let jira = TicketConfig::new(Some("FLU"), Some("https://j/{key}"));
        assert_eq!(target(Kind::Ticket, "b", &ctx(true), &jira).unwrap(), ("https://j/FLU-9".into(), "FLU-9".into()));
        let none = TicketConfig::new(Some("XYZ"), None);
        assert_eq!(target(Kind::Ticket, "b", &ctx(true), &none).unwrap().0, "https://github.com/o/r/issues/3");
        assert!(target(Kind::Ticket, "b", &ctx(false), &none).is_err());
    }

    #[test]
    fn a_key_with_nowhere_to_go_says_how_to_fix_that() {
        let unlinked = TicketConfig::new(Some("FLU"), None);
        let err = target(Kind::Ticket, "b", &ctx(true), &unlinked).unwrap_err();
        assert!(err.contains("@agent_mgr_ticket_url"), "{err}");
    }
}
