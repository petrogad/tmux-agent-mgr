# Working in this repo

`README.md` is for people *using* the plugin. This file is for whoever is *changing*
it: what it is for, the invariants that must not be broken, and the traps that have
already cost someone a debugging session.

## What this is, and what it deliberately is not

One tmux plugin that (a) watches AI coding agents across every session and window and
(b) doubles as a session/window navigator. It exists because two prior plugins each
got half of it right:

- `tmux-agent-sidebar` (~32k LOC) could stay persistently open and showed genuinely
  useful per-agent detail, but carried a great deal we don't want.
- `tmux-agent-switcher` (~5k LOC) had much better vim-style navigation, but was
  popup-only and **flickered**, because it redrew every 50 ms and forced a full
  `terminal.clear()` every 500 ms. Inside a `display-popup` overlay tmux must
  recomposite the whole region against the pane behind it, and that is the flicker.

Both are still on disk as reference:
`~/GitHub/alexciarlillo/tmux-agent-{sidebar,switcher}/main`. Read them for ported
heuristics, but treat their agent-payload parsing as **stale** — see the hooks
section.

Explicitly out of scope, and the reason this repo is ~10k lines instead of 32k:
worktree spawn/teardown, a bottom panel, git tab / PR fetching, an activity log, a
virtual pet, desktop notifications, OSC52 clipboard, HTML capture, an install wizard,
a docs website. New features are weighed against "does a monitoring sidebar need to
do this?" — usually the answer is no.

## Commands

```sh
cargo test                  # 357 tests, all pure — no tmux server required
cargo clippy --all-targets  # must be warning-clean
cargo build --release
./target/release/agent-mgr daemon --once   # resolved per-pane state as TSV
```

`cargo fmt` is **not** part of the loop. The code is hand-formatted in a style close
to but not identical to rustfmt's, and 14 files currently differ from it. Running
`cargo fmt` inside a feature change buries the real diff in unrelated churn; if the
repo should be normalized, that is its own commit.

## Invariants

These are the load-bearing ones. Each is enforced by a test, and each was a bug
somewhere else first.

1. **`terminal.clear()` appears exactly once in the crate, in the `Event::Resize`
   arm** (`app/mod.rs`). Never on a timer. `grep` finds four hits; three are doc
   comments explaining this rule.
2. **Rendering is pure and drawing is change-driven.** `ui::rows::build` takes a
   tree and returns lines with no I/O. Each pass hashes the plain text of the output
   (`app::fingerprint`) and draws only when it differs. An idle sidebar draws **zero**
   frames — measured, see below.
3. **The spinner is the only self-animating thing**, and it only advances while a
   visible pane is Working. This is what makes invariant 2 observable rather than
   theoretical.
4. **No tmux or git subprocess ever runs on the UI thread.** Everything is on
   `app::worker`, which feeds a channel. A slow `git` on a network mount must never
   stall input or leave a half-painted frame.
5. **Every rendered line is exactly the requested width, counted in display
   columns.** Not characters — an emoji is one char and two columns. A line one cell
   too wide wraps, shifts everything below it, and is precisely what forced the old
   plugin into its periodic clear.
6. **The daemon is the only writer of resolved status; hooks only ever write the
   hook-owned namespace.** See below.
7. **A hook never fails.** Every path in `hook::cmd_hook` returns 0 and swallows
   errors. A non-zero exit surfaces inside the user's agent session.

## Module map

