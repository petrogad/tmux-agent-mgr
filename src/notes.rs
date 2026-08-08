//! A global scratchpad, stored as markdown.
//!
//! Notes are not per-pane and not per-session: the whole point is that you are
//! deep in one agent's output, notice something about *another* thing, and want
//! it out of your head without leaving. Scoping that to the pane you happened to
//! be in would make you go looking for it later in the one place you no longer
//! remember.
//!
//! The file is markdown rather than JSON because three different things write it
//! and none of them should have to understand the others: you in `$EDITOR`, an
//! agent with an edit tool, and this binary. JSON would force every writer
//! through us.
//!
//! ```markdown
//! # Notes
//!
//! ## [ ] auth redirect drops ?next
//! <!-- t=2026-08-08T14:22:03Z from=blueberry:3 -->
//! The 302 out of /callback loses the `next` param.
//!
//! ## [x] starship timeout
//! Raised to 1500ms.
//! ```
//!
//! Two properties the rest of the feature leans on:
//!
//! - **Appends never renumber.** New notes go at the end, so an agent writing
//!   while you navigate cannot move the note under your cursor. Only deletion
//!   renumbers, and only the TUI deletes. That is what makes it safe for the
//!   detail overlay to address a note by index.
//! - **The file is the source of truth, not the TUI.** We hold a parsed snapshot
//!   plus the [`Stamp`] it came from and reparse only when that changes; every
//!   mutation re-reads under the lock first. Two agents appending at once must
//!   not lose one.
//!
//! Everything above the I/O section is pure, which is what lets the tests cover
//! the format without a filesystem.

#![allow(
    dead_code,
    reason = "the panel reads (`path`, `load`, `len`); what is still uncalled is \
              the reload path (`changed`) and the write path (`render`, `store`, \
              `update`, `toggle`, `is_empty`), which the worker and the panel's \
              input mode consume. Remove this attribute with that change"
)]

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// One note: a title you can read in a 24-column sidebar, and a body you read in
/// the overlay.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct Note {
    pub done: bool,
    pub title: String,
    /// Key/value pairs from the `<!-- … -->` line under the heading. Kept as a
    /// list rather than a map so unknown keys survive a rewrite in their original
    /// order — we are not the only writer, and dropping a key someone else added
    /// would make this file hostile to edit by hand.
    pub meta: Vec<(String, String)>,
    /// Body text, with trailing blank lines stripped. May contain anything,
    /// including fenced code and its own `##` headings.
    pub body: String,
}

impl Note {
    /// A note as typed at the sidebar prompt or handed to `note add`.
    ///
    /// The title is flattened because a model will hand you a multi-line string
    /// verbatim, and a newline in a heading would silently split one note into
    /// two.
    pub fn new(title: &str, body: &str) -> Self {
        Self {
            done: false,
            title: flatten(title),
            meta: Vec::new(),
            body: body.trim_end().to_owned(),
        }
    }
}

/// A parsed notes file.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct NoteFile {
    /// Everything before the first note, verbatim — the `# Notes` title, a
    /// comment someone left at the top, whatever. Preserved so a rewrite does not
    /// quietly delete it.
    pub preamble: String,
    pub notes: Vec<Note>,
}

impl NoteFile {
    pub fn is_empty(&self) -> bool {
        self.notes.is_empty()
    }

    pub fn len(&self) -> usize {
        self.notes.len()
    }

    /// Flip a note's done flag. Out-of-range is a no-op rather than a panic: the
    /// index comes from a selection that a concurrent write may have shortened.
    pub fn toggle(&mut self, index: usize) {
        if let Some(note) = self.notes.get_mut(index) {
            note.done = !note.done;
        }
    }

}

// ─── parsing ─────────────────────────────────────────────────────────

/// The heading marker that starts a note. Deliberately requires the space: `##x`
/// is not an ATX heading in any markdown dialect, and treating it as one would
/// mean an agent's stray `##` swallowed the rest of the file.
const HEADING: &str = "## ";

