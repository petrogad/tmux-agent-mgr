//! Where a branch lives on the web: the repo's page, its pull request, and the
//! ticket it is for. Feeds the context card at the top of the sidebar.
//!
//! The PR comes from `gh`, which is a network round trip — routinely a second,
//! occasionally much worse. So none of this runs on the collector thread either:
//! [`crate::app::worker`] gives it a thread of its own, asks only about the pane
//! the cursor is on, and caches every answer (including "no PR") for [`TTL`].
//!
//! Tickets are not looked up anywhere. A key is read out of the branch name or the
//! PR title and turned into a link with a URL template, which is all a link needs
//! and costs nothing.

use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long a lookup is reused. A PR opened a minute ago showing up a minute late
/// is fine; a `gh` call every second per sidebar is not.
const TTL: Duration = Duration::from_secs(60);

/// The web side of one `(path, branch)`.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct RepoContext {
    /// `https://host/owner/repo`, or `None` when there is no usable remote.
    pub web: Option<String>,
    /// The PR whose head is this branch, open or not.
    pub pr: Option<PullRequest>,
}

impl RepoContext {
    /// Where the branch name should take you: its PR when there is one, the branch
    /// itself on the forge when there is not.
    pub fn branch_url(&self, branch: &str) -> Option<String> {
        if let Some(pr) = &self.pr {
            return Some(pr.url.clone());
        }
        let web = self.web.as_ref()?;
        (!branch.is_empty()).then(|| format!("{web}/tree/{branch}"))
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub state: PrState,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PrState {
    Open,
    Draft,
    Merged,
    Closed,
}

/// TTL cache over [`lookup`]. Lives on the context thread, so no locking.
#[derive(Default)]
pub struct ContextCache {
    entries: HashMap<(String, String), (Instant, RepoContext)>,
}

impl ContextCache {
    /// Look up `(path, branch)`, reusing a recent answer when there is one.
    pub fn get(&mut self, path: &str, branch: &str) -> RepoContext {
        self.get_at(path, branch, Instant::now(), lookup)
    }

    /// Testable core: the clock and the resolver are injected.
    fn get_at(
        &mut self,
        path: &str,
        branch: &str,
        now: Instant,
        resolve: impl FnOnce(&str, &str) -> RepoContext,
    ) -> RepoContext {
        if let Some((fetched_at, context)) = self.entries.get(&(path.to_owned(), branch.to_owned()))
            && now.duration_since(*fetched_at) < TTL
        {
            return context.clone();
        }
        let context = resolve(path, branch);
        // Nothing else ever evicts, and a sidebar lives for days; a bound this
        // generous never bites in practice and still keeps the map finite.
        if self.entries.len() >= 256 {
            self.entries.retain(|_, (at, _)| now.duration_since(*at) < TTL);
        }
        self.entries
            .insert((path.to_owned(), branch.to_owned()), (now, context.clone()));
        context
    }
}

/// Ask git for the remote and `gh` for the PR. Two subprocesses, the second one
/// skipped when there is no forge to ask.
fn lookup(path: &str, branch: &str) -> RepoContext {
    let web = run("git", path, &["remote", "get-url", "origin"])
        .as_deref()
        .and_then(web_url);
    let pr = match &web {
        // No branch argument: bare `gh pr view` resolves the checkout's own push
        // target, which survives a local branch named differently from its remote.
        Some(_) if !branch.is_empty() => run(
            "gh",
            path,
            &["pr", "view", "--json", "number,title,url,state,isDraft"],
        )
        .as_deref()
        .and_then(parse_pr),
        _ => None,
    };
    RepoContext { web, pr }
}

/// The browsable URL for a git remote.
///
/// Handles the three shapes remotes come in — scp-style `git@host:owner/repo`,
/// `ssh://` and `https://` — and drops credentials, ports and the `.git` suffix.
/// Anything else (a local path, a `file://` remote) has no web page.
pub fn web_url(remote: &str) -> Option<String> {
    let remote = remote.trim();
    let (host, path) = if let Some(rest) = remote
        .strip_prefix("https://")
        .or_else(|| remote.strip_prefix("http://"))
        .or_else(|| remote.strip_prefix("ssh://"))
        .or_else(|| remote.strip_prefix("git://"))
    {
        rest.split_once('/')?
    } else {
        // scp-style. A colon followed by a slash would be a URL we did not
        // recognise above, not a host:path pair.
        let (host, path) = remote.split_once(':')?;
        if path.starts_with('/') || host.contains('/') {
            return None;
        }
        (host, path)
    };
    let host = host.rsplit('@').next()?;
    let host = host.split(':').next()?;
    let path = path.trim_end_matches('/').trim_end_matches(".git");
    if host.is_empty() || path.split('/').filter(|part| !part.is_empty()).count() < 2 {
        return None;
    }
    Some(format!("https://{host}/{path}"))
}

/// Parse `gh pr view --json number,title,url,state,isDraft`.
fn parse_pr(json: &str) -> Option<PullRequest> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let state = match (value["state"].as_str()?, value["isDraft"].as_bool()) {
        ("OPEN", Some(true)) => PrState::Draft,
        ("OPEN", _) => PrState::Open,
        ("MERGED", _) => PrState::Merged,
        ("CLOSED", _) => PrState::Closed,
        _ => return None,
    };
    Some(PullRequest {
        number: value["number"].as_u64()?,
        title: value["title"].as_str()?.trim().to_owned(),
        url: value["url"].as_str()?.to_owned(),
        state,
    })
}

fn run(program: &str, path: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program)
        .args(args)
        .current_dir(path)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        // gh otherwise pipes through a pager or asks to upgrade when it thinks a
        // human is watching.
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("NO_COLOR", "1")
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|out| !out.is_empty())
}

