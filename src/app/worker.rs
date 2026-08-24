//! The background collector thread.
//!
//! Every tmux and git subprocess this plugin runs while the TUI is open happens
//! here. The UI thread does nothing but read from a channel, hash lines, and
//! occasionally draw — so a slow `git` on a network mount or a busy tmux server
//! can never stall input or leave a half-painted frame on screen.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::git::GitCache;
use crate::model::SessionGroup;
use crate::notes::{self, NoteFile, Stamp};
use crate::preview::{self, PanePreview};
use crate::tmux;

use super::Msg;

/// How long between collections. Everything urgent arrives via
/// [`Worker::request_refresh`] or the SIGUSR1 flag instead, so this only has to be
/// fast enough for changes nobody told us about (a new window, an agent starting).
const INTERVAL: Duration = Duration::from_millis(1000);
/// Granularity at which a wake request is noticed while waiting. Small because a
/// tmux focus hook's SIGUSR1 is seen here, and this is then the whole of the delay
/// between pressing a jump key and the collection that answers it.
const WAKE_SLICE: Duration = Duration::from_millis(10);
/// How long a cached session order is reused when the session set is unchanged.
/// Bounds how long a re-ranked session takes to move in the list.
const ORDER_TTL: Duration = Duration::from_secs(5);

/// One collection: the tree, plus the preview for whichever window the UI asked
/// about. Sent together so the list and the preview on screen always describe the
/// same instant.
pub struct Snapshot {
    pub sessions: Vec<SessionGroup>,
    /// `None` unless a preview was requested. Carries the window id it belongs to,
    /// so a snapshot that arrives after the selection moved can be recognised as
    /// stale rather than drawn beside the wrong row.
    pub preview: Option<(String, Vec<PanePreview>)>,
    /// The pane tmux focus is on, as [`tmux::focused_pane`] resolves it. Read from
    /// the same `list-panes` the tree comes from, so it costs no extra subprocess and
    /// cannot disagree with the tree it arrives with.
    pub focused: Option<String>,
    /// A reparsed scratchpad, present only on a pass where the file actually
    /// changed.
    ///
    /// `None` means *unchanged*, not *empty* — the panel keeps what it has. That
    /// distinction is the whole point: the common pass does one `stat` and sends
    /// nothing, so an idle sidebar with notes open still draws zero frames.
    pub notes: Option<NoteFile>,
}

pub struct Worker {
    /// Set to collect immediately instead of waiting out [`INTERVAL`]. Used by the
    /// manual refresh key; the SIGUSR1 path has its own flag, read directly by this
    /// thread rather than relayed through the UI one.
    wake: Arc<AtomicBool>,
    /// Window to capture a preview of, or `None` for no preview at all — which is
    /// how the sidebar surface avoids paying for one.
    preview_target: Arc<Mutex<Option<String>>>,
}

impl Worker {
    /// Ask for a collection as soon as possible.
    pub fn request_refresh(&self) {
        self.wake.store(true, Ordering::Relaxed);
    }

    /// A worker with no collector behind it, for exercising input handling. Keeps
    /// the key tests off any tmux server, since a real one would list panes on the
    /// developer's own.
    #[cfg(test)]
    pub fn inert() -> Self {
        Self {
            wake: Arc::new(AtomicBool::new(false)),
            preview_target: Arc::new(Mutex::new(None)),
        }
    }

    /// Point the preview at a window, or turn it off with `None`.
    ///
    /// Requests a refresh on an actual change so the preview catches up with a
    /// motion immediately, rather than showing the previous window for up to a
    /// full interval — the lag would read as the wrong preview, not a late one.
    pub fn set_preview_target(&self, window_id: Option<&str>) {
        let Ok(mut target) = self.preview_target.lock() else {
            return;
        };
        let changed = target.as_deref() != window_id;
        if changed {
            *target = window_id.map(str::to_owned);
            drop(target);
            self.request_refresh();
        }
    }
}