/// Parse a notes file. Never fails — anything unrecognised becomes body text,
/// because losing something you typed is the only unrecoverable outcome here.
pub fn parse(text: &str) -> NoteFile {
    let mut file = NoteFile::default();
    let mut fence: Option<Fence> = None;
    let mut current: Option<Note> = None;
    let mut body: Vec<&str> = Vec::new();
    let mut preamble: Vec<&str> = Vec::new();

    for line in text.lines() {
        // Fence state has to be tracked through the preamble as well as through
        // bodies: a `##` inside a fenced block anywhere in the file is code, not
        // a heading. Notes are mostly *about* code, so this is the common case,
        // not an exotic one.
        if let Some(open) = &fence {
            if open.closed_by(line) {
                fence = None;
            }
        } else if let Some(open) = Fence::opened_by(line) {
            fence = Some(open);
        } else if let Some(heading) = parse_heading(line) {
            if let Some(mut note) = current.take() {
                note.body = join_body(&body);
                file.notes.push(note);
            }
            body.clear();
            current = Some(heading);
            continue;
        }

        match &current {
            Some(_) => body.push(line),
            None => preamble.push(line),
        }
    }

    if let Some(mut note) = current.take() {
        note.body = join_body(&body);
        file.notes.push(note);
    }

    file.preamble = join_preamble(&preamble);
    // The metadata line is only metadata directly under its heading. Further down
    // it is prose that happens to be a comment, and rewriting it as structure
    // would move it.
    for note in &mut file.notes {
        lift_meta(note);
    }
    file
}

/// `## [x] title` → a note. `None` when the line is not a note heading.
fn parse_heading(line: &str) -> Option<Note> {
    let rest = line.strip_prefix(HEADING)?;
    let (done, title) = match strip_checkbox(rest) {
        Some(pair) => pair,
        None => (false, rest),
    };
    Some(Note {
        done,
        title: title.trim().to_owned(),
        meta: Vec::new(),
        body: String::new(),
    })
}

/// Strip one leading `[ ]` / `[x]`, returning the flag and the remainder.
///
/// Exactly one, which is what lets a title that literally starts with `[x]`
/// survive: `append` always writes an explicit checkbox, so the user's text ends
/// up as `## [ ] [x] whatever` and reads back with the braces intact.
fn strip_checkbox(rest: &str) -> Option<(bool, &str)> {
    let (mark, tail) = match rest.as_bytes() {
        [b'[', mark, b']', tail @ ..] => (*mark, tail),
        _ => return None,
    };
    let done = match mark {
        b' ' => false,
        b'x' | b'X' => true,
        _ => return None,
    };
    // Safe: we only skipped three ASCII bytes off a `&str` boundary.
    let tail = std::str::from_utf8(tail).ok()?;
    Some((done, tail))
}

/// Move a leading `<!-- k=v -->` line out of the body and into `meta`.
fn lift_meta(note: &mut Note) {
    let Some(first) = note.body.lines().next() else {
        return;
    };
    let Some(meta) = parse_meta(first) else {
        return;
    };
    note.meta = meta;
    note.body = note.body[first.len()..].trim_start_matches('\n').to_owned();
}

/// `<!-- k=v k=v -->` → pairs. `None` for any comment that is not entirely
/// key/value, so a freeform remark stays in the body rather than being eaten.
fn parse_meta(line: &str) -> Option<Vec<(String, String)>> {
    let inner = line.trim().strip_prefix("<!--")?.strip_suffix("-->")?;
    let mut pairs = Vec::new();
    for token in inner.split_whitespace() {
        let (key, value) = token.split_once('=')?;
        if key.is_empty() {
            return None;
        }
        pairs.push((key.to_owned(), value.to_owned()));
    }
    (!pairs.is_empty()).then_some(pairs)
}

