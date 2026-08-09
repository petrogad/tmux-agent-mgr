//! The background collector thread.
//!
//! Every tmux and git subprocess this plugin runs while the TUI is open happens
//! here. The UI thread does nothing but read from a channel, hash lines, and
//! occasionally draw — so a slow `git` on a network mount or a busy tmux server
//! can never stall input or leave a half-painted frame on screen.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::git::GitCache;
use crate::model::SessionGroup;
use crate::notes::{self, NoteFile, Stamp};
use crate::preview::{self, PanePreview};
use crate::tmux;

/// How long between collections. Everything urgent arrives via [`Worker::wake`]
/// instead, so this only has to be fast enough for changes nobody told us about
/// (a new window, an agent starting).
const INTERVAL: Duration = Duration::from_millis(1000);
/// Granularity at which a wake request is noticed while waiting.
const WAKE_SLICE: Duration = Duration::from_millis(50);

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
    pub rx: Receiver<Snapshot>,
    /// Set to collect immediately instead of waiting out [`INTERVAL`]. Used by
    /// the SIGUSR1 path (tmux focus changed) and the manual refresh key.
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
/// `notes_file` is resolved by the caller rather than here, because resolving it
/// reads a tmux option and this thread should not pay for that once a second.
/// `None` turns the watch off entirely, which is how a popup avoids a `stat` per
/// pass for a panel it will never draw.
pub fn spawn(agents_only: bool, own_pane: String, notes_file: Option<PathBuf>) -> Worker {
    let (tx, rx) = mpsc::channel();
    let wake = Arc::new(AtomicBool::new(false));
    let thread_wake = Arc::clone(&wake);
    let preview_target = Arc::new(Mutex::new(None));
    let thread_target = Arc::clone(&preview_target);

    thread::spawn(move || {
        let mut git = GitCache::new();
        let mut notes_watch = notes_file.map(NotesWatch::new);
        loop {
            // A tmux failure here means the server is gone, and so is our pane;
            // there is nothing to report and nothing to retry.
            let Some((sessions, focused)) = collect(agents_only, &own_pane, &mut git) else {
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
                .send(Snapshot {
                    sessions,
                    preview,
                    focused,
                    notes,
                })
                .is_err()
            {
                return;
            }
            wait(&thread_wake, INTERVAL);
        }
    });

    Worker {
        rx,
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

/// Sleep up to `total`, returning early once a wake has been requested.
fn wait(wake: &AtomicBool, total: Duration) {
    let mut slept = Duration::ZERO;
    while slept < total {
        if wake.swap(false, Ordering::Relaxed) {
            return;
        }
        let slice = WAKE_SLICE.min(total - slept);
        thread::sleep(slice);
        slept += slice;
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
) -> Option<(Vec<SessionGroup>, Option<String>)> {
    let rows = tmux::list_panes().ok()?;
    let focused = tmux::focused_pane(&rows, own_pane);
    let mut sessions = tmux::group_sessions(&rows, agents_only);
    // One extra subprocess per pass to honour the user's session order. tmux itself
    // has no session ordering, so without this the list is alphabetical whatever the
    // user arranged.
    tmux::apply_session_order(&mut sessions, &tmux::session_order());

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
        wait(&wake, Duration::from_secs(5));
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "a pending wake must not be ignored for the full interval"
        );
    }

    #[test]
    fn wait_consumes_the_request_so_it_fires_once() {
        let wake = AtomicBool::new(true);
        wait(&wake, Duration::from_millis(1));
        assert!(!wake.load(Ordering::Relaxed));
    }

    #[test]
    fn wait_sleeps_out_a_quiet_interval() {
        let wake = AtomicBool::new(false);
        let start = Instant::now();
        wait(&wake, Duration::from_millis(120));
        assert!(start.elapsed() >= Duration::from_millis(100));
    }
}