```
main.rs        argv dispatch: (no args)=sidebar | popup | toggle | toggle-all
               | focus | resize | auto-close | daemon [--once] | hook <agent> <event>
model.rs       AgentKind/State/Status, PermissionMode, the tree types, and the
               option string round-trips. Both status vocabularies merge here.
detect.rs      passive detection: process name -> agent, title + screen -> state.
               Heuristic and version-sensitive by nature. Ported from the switcher.
               See "Passive detection per agent" below — the two agents share
               almost none of this path.
daemon.rs      one poller per tmux server: precedence, directional debounce, run
               timer, unread marker, diff-only option writes, window tab icons,
               agent identification and liveness via ProcessTree, per-agent
               capture range (capture_screen), singleton guard.
hook/mod.rs    `hook <agent> <event>`: stdin JSON, pane state read, apply writes.
hook/claude.rs the Claude Code mapping. Pure: plan(event, payload, state, now).
git.rs         branch + worktree for a path, TTL-cached including negative results.
pane.rs        sidebar pane lifecycle: resolve_width's clamp order, which side the
               split lands on, should_kill_window's multi-client guard, and who
               takes focus on open (Focus, focus_action).
nav.rs         counted motions, session edges, relative numbering.
search.rs      the `/` query: terms ANDed across session/window/command/branch/
               worktree/agent.
preview.rs     popup-only window mirror: capture, ANSI parse, fixed-grid compose.
app/mod.rs     the event loop, dirty-flag redraw, fingerprint, rebuild, and
               apply_focus — the cursor follows tmux focus, but only on a change.
app/worker.rs  the only place subprocesses run while the TUI is open.
app/input.rs   key handling, including the vim-ish modes.
ui/*           draw dispatch by Surface, row composition, help page, display-width
               text helpers, theme.
tmux/*         commands (two error conventions), options (every key as a const),
               query (one `list-panes -a` builds the whole hierarchy, and
               focused_pane reads which pane the *client* is on from the same rows —
               via client_session, which is why a sidebar follows you out of its
               own session instead of pointing at where you used to be).
```

Non-Rust, and just as load-bearing:

```
tmux-agent-mgr.tmux   TPM entry: version guard, publishes @agent_mgr_bin, sources
                      the conf. A bash script TPM *executes* — not a tmux config
                      to `source-file`; sourcing it yields "invalid environment
                      variable".
agent-mgr.conf        option defaults, keybindings, tmux hooks, tab glyph.
hook.sh               /bin/sh dispatcher; resolves the binary fresh per fire.
hooks/hooks.json      Claude Code hook registrations.
.claude-plugin/       plugin.json + marketplace.json, for `/plugin marketplace add`.
```

## Passive detection per agent

The two agents look nothing alike through this lens, and assuming otherwise is how
Codex detection sat broken. Verified against Claude Code 2.x and Codex 0.144.3.

| | Claude Code | Codex |
|---|---|---|
| Foreground command | `claude`, or a bare semver for native installs | `node`, for the npm shim |
| Sets an OSC title | yes — braille spinner while working | **no, ever** |
| UI anchor | input box at the bottom | grows down from the top |
| Working signal | spinner in the title | `esc to interrupt` on screen |
| Blocked signal | `❯`-cursor numbered menu | `›`-cursor numbered menu |

Three consequences worth keeping straight:

- **Codex is identified from the process tree, not the pane's command.** The npm
  install runs `node /usr/local/bin/codex`, which spawns the real binary as a
  child, so tmux reports `node`. `daemon::read_pane` falls back to
  `ProcessTree::agent_under`, gated by `detect::may_host_agent` so a workspace of
  plain shells still never pays for a `ps`. Note the `ps -Ao …,comm=,args=` layout:
  `args` repeats `argv[0]`, so naming the agent needs `agent_from_command_line`,
  which reads `argv[1]` and requires a `/` in it — otherwise `rg codex` is an agent.
- **A Codex pane title is never evidence.** Whatever tmux reports came from the
  launching shell. `state_from_title` used to read any non-empty title as idle,
  which pinned every Codex pane to idle forever — mid-turn included — because the
  screen was then never captured at all.
- **Codex is read from the whole visible screen, Claude from a trailing window**
  (`daemon::capture_screen`). Codex is top-anchored until its transcript fills the
  pane, so a fresh session in a 92-row pane puts the status line around row 12,
  which a bottom-anchored window misses. That needs no staleness bound because the
  status line is transient — erased the frame a turn ends, never left in the
  transcript.