/// An open code fence, remembered precisely enough to know what closes it.
///
/// A simplification of CommonMark: we track the marker and its length, and ignore
/// info strings and indentation. That is enough for "don't split a note in the
/// middle of a code block" and stops well short of being a markdown parser.
struct Fence {
    marker: u8,
    len: usize,
}

impl Fence {
    fn opened_by(line: &str) -> Option<Self> {
        let trimmed = line.trim_start();
        let marker = match trimmed.as_bytes().first()? {
            b'`' => b'`',
            b'~' => b'~',
            _ => return None,
        };
        let len = trimmed.bytes().take_while(|byte| *byte == marker).count();
        (len >= 3).then_some(Self { marker, len })
    }

    fn closed_by(&self, line: &str) -> bool {
        let trimmed = line.trim_start();
        let len = trimmed.bytes().take_while(|byte| *byte == self.marker).count();
        len >= self.len && trimmed.len() == len
    }
}

/// Body lines joined, with trailing blank lines dropped — they are the separator
/// before the next heading, not content, and keeping them would make every
/// rewrite grow the file.
fn join_body(lines: &[&str]) -> String {
    let end = lines
        .iter()
        .rposition(|line| !line.trim().is_empty())
        .map_or(0, |last| last + 1);
    lines[..end].join("\n")
}

/// Preamble lines joined, keeping the blank line that separates them from the
/// first heading so a round trip is byte-identical.
fn join_preamble(lines: &[&str]) -> String {
    if lines.is_empty() {
        return String::new();
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// Collapse whitespace runs, including newlines, to single spaces.
fn flatten(title: &str) -> String {
    title.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ─── rendering ───────────────────────────────────────────────────────

/// Render a file back to markdown.
///
/// Canonical form: the preamble verbatim, then each note as a heading, an
/// optional metadata line, its body, and a blank line before the next. The
/// trailing newline is normalised, so `render(parse(x)) == x` holds for any file
/// already in canonical form and `render(parse(_))` is idempotent for the rest.
pub fn render(file: &NoteFile) -> String {
    let mut out = String::with_capacity(file.preamble.len() + file.notes.len() * 64);
    out.push_str(&file.preamble);
    for (index, note) in file.notes.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(&block(note));
    }
    out
}

/// One note as markdown, ending in a newline.
fn block(note: &Note) -> String {
    let mark = if note.done { 'x' } else { ' ' };
    let mut out = format!("{HEADING}[{mark}] {}\n", note.title);
    if !note.meta.is_empty() {
        let pairs: Vec<String> = note
            .meta
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect();
        out.push_str(&format!("<!-- {} -->\n", pairs.join(" ")));
    }
    if !note.body.is_empty() {
        out.push_str(&note.body);
        out.push('\n');
    }
    out
}

/// Append a note to raw file text.
///
/// Takes and returns raw text rather than a [`NoteFile`] on purpose: an append is
/// the one operation an agent performs, and it must never rewrite the parts of
/// the file it did not understand. Parsing and re-rendering to add one line at
/// the end would turn every agent write into a whole-file rewrite.
pub fn append(text: &str, note: &Note) -> String {
    let base = text.trim_end_matches('\n');
    let mut out = String::with_capacity(base.len() + 96);
    if !base.is_empty() {
        out.push_str(base);
        out.push_str("\n\n");
    }
    out.push_str(&block(note));
    out
}

// ─── I/O ─────────────────────────────────────────────────────────────
//
// `changed`, `render`, `store` and `update` are the read/write path the sidebar
// panel uses; the `note` subcommand below only appends. They are covered by tests
// but have no non-test caller until the panel lands.

/// What a snapshot was read from, so the worker can tell "unchanged" from
/// "unread" with one `stat` instead of a read and a parse.
///
/// Size is checked alongside mtime because a coarse filesystem timestamp can hide
/// two writes in the same second, which for an append-heavy file is not rare.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Stamp {
    mtime: Option<SystemTime>,
    len: u64,
}

impl Stamp {
    /// The stamp of a file that does not exist. Distinct from any real file, so
    /// creating the first note registers as a change.
    pub const MISSING: Self = Self {
        mtime: None,
        len: 0,
    };

    pub fn of(path: &Path) -> Self {
        match fs::metadata(path) {
            Ok(meta) => Self {
                mtime: meta.modified().ok(),
                len: meta.len(),
            },
            Err(_) => Self::MISSING,
        }
    }
}

/// Has the file changed since `since`? One `stat`, cheap enough for the poll.
pub fn changed(path: &Path, since: Stamp) -> bool {
    Stamp::of(path) != since
}

/// Read and parse, treating a missing file as an empty one — the first run is not
/// an error state.
pub fn load(path: &Path) -> io::Result<(NoteFile, Stamp)> {
    let stamp = Stamp::of(path);
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err),
    };
    Ok((parse(&text), stamp))
}