// ─── tickets ─────────────────────────────────────────────────────────

/// How to find a ticket key and where to send it, from
/// `@agent_mgr_ticket_prefixes` and `@agent_mgr_ticket_url`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TicketConfig {
    /// Known project keys, uppercased. Empty means "any `ABC-123`-shaped key".
    pub prefixes: Vec<String>,
    /// URL with `{key}` where the key goes. `None` shows the key unlinked.
    pub url: Option<String>,
}

impl TicketConfig {
    pub fn new(prefixes: Option<&str>, url: Option<&str>) -> Self {
        Self {
            prefixes: prefixes
                .unwrap_or_default()
                .split(|c: char| c == ',' || c.is_whitespace())
                .filter(|p| !p.is_empty())
                .map(str::to_ascii_uppercase)
                .collect(),
            url: url.filter(|u| u.contains("{key}")).map(str::to_owned),
        }
    }

    pub fn link(&self, key: &str) -> Option<String> {
        Some(self.url.as_ref()?.replace("{key}", key))
    }

    /// The first ticket key in any of `sources`, tried in order.
    pub fn find<'a>(&self, sources: impl IntoIterator<Item = &'a str>) -> Option<String> {
        sources.into_iter().find_map(|text| self.find_in(text))
    }

    /// A key is `PREFIX-123` with word boundaries on both ends.
    ///
    /// With prefixes configured, case is ignored — branches are usually lowercase,
    /// `grps-3458-peek-page`, and the prefix list is what keeps `fix-2` from
    /// reading as a ticket. Without them only an all-caps key counts, for the same
    /// reason: a lowercase `v2-3` or `utf-8` in a branch name is never a ticket.
    fn find_in(&self, text: &str) -> Option<String> {
        let bytes = text.as_bytes();
        let is_word = |b: u8| b.is_ascii_alphanumeric();
        let mut start = 0;
        while start < bytes.len() {
            let boundary = start == 0 || !is_word(bytes[start - 1]);
            if boundary && bytes[start].is_ascii_alphabetic() {
                let prefix_end = start
                    + bytes[start..]
                        .iter()
                        .take_while(|b| b.is_ascii_alphanumeric())
                        .count();
                let digits_start = prefix_end + 1;
                let digits = bytes
                    .get(digits_start..)
                    .unwrap_or_default()
                    .iter()
                    .take_while(|b| b.is_ascii_digit())
                    .count();
                let end = digits_start + digits;
                let closed = bytes.get(end).is_none_or(|&b| !is_word(b));
                if bytes.get(prefix_end) == Some(&b'-') && digits > 0 && closed {
                    let prefix = &text[start..prefix_end];
                    if self.accepts(prefix) {
                        return Some(format!(
                            "{}-{}",
                            prefix.to_ascii_uppercase(),
                            &text[digits_start..end]
                        ));
                    }
                }
                start = prefix_end.max(start + 1);
                continue;
            }
            start += 1;
        }
        None
    }

    fn accepts(&self, prefix: &str) -> bool {
        if self.prefixes.is_empty() {
            prefix.len() >= 2
                && prefix.as_bytes()[0].is_ascii_uppercase()
                && prefix.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        } else {
            self.prefixes.iter().any(|p| p.eq_ignore_ascii_case(prefix))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn every_remote_shape_maps_to_the_same_page() {
        for remote in [
            "git@github.com:petrogad/tmux-agent-mgr.git",
            "ssh://git@github.com/petrogad/tmux-agent-mgr.git",
            "ssh://git@github.com:22/petrogad/tmux-agent-mgr",
            "https://github.com/petrogad/tmux-agent-mgr.git",
            "https://user:token@github.com/petrogad/tmux-agent-mgr/",
        ] {
            assert_eq!(
                web_url(remote).as_deref(),
                Some("https://github.com/petrogad/tmux-agent-mgr"),
                "{remote}"
            );
        }
    }

    #[test]
    fn a_remote_with_no_web_page_has_no_url() {
        // A local path or a bare host must not become a link to nowhere.
        assert_eq!(web_url("/srv/git/repo.git"), None);
        assert_eq!(web_url("file:///srv/git/repo.git"), None);
        assert_eq!(web_url("https://github.com/onlyowner"), None);
        assert_eq!(web_url(""), None);
    }

    #[test]
    fn pr_json_reads_draft_as_its_own_state() {
        let pr = parse_pr(
            r#"{"isDraft":true,"number":142,"state":"OPEN","title":" Derive ports ","url":"https://github.com/o/r/pull/142"}"#,
        )
        .unwrap();
        assert_eq!(pr.number, 142);
        assert_eq!(pr.title, "Derive ports");
        assert_eq!(pr.state, PrState::Draft);
        let merged = parse_pr(r#"{"isDraft":false,"number":1,"state":"MERGED","title":"t","url":"u"}"#);
        assert_eq!(merged.unwrap().state, PrState::Merged);
    }

    #[test]
    fn unrecognised_pr_json_is_no_pr_rather_than_a_guess() {
        assert_eq!(parse_pr("no pull requests found"), None);
        assert_eq!(parse_pr(r#"{"number":1,"state":"WEIRD","title":"t","url":"u"}"#), None);
    }

    #[test]
    fn the_branch_links_to_its_pr_before_its_tree() {
        let mut ctx = RepoContext {
            web: Some("https://github.com/o/r".into()),
            pr: None,
        };
        assert_eq!(
            ctx.branch_url("pete/ports").as_deref(),
            Some("https://github.com/o/r/tree/pete/ports")
        );
        ctx.pr = Some(PullRequest {
            number: 7,
            title: "t".into(),
            url: "https://github.com/o/r/pull/7".into(),
            state: PrState::Open,
        });
        assert_eq!(ctx.branch_url("pete/ports").as_deref(), Some("https://github.com/o/r/pull/7"));
    }

    #[test]
    fn a_negative_answer_is_cached_too() {
        // "no PR" is the common answer for a fresh branch; asking gh again every
        // pass for it is exactly the cost the cache exists to avoid.
        let mut cache = ContextCache::default();
        let calls = Cell::new(0);
        let resolve = |_: &str, _: &str| {
            calls.set(calls.get() + 1);
            RepoContext::default()
        };
        let t0 = Instant::now();
        cache.get_at("/r", "b", t0, resolve);
        cache.get_at("/r", "b", t0 + Duration::from_secs(5), resolve);
        assert_eq!(calls.get(), 1);
        cache.get_at("/r", "b", t0 + TTL, resolve);
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn without_prefixes_only_an_uppercase_key_is_a_ticket() {
        let cfg = TicketConfig::new(None, None);
        assert_eq!(
            cfg.find(["GRPS-3458: Add native SDUI CustomForm"]).as_deref(),
            Some("GRPS-3458")
        );
        assert_eq!(cfg.find(["pete/utf-8-fix", "bump v2-3"]), None);
        assert_eq!(cfg.find(["A-1"]), None, "a one-letter prefix is too weak");
    }

    #[test]
    fn configured_prefixes_match_a_lowercase_branch() {
        let cfg = TicketConfig::new(Some("grps, FLU"), None);
        assert_eq!(cfg.find(["grps-3460-peek-page"]).as_deref(), Some("GRPS-3460"));
        assert_eq!(cfg.find(["pete/flu-88-ports"]).as_deref(), Some("FLU-88"));
        // Not a configured project, however ticket-shaped.
        assert_eq!(cfg.find(["ABC-12"]), None);
    }

    #[test]
    fn a_key_needs_word_boundaries() {
        let cfg = TicketConfig::new(Some("GRPS"), None);
        assert_eq!(cfg.find(["xgrps-12"]), None);
        assert_eq!(cfg.find(["grps-12a"]), None);
        assert_eq!(cfg.find(["grps-"]), None);
        assert_eq!(cfg.find(["(grps-12)"]).as_deref(), Some("GRPS-12"));
    }

    #[test]
    fn sources_are_tried_in_order() {
        let cfg = TicketConfig::new(None, None);
        assert_eq!(cfg.find(["ABC-1 first", "XYZ-2"]).as_deref(), Some("ABC-1"));
        assert_eq!(cfg.find(["nothing", "XYZ-2"]).as_deref(), Some("XYZ-2"));
    }

    #[test]
    fn a_url_template_without_a_key_slot_is_ignored() {
        // Linking every ticket to the same page would be worse than no link.
        assert_eq!(TicketConfig::new(None, Some("https://jira/browse/")).url, None);
        let cfg = TicketConfig::new(None, Some("https://jira/browse/{key}"));
        assert_eq!(cfg.link("GRPS-1").as_deref(), Some("https://jira/browse/GRPS-1"));
    }
}