/// Start collecting. The thread stops on its own once the receiver is dropped.
///
/// `own_pane` is the sidebar's own pane id, used only to resolve which pane tmux
/// focus is on (see [`tmux::focused_pane`]); empty for a popup.
///
/// `signalled` is the SIGUSR1 flag. This thread consumes it directly: relaying it
/// through the UI thread cost up to a full input timeout before the collection a
/// focus change asks for even started, which is most of what made a jump between
/// sessions feel late.
///
/// `notes_file` is resolved by the caller rather than here, because resolving it
/// reads a tmux option and this thread should not pay for that once a second.
/// `None` turns the watch off entirely, which is how a popup avoids a `stat` per
/// pass for a panel it will never draw.
pub fn spawn(
    agents_only: bool,
    own_pane: String,
    signalled: &'static AtomicBool,
    tx: Sender<Msg>,
    notes_file: Option<PathBuf>,
) -> Worker {
    let wake = Arc::new(AtomicBool::new(false));
    let thread_wake = Arc::clone(&wake);
    let preview_target = Arc::new(Mutex::new(None));
    let thread_target = Arc::clone(&preview_target);

    thread::spawn(move || {
        let mut git = GitCache::new();
        let mut notes_watch = notes_file.map(NotesWatch::new);
        let mut order = OrderCache::default();
        loop {
            // A tmux failure here means the server is gone, and so is our pane;
            // there is nothing to report and nothing to retry.
            let Some((sessions, focused)) =
                collect(agents_only, &own_pane, &mut git, &mut order)
            else {
                return;
            };
            // Read the target fresh each pass: the UI may have moved since the last
            // one, and capturing the window it has since left would be wasted work.
            let target: Option<String> = match thread_target.lock() {
                Ok(target) => target.clone(),
                // A poisoned lock means the UI thread panicked; there is no one left
                // to draw a preview for.
                Err(_) => None,
            };
            let preview = target.map(|window_id| {
                let panes = preview::capture_window(&window_id);
                (window_id, panes)
            });
            let notes = notes_watch.as_mut().and_then(NotesWatch::poll);
            if tx
                .send(Msg::Snapshot(Snapshot {
                    sessions,
                    preview,
                    focused,
                    notes,
                }))
                .is_err()
            {
                return;
            }
            wait(&[&thread_wake, signalled], INTERVAL);
        }
    });

    Worker {
        wake,
        preview_target,
    }
}

/// Longest gap between retries after a failed read, in poll passes. At the
/// one-second interval that is about a minute, which is short enough that a
/// `chmod` fixing the problem is noticed while you are still at the keyboard.
const NOTES_MAX_BACKOFF: u32 = 64;

/// The scratchpad watch: where it is, what we last read, and how badly it is
/// going.
///
/// The `stat` is the whole design: the sidebar is open for hours beside a file
/// almost nobody is writing, so the common pass must cost one syscall and
/// produce nothing. Reading and re-parsing every second would be affordable in
/// CPU and still wrong — [`crate::app::fingerprint`] would see identical text
/// and skip the draw, but we would have spent the work to learn that.
struct NotesWatch {
    path: PathBuf,
    stamp: Stamp,
    /// Consecutive failed reads, driving the backoff.
    failures: u32,
    /// Passes still to skip before retrying.
    cooldown: u32,
}

impl NotesWatch {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            // Starts unread, so the first pass reparses whatever `run` already
            // loaded at startup. One redundant parse of a small file, off the UI
            // thread, producing identical lines — the fingerprint does not move
            // and nothing is drawn. Cheaper than threading the startup stamp
            // across the channel.
            stamp: Stamp::MISSING,
            failures: 0,
            cooldown: 0,
        }
    }

    /// Reparse, but only if the file moved since we last read it.
    ///
    /// A failed read does **not** advance the stamp. Advancing would mean a file
    /// that becomes readable again without its mtime or size changing — a
    /// `chmod`, a remount, a permissions fix — is never picked up, and the panel
    /// stays stale forever. Instead the failure is retried on a doubling
    /// backoff, so a permanently unreadable file settles at one attempt a minute
    /// rather than one a second, and a transient one recovers on the next pass.
    ///
    /// Split out of the loop so it can be tested against a real temp file without
    /// a thread, matching how the rest of the crate splits a decision from its
    /// caller.
    fn poll(&mut self) -> Option<NoteFile> {
        if self.cooldown > 0 {
            self.cooldown -= 1;
            return None;
        }
        if !notes::changed(&self.path, self.stamp) {
            return None;
        }
        match notes::load(&self.path) {
            Ok((file, fresh)) => {
                self.stamp = fresh;
                self.failures = 0;
                Some(file)
            }
            Err(_) => {
                self.failures = self.failures.saturating_add(1);
                self.cooldown = (1u32 << self.failures.min(6)).min(NOTES_MAX_BACKOFF);
                None
            }
        }
    }
}

/// Sleep up to `total`, returning early once any of `flags` has been raised.
///
/// Every flag is consumed on the way past, so one request produces one collection.
fn wait(flags: &[&AtomicBool], total: Duration) {
    let mut slept = Duration::ZERO;
    loop {
        // Checked before the first sleep, so a request raised while the previous
        // pass was still running is not made to wait out a slice.
        // Not `any`, which short-circuits: a flag left raised would fire a second
        // collection the moment this one finished.
        let mut woken = false;
        for flag in flags {
            woken |= flag.swap(false, Ordering::Relaxed);
        }
        if woken || slept >= total {
            return;
        }
        let slice = WAKE_SLICE.min(total - slept);
        thread::sleep(slice);
        slept += slice;
    }
}