/// Write the file back, atomically.
///
/// Temp file plus rename, so a reader never sees a half-written document and a
/// crash mid-write cannot leave you with a truncated scratchpad.
pub fn store(path: &Path, file: &NoteFile) -> io::Result<()> {
    let _guard = Lock::acquire(path)?;
    store_locked(path, file)
}

fn store_locked(path: &Path, file: &NoteFile) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = temp_path(path);
    {
        let mut handle = File::create(&temp)?;
        handle.write_all(render(file).as_bytes())?;
        handle.sync_all()?;
    }
    fs::rename(&temp, path)
}

/// Append one note under the lock.
///
/// Re-reads inside the lock rather than trusting any snapshot the caller holds:
/// this is the path several agents hit at once, and a read-modify-write over a
/// stale copy is exactly how one of them loses.
pub fn add(path: &Path, note: &Note) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let _guard = Lock::acquire(path)?;
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err),
    };
    let temp = temp_path(path);
    {
        let mut handle = File::create(&temp)?;
        handle.write_all(append(&text, note).as_bytes())?;
        handle.sync_all()?;
    }
    fs::rename(&temp, path)
}

/// Apply a mutation to the file under the lock, re-reading first.
///
/// The TUI's snapshot is always potentially stale — an agent may have appended
/// since the last poll — so a toggle or delete resolves against fresh content
/// rather than overwriting the file with what the sidebar last drew.
pub fn update<F>(path: &Path, mutate: F) -> io::Result<NoteFile>
where
    F: FnOnce(&mut NoteFile),
{
    let _guard = Lock::acquire(path)?;
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err),
    };
    let mut file = parse(&text);
    mutate(&mut file);
    store_locked(path, &file)?;
    Ok(file)
}

fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp{}", std::process::id()));
    path.with_file_name(name)
}

/// An advisory `flock` on a sidecar file.
///
/// A sidecar rather than the notes file itself, because the write path replaces
/// that file by rename — locking an inode we are about to unlink would protect
/// nothing.
struct Lock(File);

impl Lock {
    fn acquire(path: &Path) -> io::Result<Self> {
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(".lock");
        let handle = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path.with_file_name(name))?;
        // SAFETY: a valid fd owned by `handle` for the duration of the call.
        if unsafe { libc::flock(handle.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(handle))
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        // SAFETY: same fd, still owned by `self.0`.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// Where the notes live: `@agent_mgr_notes_file` if set, else the XDG default.
///
/// Resolved fresh per call rather than cached, for the same reason the binary
/// path is: it lets you move the file and reload without restarting anything.
pub fn path() -> PathBuf {
    match crate::tmux::options::global(crate::tmux::options::CFG_NOTES_FILE) {
        Some(configured) => expand_home(&configured),
        None => default_path(),
    }
}

/// Default location, honouring `XDG_DATA_HOME`.
fn default_path() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("tmux-agent-mgr/notes.md")
}

/// Expand a leading `~/`. tmux does not do this for option values, and a literal
/// `./~` directory is a baffling thing to find in your home.
fn expand_home(value: &str) -> PathBuf {
    match value.strip_prefix("~/") {
        Some(rest) => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(rest),
            None => PathBuf::from(value),
        },
        None => PathBuf::from(value),
    }
}