To re-derive any of this, don't read the reference repos — their Codex heuristics
predate the current TUI (they look for an `Action Required` title and
`press enter to confirm or esc to cancel`; today's wording is `… or esc to go
back`). Capture a live session instead, per "Live verification" below.

## The two status sources

| | passive (`detect.rs`) | hooks (`hook/`) |
|---|---|---|
| Setup | none | install the Claude Code plugin |
| Sees | working / blocked / idle | + permission mode, wait reason, subagents, tasks, cwd |
| Nature | heuristic, reads the agent's UI | assertions from the agent itself |

Three pane-option namespaces, and keeping them separate is what makes precedence
explicit (`tmux/options.rs`):

1. **daemon-owned** (`@agent_mgr_state`, `_source`, `_seen`, `_run_started_at`, …) —
   the resolved status. Daemon writes, everything else reads.
2. **hook-owned** (`@agent_mgr_hook_state`, `_permission_mode`, `_wait_reason`,
   `_subagents`, `_task_*`, `_cwd`, …) — raw facts. Listed once in
   `HOOK_OWNED_PANE_OPTIONS` so the sweep can't miss a key.
3. **user config** (`@agent_mgr_width`, …) — read-only from our side.

Precedence, in `daemon::read_pane`: hook state wins while it is fresh (15 min) *and*
an agent process is still alive under the pane; otherwise passive. The daemon owns
liveness for **both** — when `ProcessTree` finds no agent under a pane, hook options
are swept, so a `kill -9`'d agent cannot leave a pane latched to "running".

Consequence worth internalising: **while hooks are fresh, passive detection is not
consulted at all.** A hook event we drop is therefore a *wrong* answer, not a missing
one. That is why the subagent gate is asymmetric.

### The subagent gate

Subagents inherit the parent's `$TMUX_PANE` and fire their own lifecycle hooks
through it, so a child's `Stop` is indistinguishable from the parent's. The rule:
**accept only assertions that stay true whichever of them sent it.**

- Pass always: `UserPromptSubmit`, `PermissionDenied`, a blocking `Notification` — a
  child running means the pane is running; a child's permission prompt is still the
  user's to answer, in this pane.
- Gated until `@agent_mgr_subagents` drains: `SessionStart`, `SessionEnd`, `Stop`,
  `StopFailure`, and an `idle_prompt` notification.

The reference repo gates all of them; that looked right until a live probe showed it
swallowing a permission prompt, leaving the pane claiming "working" while it waited.
Residual failure: a subagent that dies without its `SubagentStop` stops the pane
reporting "done" until hook state ages out. Better than a live pane wiped by a child.

### Hook payloads

Field names come from <https://code.claude.com/docs/en/hooks>. **Check the docs, not
the reference repo** — it reads `end_reason` where the current docs say `reason`, and
predates `notification_type` values like `permission_prompt`. Unknown values degrade
to silence: an unrecognized notification type is a heartbeat and nothing more, because
inventing a state is how a pane ends up claiming it needs you when it doesn't.

`@agent_mgr_bg_cmd` (the `▸ bg` row) has **no writer** by design. A backgrounded
shell is only visible through `PostToolUse`, which fires on every tool call and has
no completion event, so the row would go stale until the next turn.

## The tmux boundary

Most bugs in this plugin have lived here, not in the Rust.

- **Every option key is a const in `tmux/options.rs`**, including keys only the conf
  reads (`conf_only`). Tests assert the shipped conf and `.tmux` still mention them —
  a rename would otherwise silently disable a feature with nothing to notice.
- **One `list-panes -a -F` builds the whole tree.** Every field is wrapped in
  `#{q:…}` and split with `split_fields`, so a `|` in a window name or path cannot
  shift columns. The field-index consts and the format list must be edited together;
  a test enforces it.
- **`run_tmux` vs `tmux_output`**: `Option` where failure is ordinary (a pane
  vanished, an option was never set), `Result` where failure means the server is gone
  and the caller should stop.
- **`display_message` trims the whole output**, so it cannot read several options at
  once — a leading empty field vanishes and shifts every value after it. See
  `hook::read_pane_state`.
- **In the conf, write `##{...}`, not `#{...}`,** for anything that must be evaluated
  at key-press time. tmux expands formats in a `run-shell` command *before* the shell
  sees it, so a single `#` freezes the value as of config load.
- **`run-shell` uses `/bin/sh`, which is dash here.** `${var//x/}` and other bashisms
  are a syntax error, the command aborts, and run-shell discards the message — the
  feature silently does nothing. Use `sed`.
- **tmux has no "replace this hook" verb.** Our hooks are removed before being
  re-added, or a config reload stacks another copy and fires them N times.

## Testing conventions

- **Tests never mutate any tmux server.** Several actions (`Enter`, `R`, `J`) issue
  real `switch-client` / `rename-window` and would move the developer's own client.
  Each has its decision split from its I/O — `activation_target()`, `take_rename()`,
  `hook::claude::plan()` — and the tests exercise the decision. Keep that split when
  adding an action; it is the reason the suite is safe to run anywhere.
- **Name a test after the behaviour it pins**, as a sentence, and say in a comment
  *why* the behaviour matters — ideally which bug it prevents. `only_changed_fields_are_written`
  beats `test_status_updates`.
- **Drift tests guard every boundary the compiler cannot see.** There are several,
  and they are the cheapest tests in the repo:
  - `tmux/options.rs` — option consts vs `agent-mgr.conf` and `tmux-agent-mgr.tmux`;
    the daemon and hook namespaces not overlapping.
  - `hook/claude.rs` — `HOOK_REGISTRATIONS` vs the parsed `hooks/hooks.json`, vs
    `Event::ALL`, vs the arms of `plan()`; every key `plan()` writes being hook-owned;
    hook state labels being ones `model::parse_hook_state` accepts; `plugin.json`'s
    version vs Cargo's.
  - `ui/help.rs` — every documented binding existing in the keymap.
  If you add a string that another program reads, add the test with it.

## Live verification

**Never run `tmux kill-server`, and never mutate the default socket** — that is what
killed the original session. Use a throwaway server, and pass `-L probe` to *every*
call: a bare `tmux` resolves through `$TMUX` to the real one.

```sh
tmux -L probe -f /dev/null new-session -d -s t -x 150 -y 40
tmux -L probe set-option -g @agent_mgr_bin "$PWD/target/release/agent-mgr"
tmux -L probe kill-server
```

`-f /dev/null` or you inherit the user's real config and cannot tell your bindings
from theirs. Run the plugin's own commands *inside* the probe server
(`tmux -L probe new-window "…/agent-mgr daemon --once"`), for the same `$TMUX` reason.

Three recipes that have earned their keep:

- **Fire a hook by hand.** It reads stdin and attributes to `$TMUX_PANE`, exactly as
  it inherits both from Claude Code, so you can aim one at another pane:
  `echo '{"notification_type":"permission_prompt"}' | TMUX_PANE=%0 sh hook.sh claude notification`.
  The daemon sweeps hook state on a pane with no agent process, so give the target an
  agent-shaped one: `ln -sf "$(command -v sleep)" /tmp/fakebin/claude`.
- **Capture the popup.** `agent-mgr popup` takes its surface from argv, so running it
  in an ordinary pane puts the popup TUI somewhere `capture-pane -peN` can see it.
  `display-popup` is only how the keybinding launches it.
- **Measure the flicker claim.** `AGENT_MGR_DEBUG_FRAMES=/tmp/frames agent-mgr`
  writes the draw count on exit. An idle sidebar has measured `frames=2` over 35 s
  and 25 s. If a change makes that number grow, invariant 2 is broken.
- **Re-derive an agent's real screens.** Launch the agent in the probe server with
  its prompt in argv so it auto-submits, and diff-capture in a loop beside it,
  saving a file each time `capture-pane -p` *plus* `pane_title` and
  `pane_current_command` change. One turn yields every transition — a `sleep`-style
  prompt buys the working frames, and the directory-trust prompt on first launch in
  a fresh cwd is a free modal. What matters is which parts *don't* vary across the
  frames: that is the only thing safe to match on.
- **Exercise a state without running the agent.** Copy `sleep` to
  `/tmp/fakebin/{node,codex}` and run a pane as
  `cat captured-screen.txt; /tmp/fakebin/codex 900 & exec /tmp/fakebin/node 900` —
  a pane whose command is `node`, whose child is `codex`, showing a real captured
  screen. That drives the whole chain (`may_host_agent` → `ProcessTree` →
  `capture_screen` → state) with no model call. It cannot test *teardown*, though:
  `sleep` never reaps, so the killed child lingers as a zombie that `ProcessTree`
  still counts.

## Preview specifics

`preview.rs` composes into a `width × height` grid of `Cell { ch, attrs }` — one cell
per **display column** — clamping every write to the target pane's rectangle. That is
what makes invariant 5 structural rather than aspirational, so keep new work inside
the grid rather than beside it.

Colour comes from `capture-pane -peN`, parsed into `Attrs` at the edge. The invariant
is not "we handle the escapes we know about" but **"no escape survives parsing"** —
one reaching the terminal would latch a colour or move the real cursor and corrupt
the frame *around* the preview, in a way no width assertion catches. `consume_escape`
therefore swallows CSI whole (any final byte, SGR or not), OSC on either BEL or ST,
and two-byte escapes.

Rendering choices live in `ui::preview_style`: palette indices stay indices so the
preview follows the user's terminal theme; uncoloured text renders muted, so a pane
emitting no colour previews as it always did; borders carry default attrs so the
frame doesn't flicker colours as a pane scrolls; bold is dropped, reverse is kept
because it is how both agents mark a selected menu row.

## State of the work

All four planned phases are done: passive sidebar, popup + navigation, Claude Code
hooks, polish (git rows, tab glyphs, options, preview colour).

On the `notes-panel` branch of this fork, a fifth: the global scratchpad
(`notes.rs`), its sidebar panel (`ui/notes.rs`), and the markdown highlighter for
its detail popup (`highlight.rs`). Upstream's out-of-scope list names "a bottom
panel", which is why it lives here rather than in a PR. Its living doc is
`~/agents/handoffs/agent-mgr-notes-panel-handoff.md` — it carries the reasoning
behind the decisions that are not obvious from the code, including why the panel
is a focus mode and why a note heading requires its checkbox.

Two earlier docs this section used to point at,
`~/agents/handoffs/tmux-agent-mgr-phase1-handoff.md` and
`~/agents/plans/tmux-agent-mgr-plan.md`, no longer exist in the active dirs or in
`archives/`. Do not go looking for them.

Known gaps: the `▸ bg` row has no writer (deliberate, above), and the flicker check's
companion case — that a *working* agent advances the spinner and nothing more — has
never been run, because it needs a real agent in a pane.

Two things about Codex are still unverified against a live agent. Its **command
approval** modal has only been inferred: it is the same `list_selection_view`
widget as the directory-trust prompt and `/model`, both of which were captured and
do read as Blocked, but this devspace's `/etc/codex/managed_config.toml` pins
`approval_policy` to `Never`, so the modal itself could not be produced. And
`ProcessTree` counts a **zombie** agent as live — `ps` still reports
`[codex] <defunct>` with `comm=codex`. Pre-existing, and harmless in practice
because the npm shim reaps its child, but it does mean a non-reaping wrapper could
latch a pane. Excluding zombies means adding `stat=` to the `ps` format.