/// The session order, and what it was read against.
///
/// `list-sessions` is a whole extra subprocess per pass, for an answer that only
/// changes when a session is created, killed, renamed, or re-ranked. The first three
/// show up as a change to the session set, which is free to check; the last is what
/// [`ORDER_TTL`] is for.
#[derive(Default)]
struct OrderCache {
    names: Vec<String>,
    order: Vec<String>,
    read_at: Option<Instant>,
}

impl OrderCache {
    fn refresh(&mut self, sessions: &[SessionGroup]) {
        let names: Vec<String> = sessions
            .iter()
            .map(|session| session.session_name.clone())
            .collect();
        if !self.is_fresh(&names) {
            self.order = tmux::session_order();
            self.names = names;
            self.read_at = Some(Instant::now());
        }
    }

    /// Split from [`Self::refresh`] so the decision is testable without tmux.
    fn is_fresh(&self, names: &[String]) -> bool {
        self.read_at
            .is_some_and(|at| at.elapsed() < ORDER_TTL && self.names == names)
    }
}

/// One collection pass: read every pane, attach git context, group.
///
/// Returns the tree and the focused pane together — both come out of the same
/// `list-panes`, so they always describe one instant.
fn collect(
    agents_only: bool,
    own_pane: &str,
    git: &mut GitCache,
    order: &mut OrderCache,
) -> Option<(Vec<SessionGroup>, Option<String>)> {
    let rows = tmux::list_panes().ok()?;
    let focused = tmux::focused_pane(&rows, own_pane);
    let mut sessions = tmux::group_sessions(&rows, agents_only);
    // tmux itself has no session ordering, so without this the list is alphabetical
    // whatever the user arranged. Cached, because it is a second subprocess and this
    // pass may be one of a burst.
    order.refresh(&sessions);
    tmux::apply_session_order(&mut sessions, &order.order);

    let mut live_paths: Vec<&str> = Vec::new();
    for pane in sessions
        .iter_mut()
        .flat_map(|session| &mut session.windows)
        .flat_map(|window| &mut window.panes)
    {
        let info = git.get(&pane.current_path);
        pane.branch = info.branch;
        pane.worktree = info.worktree;
    }
    for session in &sessions {
        for window in &session.windows {
            for pane in &window.panes {
                live_paths.push(&pane.current_path);
            }
        }
    }
    git.retain_paths(&live_paths);

    Some((sessions, focused))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    // ─── the notes watch ──────────────────────────────────────────────

    /// A scratchpad in a temp dir, and its path. Named per test so a parallel
    /// run cannot have two tests writing the same file.
    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("agent-mgr-worker-{name}.md"));
        let _ = std::fs::remove_file(&path);
        path
    }

    fn write(path: &std::path::Path, body: &str) {
        std::fs::write(path, body).expect("write scratch notes");
    }

    #[test]
    fn an_unchanged_file_is_not_reparsed() {
        // The point of the `stat`: a sidebar open for hours beside a file nobody
        // is writing must send nothing, or the panel becomes the one thing that
        // keeps the loop awake.
        let path = scratch("unchanged");
        write(&path, "## [ ] one\n");
        let mut watch = NotesWatch::new(path.clone());

        assert!(watch.poll().is_some(), "the first read");
        for _ in 0..5 {
            assert!(
                watch.poll().is_none(),
                "an untouched file must not come back a second time"
            );
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_appended_note_comes_back_on_the_next_pass() {
        let path = scratch("appended");
        write(&path, "## [ ] one\n");
        let mut watch = NotesWatch::new(path.clone());
        let first = watch.poll().expect("the first read");
        assert_eq!(first.len(), 1);

        write(&path, "## [ ] one\n\n## [ ] two\n");
        let second = watch.poll().expect("the append");
        assert_eq!(second.len(), 2);
        assert_eq!(second.notes[1].title, "two");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_scratchpad_that_never_existed_reports_nothing_every_pass() {
        // The first run has no file, and a panel is not worth waking the loop for
        // once a second to rediscover that.
        let path = scratch("absent");
        let mut watch = NotesWatch::new(path.clone());
        for _ in 0..3 {
            assert!(watch.poll().is_none());
        }
    }

    #[test]
    fn an_unreadable_file_is_retried_on_a_backoff_rather_than_given_up_on() {
        // Advancing the stamp on a failed read would mean a file that becomes
        // readable again *without* its mtime or size changing — a chmod, a
        // remount, a permissions fix — is never picked up, and the panel stays
        // stale for as long as the sidebar is open.
        let path = scratch("unreadable");
        write(&path, "## [ ] one\n");
        let mut watch = NotesWatch::new(path.clone());
        assert!(watch.poll().is_some(), "the first read");

        // Make it unreadable without touching mtime or size.
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o000);
        std::fs::set_permissions(&path, perms).unwrap();
        // Force a re-read attempt by pretending we never saw this content.
        watch.stamp = Stamp::MISSING;

        assert!(watch.poll().is_none(), "unreadable");
        assert!(watch.failures > 0, "the failure was recorded");
        assert!(watch.cooldown > 0, "and it backed off rather than spinning");
        assert_ne!(watch.stamp, Stamp::of(&path), "the stamp must NOT have moved");

        // Readable again, still same mtime and size.
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o600);
        std::fs::set_permissions(&path, perms).unwrap();

        // Wait out the cooldown, then it must recover.
        for _ in 0..=watch.cooldown {
            if watch.poll().is_some() {
                assert_eq!(watch.failures, 0, "the failure count resets on success");
                let _ = std::fs::remove_file(&path);
                return;
            }
        }
        let _ = std::fs::remove_file(&path);
        panic!("never recovered after the file became readable again");
    }

    #[test]
    fn the_backoff_is_bounded_so_a_dead_file_still_gets_retried() {
        // A permanently unreadable file must settle at roughly one attempt a
        // minute — neither a one-second spin nor silence forever.
        let mut watch = NotesWatch::new(scratch("bounded"));
        for _ in 0..20 {
            watch.failures = watch.failures.saturating_add(1);
            watch.cooldown = (1u32 << watch.failures.min(6)).min(NOTES_MAX_BACKOFF);
        }
        assert_eq!(watch.cooldown, NOTES_MAX_BACKOFF);
    }

    #[test]
    fn a_file_deleted_under_us_empties_the_panel_once() {
        // Once, not every pass: the deletion is a change, and what follows is a
        // steady state that must go quiet again.
        let path = scratch("deleted");
        write(&path, "## [ ] one\n");
        let mut watch = NotesWatch::new(path.clone());
        assert!(watch.poll().is_some());

        std::fs::remove_file(&path).expect("remove scratch notes");
        let emptied = watch.poll().expect("the deletion");
        assert!(emptied.is_empty(), "a missing file reads as an empty one");
        assert!(
            watch.poll().is_none(),
            "and then nothing more"
        );
    }

    #[test]
    fn wait_returns_early_when_a_refresh_is_requested() {
        let wake = AtomicBool::new(true);
        let start = Instant::now();
        wait(&[&wake], Duration::from_secs(5));
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "a pending wake must not be ignored for the full interval"
        );
    }

    /// The tmux focus hooks raise their own flag, and it reaches this thread
    /// directly. Relaying it through the UI thread is what used to add an input
    /// timeout to every session jump.
    #[test]
    fn wait_returns_early_for_the_signal_flag_too() {
        let wake = AtomicBool::new(false);
        let signalled = AtomicBool::new(true);
        let start = Instant::now();
        wait(&[&wake, &signalled], Duration::from_secs(5));
        assert!(start.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn wait_consumes_every_request_so_one_fires_one_collection() {
        let wake = AtomicBool::new(true);
        let signalled = AtomicBool::new(true);
        wait(&[&wake, &signalled], Duration::from_millis(1));
        assert!(!wake.load(Ordering::Relaxed));
        assert!(
            !signalled.load(Ordering::Relaxed),
            "a flag left raised would spin the collector at zero interval"
        );
    }

    #[test]
    fn wait_sleeps_out_a_quiet_interval() {
        let wake = AtomicBool::new(false);
        let start = Instant::now();
        wait(&[&wake], Duration::from_millis(120));
        assert!(start.elapsed() >= Duration::from_millis(100));
    }

    fn named(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn an_unread_order_is_never_fresh() {
        let cache = OrderCache::default();
        assert!(!cache.is_fresh(&named(&["work"])));
    }

    /// A new, killed or renamed session has to reorder the list on the next pass —
    /// waiting out the TTL would show it in the wrong place.
    #[test]
    fn a_changed_session_set_is_not_fresh() {
        let cache = OrderCache {
            names: named(&["work"]),
            order: named(&["work"]),
            read_at: Some(Instant::now()),
        };
        assert!(cache.is_fresh(&named(&["work"])));
        assert!(!cache.is_fresh(&named(&["work", "notes"])));
    }

    #[test]
    fn an_aged_order_is_re_read_even_with_the_same_sessions() {
        let cache = OrderCache {
            names: named(&["work"]),
            order: named(&["work"]),
            read_at: Some(Instant::now() - ORDER_TTL - Duration::from_millis(1)),
        };
        assert!(!cache.is_fresh(&named(&["work"])));
    }
}