// ─── the `note` subcommand ───────────────────────────────────────────

/// `note add <title> [--body -|<text>]` · `note list` · `note show <index>`
///
/// The agent-facing surface. An agent *could* append markdown by hand — that is
/// the point of the format — but handing it one command avoids teaching it the
/// heading rules and gets the locking for free.
pub fn cmd_note(args: &[&str]) -> i32 {
    match args.first().copied() {
        Some("add") => cmd_add(&args[1..]),
        Some("list") => cmd_list(),
        Some("show") => cmd_show(&args[1..]),
        other => {
            eprintln!("agent-mgr note: expected add|list|show, got {other:?}");
            2
        }
    }
}

fn cmd_add(args: &[&str]) -> i32 {
    let mut title: Option<&str> = None;
    let mut body: Option<String> = None;
    let mut index = 0;
    while index < args.len() {
        match args[index] {
            "--body" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("agent-mgr note add: --body needs a value ('-' for stdin)");
                    return 2;
                };
                body = Some(match *value {
                    "-" => match io::read_to_string(io::stdin()) {
                        Ok(text) => text,
                        Err(err) => {
                            eprintln!("agent-mgr note add: reading stdin: {err}");
                            return 1;
                        }
                    },
                    text => text.to_owned(),
                });
                index += 2;
            }
            other if title.is_none() => {
                title = Some(other);
                index += 1;
            }
            other => {
                eprintln!("agent-mgr note add: unexpected argument {other:?}");
                return 2;
            }
        }
    }

    let Some(title) = title else {
        eprintln!("agent-mgr note add: a title is required");
        return 2;
    };

    let mut note = Note::new(title, body.as_deref().unwrap_or(""));
    note.meta = origin_meta();
    match add(&path(), &note) {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("agent-mgr note add: {err}");
            1
        }
    }
}

/// Where and when a note was taken.
///
/// Best-effort: a note written outside tmux, or when the `display-message` fails,
/// simply carries no origin. Refusing to record the note over missing metadata
/// would be the wrong trade — the note is the point.
fn origin_meta() -> Vec<(String, String)> {
    let mut meta = Vec::new();
    if let Ok(elapsed) = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
        meta.push(("t".to_owned(), elapsed.as_secs().to_string()));
    }
    if let Some(from) = crate::tmux::commands::run_tmux(&["display-message", "-p", "#S:#I"]) {
        let from = from.trim();
        if !from.is_empty() {
            meta.push(("from".to_owned(), from.to_owned()));
        }
    }
    meta
}

/// One line per note: index, `open`/`done`, title. Tab-separated, matching
/// `daemon --once` — the quickest way to check the file without the TUI.
fn cmd_list() -> i32 {
    let (file, _) = match load(&path()) {
        Ok(loaded) => loaded,
        Err(err) => {
            eprintln!("agent-mgr note list: {err}");
            return 1;
        }
    };
    for (index, note) in file.notes.iter().enumerate() {
        let state = if note.done { "done" } else { "open" };
        println!("{index}\t{state}\t{}", note.title);
    }
    0
}

/// Print one note as markdown. This is what the detail overlay runs.
fn cmd_show(args: &[&str]) -> i32 {
    let Some(index) = args.first().and_then(|raw| raw.parse::<usize>().ok()) else {
        eprintln!("agent-mgr note show: expected a note index");
        return 2;
    };
    let (file, _) = match load(&path()) {
        Ok(loaded) => loaded,
        Err(err) => {
            eprintln!("agent-mgr note show: {err}");
            return 1;
        }
    };
    // Out of range is not an error worth a non-zero exit: the file may have been
    // edited between the keypress and the popup opening, and a popup that reports
    // a failure is worse than one that says the note is gone.
    match file.notes.get(index) {
        Some(note) => print!("{}", block(note)),
        None => println!("note {index} is no longer there"),
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn titles(file: &NoteFile) -> Vec<&str> {
        file.notes.iter().map(|note| note.title.as_str()).collect()
    }

    // ─── parsing ─────────────────────────────────────────────────

    #[test]
    fn a_heading_without_a_checkbox_is_an_open_note() {
        // The format an agent actually writes when told "add a note". Requiring
        // the checkbox would make the common case unparseable.
        let file = parse("## fix the redirect\nbody\n");
        assert_eq!(titles(&file), ["fix the redirect"]);
        assert!(!file.notes[0].done);
        assert_eq!(file.notes[0].body, "body");
    }

    #[test]
    fn a_checkbox_marks_a_note_done_in_either_case() {
        assert!(parse("## [x] done\n").notes[0].done);
        assert!(parse("## [X] done\n").notes[0].done);
        assert!(!parse("## [ ] open\n").notes[0].done);
    }

    #[test]
    fn a_heading_needs_its_space_so_a_stray_hash_run_cannot_swallow_the_file() {
        let file = parse("## one\n##notaheading\n###deeper\n");
        assert_eq!(file.notes.len(), 1);
        assert!(file.notes[0].body.contains("##notaheading"));
    }

    #[test]
    fn a_heading_inside_a_fence_does_not_start_a_note() {
        // Notes are mostly about code, so bodies contain fenced markdown often
        // enough that an unfenced scan would split notes at random.
        let file = parse("## one\n```md\n## not a note\n```\ntail\n");
        assert_eq!(file.notes.len(), 1);
        assert!(file.notes[0].body.contains("## not a note"));
        assert!(file.notes[0].body.ends_with("tail"));
    }

    #[test]
    fn a_tilde_fence_and_a_longer_closing_fence_both_work() {
        let file = parse("## one\n~~~\n## no\n~~~\n\n## two\n");
        assert_eq!(titles(&file), ["one", "two"]);

        let nested = parse("## one\n````\n```\n## no\n````\n\n## two\n");
        assert_eq!(titles(&nested), ["one", "two"]);
    }

    #[test]
    fn text_before_the_first_note_is_preserved_as_preamble() {
        let file = parse("# Notes\n\n## one\n");
        assert_eq!(file.preamble, "# Notes\n\n");
        assert_eq!(titles(&file), ["one"]);
    }

    #[test]
    fn metadata_is_read_only_from_the_line_under_the_heading() {
        let file = parse("## one\nbody\n<!-- t=123 -->\n");
        assert!(file.notes[0].meta.is_empty(), "this one is prose");
        assert!(file.notes[0].body.contains("<!-- t=123 -->"));
    }

    #[test]
    fn an_unrecognised_comment_line_stays_in_the_body() {
        // Losing text somebody typed is the one unrecoverable failure here, so
        // anything that is not cleanly key/value is content.
        let file = parse("## one\n<!-- freeform remark -->\nbody\n");
        assert!(file.notes[0].meta.is_empty());
        assert!(file.notes[0].body.contains("freeform remark"));
    }

    #[test]
    fn metadata_keys_are_kept_in_order_including_unknown_ones() {
        let file = parse("## one\n<!-- t=1 from=work:2 mystery=yes -->\n");
        let keys: Vec<&str> = file.notes[0]
            .meta
            .iter()
            .map(|(key, _)| key.as_str())
            .collect();
        assert_eq!(keys, ["t", "from", "mystery"]);
    }

    #[test]
    fn trailing_blank_lines_are_separators_rather_than_body() {
        let file = parse("## one\nbody\n\n\n## two\n");
        assert_eq!(file.notes[0].body, "body");
    }

    #[test]
    fn an_empty_document_parses_to_nothing() {
        assert!(parse("").is_empty());
        assert!(parse("\n\n").is_empty());
    }

    // ─── rendering and round trips ───────────────────────────────

    #[test]
    fn a_canonical_document_round_trips_byte_for_byte() {
        let raw = "# Notes\n\n## [ ] one\n<!-- t=1 -->\nbody line\n\n## [x] two\n";
        assert_eq!(render(&parse(raw)), raw);
    }

    #[test]
    fn normalising_a_hand_written_document_is_idempotent() {
        // Hand-written input is not canonical — ragged blank lines, a missing
        // checkbox, no trailing newline. One pass normalises it; a second must
        // change nothing, or every poll would rewrite the file.
        let raw = "# Notes\n\n\n## one\nbody\n\n\n\n## [x] two";
        let once = render(&parse(raw));
        assert_eq!(render(&parse(&once)), once);
    }

    #[test]
    fn a_rewrite_preserves_unknown_metadata_keys() {
        let raw = "## [ ] one\n<!-- mystery=yes t=1 -->\n";
        assert_eq!(render(&parse(raw)), raw);
    }

    #[test]
    fn a_rewrite_preserves_a_fenced_body_verbatim() {
        let raw = "## [ ] one\n```rust\nfn main() {}\n```\n";
        assert_eq!(render(&parse(raw)), raw);
    }

    // ─── appending ───────────────────────────────────────────────

    #[test]
    fn appending_never_renumbers_existing_notes() {
        // The whole reason the overlay can address a note by index: an agent
        // writing while you navigate must not move the note under your cursor.
        let raw = "## one\n\n## two\n";
        let after = parse(&append(raw, &Note::new("three", "")));
        assert_eq!(titles(&after), ["one", "two", "three"]);
    }

    #[test]
    fn appending_leaves_earlier_bytes_untouched() {
        let raw = "# Notes\n\n## [ ] one\n<!-- mystery=yes -->\nbody\n";
        let after = append(raw, &Note::new("two", ""));
        assert!(after.starts_with(raw), "prefix rewritten: {after:?}");
    }

    #[test]
    fn appending_to_a_file_without_a_trailing_newline_still_makes_a_heading() {
        let out = append("## one\nbody", &Note::new("two", ""));
        assert_eq!(titles(&parse(&out)), ["one", "two"]);
    }

    #[test]
    fn appending_to_an_empty_document_produces_a_valid_one() {
        let out = append("", &Note::new("first", "body"));
        assert_eq!(out, "## [ ] first\nbody\n");
        assert_eq!(titles(&parse(&out)), ["first"]);
    }

    #[test]
    fn appending_after_a_preamble_alone_still_separates_the_heading() {
        let out = append("# Notes\n", &Note::new("first", ""));
        assert_eq!(out, "# Notes\n\n## [ ] first\n");
        assert_eq!(parse(&out).preamble, "# Notes\n\n");
    }

    #[test]
    fn a_multi_line_title_is_flattened_into_one_heading() {
        // A model will hand you a multi-line string verbatim; a newline in the
        // heading would silently split one note into two.
        let out = append("", &Note::new("line one\nline two", ""));
        let file = parse(&out);
        assert_eq!(titles(&file), ["line one line two"]);
    }

    #[test]
    fn a_title_that_looks_like_a_checkbox_is_not_mistaken_for_one() {
        let out = append("", &Note::new("[x] do the thing", ""));
        let file = parse(&out);
        assert!(!file.notes[0].done, "the literal braces are title text");
        assert_eq!(titles(&file), ["[x] do the thing"]);
    }

    #[test]
    fn repeated_appends_stay_parseable() {
        let mut text = String::new();
        for index in 0..5 {
            text = append(&text, &Note::new(&format!("note {index}"), "body"));
        }
        let file = parse(&text);
        assert_eq!(file.len(), 5);
        assert_eq!(file.notes[4].title, "note 4");
    }

    // ─── mutation ────────────────────────────────────────────────

    #[test]
    fn toggling_flips_only_the_selected_note() {
        let mut file = parse("## one\n\n## two\n");
        file.toggle(1);
        assert!(!file.notes[0].done);
        assert!(file.notes[1].done);
        file.toggle(1);
        assert!(!file.notes[1].done);
    }

    #[test]
    fn toggling_out_of_range_is_a_no_op_rather_than_a_panic() {
        // The index comes from a selection a concurrent write may have shortened.
        let mut file = parse("## one\n");
        file.toggle(9);
        assert_eq!(file.len(), 1);
    }

    // ─── staleness ───────────────────────────────────────────────

    #[test]
    fn a_missing_file_has_a_stamp_distinct_from_any_real_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        assert_eq!(Stamp::of(&path), Stamp::MISSING);
        assert!(!changed(&path, Stamp::MISSING), "still missing");

        add(&path, &Note::new("first", "")).unwrap();
        assert!(changed(&path, Stamp::MISSING), "creation is a change");
    }

    #[test]
    fn an_unchanged_file_reports_unchanged() {
        // This is what keeps an idle sidebar drawing zero frames: no reparse, so
        // nothing downstream can differ.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        add(&path, &Note::new("first", "")).unwrap();
        let (_, stamp) = load(&path).unwrap();
        assert!(!changed(&path, stamp));
    }

    #[test]
    fn a_same_second_append_is_still_seen_as_a_change() {
        // Filesystem mtime granularity can hide two writes in the same second,
        // which for an append-heavy file is not rare — hence the length check.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        add(&path, &Note::new("first", "")).unwrap();
        let (_, stamp) = load(&path).unwrap();
        add(&path, &Note::new("second", "")).unwrap();
        assert!(changed(&path, stamp));
    }

    // ─── I/O ─────────────────────────────────────────────────────

    #[test]
    fn loading_a_missing_file_yields_an_empty_document() {
        let dir = tempfile::tempdir().unwrap();
        let (file, stamp) = load(&dir.path().join("nope.md")).unwrap();
        assert!(file.is_empty());
        assert_eq!(stamp, Stamp::MISSING);
    }

    #[test]
    fn adding_creates_the_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/deeper/notes.md");
        add(&path, &Note::new("first", "body")).unwrap();
        assert_eq!(titles(&load(&path).unwrap().0), ["first"]);
    }

    #[test]
    fn an_update_resolves_against_fresh_content_not_a_stale_snapshot() {
        // The sidebar's copy is always potentially stale — an agent may have
        // appended since the last poll. A toggle must not overwrite the file with
        // what the sidebar last drew.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        add(&path, &Note::new("first", "")).unwrap();
        let (stale, _) = load(&path).unwrap();
        assert_eq!(stale.len(), 1);

        add(&path, &Note::new("second", "")).unwrap();
        let after = update(&path, |file| file.toggle(0)).unwrap();

        assert_eq!(titles(&after), ["first", "second"], "append survived");
        assert!(after.notes[0].done);
    }

    #[test]
    fn a_store_leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        add(&path, &Note::new("first", "")).unwrap();
        let strays: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp"))
            .collect();
        assert!(strays.is_empty(), "left behind: {strays:?}");
    }

    #[test]
    fn concurrent_appends_all_survive() {
        // The claim the lock exists for. Several agents finishing a turn at once
        // is the ordinary case, not a stress test, and a read-modify-write race
        // here silently eats somebody's note.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.md");
        std::thread::scope(|scope| {
            for index in 0..8 {
                let path = path.clone();
                scope.spawn(move || {
                    add(&path, &Note::new(&format!("note {index}"), "body")).unwrap();
                });
            }
        });
        let (file, _) = load(&path).unwrap();
        let mut found = titles(&file);
        found.sort_unstable();
        assert_eq!(found.len(), 8, "lost a note: {found:?}");
    }

    #[test]
    fn a_configured_path_expands_a_leading_tilde() {
        // tmux hands option values back unexpanded, and a literal `~` directory
        // is a baffling thing to find in your home.
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand_home("~/notes.md"), PathBuf::from(&home).join("notes.md"));
        assert_eq!(expand_home("/abs/notes.md"), PathBuf::from("/abs/notes.md"));
        assert_eq!(expand_home("~weird"), PathBuf::from("~weird"), "only ~/ expands");
    }
}
