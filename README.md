# tmux-agent-mgr

A tmux sidebar for watching AI coding agents across every session and window — and
a general session/window switcher, because once you have the list you may as well
navigate with it.

Two things it tries to get right:

- **It can stay open.** The sidebar is a real tmux pane, not an overlay, so you can
  leave it beside your work all day.
- **It doesn't flicker.** Rendering is pure and the loop draws only when the output
  actually changed. An idle sidebar writes nothing to the terminal at all — no
  repaint on a timer, and exactly one `terminal.clear()` in the codebase, in the
  resize path where the geometry genuinely moved.

It works with **zero setup**: agents are detected from the process table and the
pane's visible screen, so nothing needs installing into Claude Code or Codex.
Installing the [Claude Code hooks](#optional-claude-code-hooks) is what upgrades
those inferences into things the agent tells you outright.

## Install

With [TPM](https://github.com/tmux-plugins/tpm), in `~/.tmux.conf`:

```tmux
set -g @plugin 'alexciarlillo/tmux-agent-mgr'
```

Then `prefix + I`. The plugin builds itself on first load if a binary isn't
present, so you need a Rust toolchain (`cargo`) available once. Manually:

```sh
git clone https://github.com/alexciarlillo/tmux-agent-mgr ~/.tmux/plugins/tmux-agent-mgr
cd ~/.tmux/plugins/tmux-agent-mgr && cargo build --release
tmux source ~/.tmux.conf
```

Requires tmux 3.0+. The popup needs 3.3+ (`display-popup -B -E`); below that
everything else still works and the popup key is simply not bound.

### Optional: Claude Code hooks

Passive detection can only ever see *working / blocked / idle*. Claude Code's hooks
report the rest — permission mode, why a pane is blocked, live subagents, task
progress — and they turn "blocked, I think" into "blocked, it said so". Register
this repo as a Claude Code plugin:

```
/plugin marketplace add ~/.tmux/plugins/tmux-agent-mgr
/plugin install tmux-agent-mgr@alexciarlillo
```

Restart the agent and its row gains the extra lines. Nothing else changes: hooks
apply per pane, panes without them keep using passive detection, and the sidebar
marks which source it is reading. If you would rather wire it by hand, point your
`~/.claude/settings.json` hooks at `hook.sh` — see `hooks/hooks.json` for the
trigger names and the argument each one passes.

Hooks are strictly additive and never load-bearing: `hook.sh` exits 0 when the
binary is missing or you aren't in tmux, and the daemon clears a pane's hook state
the moment no agent process remains under it — so a `kill -9`'d agent can't leave a
pane latched to "running".

## Use

| Key | |
|---|---|
| `prefix + e` | toggle the sidebar in this window |
| `prefix + E` | toggle it in every window |
| `prefix + o` | jump into the sidebar, or back out to where you were |
| `C-n` | open the full-screen popup (no prefix) |

Opening the sidebar puts you in it. `prefix + E` focuses only the one in the window
you pressed it in — the other windows keep the pane they were on. `prefix + o` is the
round trip afterwards: into the sidebar, and out again to the pane you came from.

The **sidebar** is narrow and persistent. The **popup** is the same list
full-screen with a live preview of the selected window beside it, and it closes
itself as soon as you jump somewhere — it's a chooser, not a place to live.

The cursor follows tmux: move between panes, windows or sessions by any means — the
keys below, `C-h/j/k/l`, your own bindings, the mouse — and the highlighted row is
the pane you are actually in. Move it yourself and it stays where you put it.

The preview mirrors the window's layout *and* its colours, because recognising a
window at a glance is mostly recognising its palette. Text the pane didn't colour is
drawn muted, so the mirror still reads as a mirror. The selected pane's outline is
drawn in the accent colour and tmux's own active pane keeps a heavier border, so
moving through the panes of one window is something you can see.

### Inside the list

| Key | |
|---|---|
| `j` `k` `↓` `↑` | next / previous pane |
| `N j` / `N k` | move N panes (`10j`) |
| `H` `L` | previous / next session |
| `J` `K` | move this session down / up the list |
| `g` `G` | first / last pane |
| `N G` | go to pane N |
| `C-d` `C-u` | page down / up |
| `Enter` | jump to the selected pane |
| click | jump to the pane you clicked; a header or blank row just takes focus |
| `Tab` | cycle the status filter: all → working → blocked → done |
| `/` | search; `Enter` keeps the filter, `Esc` clears it |
| `R` | rename the selected window |
| `r` | refresh now |
| `a` | jot down a note |
| `n` | give the notes panel the keyboard |
| `?` | keymap |
| `q` `Esc` | close |

A gutter shows each pane's distance from the cursor, which is what makes `10j`
something you can aim rather than guess at. `H` from mid-session goes to the top of
that session first, then to the session above.

Search matches a pane's session, window, command, git branch, worktree and agent
name, so `ops`, `claude` and `auth` all find what you'd expect. Terms are ANDed, and
it composes with the status filter rather than replacing it.

### Notes

A scratchpad at the bottom of the sidebar, for the thing you notice about *another*
project while you're deep in this one. It is global — not per-pane, not per-session —
because the whole point is getting something out of your head without first working
out where it belongs.

`a` opens a prompt from anywhere in the sidebar; `n` hands the panel the keyboard.

| Key | | |
|---|---|---|
| `a` | anywhere | write a note |
| `n` | in the list | focus the panel |
| `j` `k` `g` `G` | in the panel | move between notes |
| `Space` | in the panel | mark done |
| `Enter` | in the panel | read the full note in a popup |
| `e` | in the panel | open the note in `$EDITOR` |
| `d` | in the panel | delete it, after a `y/n` confirmation |
| `n` `q` `Esc` | in the panel | back to the list |

`a` is for getting a title down fast; `e` is where the body gets written, in your
own editor, in the markdown the file is already made of. Both popups are centred over
the window. `d` names the note it is about to delete and only `y` goes ahead — there
is no undo, the file is the only copy. Emptying a note in the editor and saving
deletes it too.

`e` runs `$VISUAL`, then `$EDITOR`, then falls back to `vi`. Worth setting, because
`vi` with no config is a rough place to land unexpectedly. One tmux wrinkle: a popup
gets the **server's** environment, not your shell's, so exporting it in your profile
only reaches servers started afterwards — `tmux set-environment -g EDITOR <yours>`
fixes one that is already running.

The panel is a mode, so `Space` marks a note done while it still jumps to a pane one
row above. It takes at most a quarter of the sidebar and at most 12 rows, shrinks to
however many notes you have, never leaves the pane list under 6 rows, and disappears
entirely when the scratchpad is empty.

Notes live in one markdown file — `${XDG_DATA_HOME:-~/.local/share}/tmux-agent-mgr/notes.md`,
or wherever `@agent_mgr_notes_file` points:

````markdown
## [ ] auth redirect drops ?next
<!-- id=dkkrw2aliwa0i82 t=1770000000 from=blueberry:3 -->
The 302 out of /callback loses the `next` param.

## Repro

1. log out
2. hit a protected route
3. land on `/` instead

```sh
curl -i localhost:3000/callback
```

## [x] starship timeout
Raised to 1500ms.
````

The sidebar shows titles; everything under one is its body, and `Enter` is how you
read it. **A note heading is `## ` plus a checkbox, and nothing else is** — so a body
can use any markdown it likes, `##` sections included, without a subsection becoming
the next note.

Markdown rather than a private format because you are not the only writer: edit it in
`$EDITOR`, or point an agent at it. The panel notices within a second either way, and
nothing here addresses a note by position — every action finds its note by `id=`, so
another pane adding or deleting one while you are mid-edit cannot redirect it.

`id=` is the one reserved key. It is written for you, it must stay lowercase
alphanumeric, and a value that isn't gets replaced on the next write rather than kept
— an id you can retype is not an identity. Every other key in that comment is yours
and is preserved untouched. From a shell:

```sh
agent-mgr note add "auth redirect drops ?next"
agent-mgr note add "check the fence case" --body -   # body on stdin
agent-mgr note list                    # index, open/done, id, title
agent-mgr note show 1
agent-mgr note edit 1                  # $EDITOR; this is what `e` runs in the popup
agent-mgr note edit --id=dkkrw2aliwa0i82
```

An index is fine when you read it from `note list` and use it a second later.
Anything that holds a reference across time — a script, a hook, an agent — should
use `--id`, because a deletion from another pane renumbers everything below it. The
sidebar always uses ids for exactly that reason.

### Reading a row

```
 ◉ claude  plan               1m12s
   feat/auth ~wt-auth
   ▸ permission prompt
   ▸ Explore ×2, Plan
   ▸ tasks 1/3
```

`●` working · `◉` blocked, needs you · `●` finished and unread · `○` idle ·
`✕` errored · `·` no agent. A `┃` marks the pane tmux is actually focused on.

A window holding one pane — most of them — has no header line of its own; its name
sits on the pane's row instead, and only a split window is listed as two levels:

```
 dev
 ◉ 1 api claude  plan         1m12s
     feat/auth ~wt-auth
   2 editor
   ┃○ 0 nvim
    ● 1 claude                  14s
```

The lines below the first appear only when there is something to say. The branch
row comes from git; the permission badge, wait reason, subagents and task progress
come from [agent hooks](#optional-claude-code-hooks). A trailing `?` on the agent
name means the *blocked* reading came from a heuristic rather than from the agent
itself.

Window tabs also carry a rolled-up status glyph, appended to your existing
`window-status-format` without replacing it.

### Global navigation

On by default, and passed through to Vim when the focused pane is running it:

| Key | |
|---|---|
| `C-h` `C-l` | move pane left/right, or wrap to the previous/next window at an edge |
| `C-j` `C-k` | previous / next session |

## Configure

Set these **before** the plugin loads; it only fills in what you haven't.

| Option | Default | |
|---|---|---|
| `@agent_mgr_width` | `20%` | sidebar width: columns, or a percentage of the window |
| `@agent_mgr_min_width` | `24` | lower clamp |
| `@agent_mgr_max_width` | *unset* | upper clamp; unset means uncapped |
| `@agent_mgr_position` | `left` | `left` or `right` |
| `@agent_mgr_agents_only` | `off` | list only panes running an agent |
| `@agent_mgr_resurrect` | `on` | re-open sidebars after a tmux-resurrect restore |
| `@agent_mgr_tab_status` | `on` | status glyph in window tabs |
| `@agent_mgr_nav` | `on` | the `C-h/j/k/l` bindings above |
| `@agent_mgr_key` | `e` | prefix key toggling the sidebar here |
| `@agent_mgr_key_all` | `E` | prefix key toggling it everywhere |
| `@agent_mgr_key_focus` | `o` | prefix key jumping into the sidebar and back; `none` binds nothing |
| `@agent_mgr_key_popup` | `C-n` | prefix-less popup key; `none` binds nothing |
| `@agent_mgr_notes_file` | *XDG default* | where the scratchpad lives; a leading `~/` is expanded |

Colors take a `#RRGGBB` or a 0–255 palette index:
`@agent_mgr_color_{accent,session,working,blocked,idle,done,error,branch}`.

```tmux
set -g @agent_mgr_position right
set -g @agent_mgr_width 28
set -g @agent_mgr_color_accent '#89b4fa'
```

### With tmux-resurrect / tmux-continuum

Sidebars come back with your sessions, at the width and position they had. Nothing to
configure — the plugin appends `agent-mgr restore` to resurrect's
`@resurrect-hook-post-restore-all`, keeping whatever you already had in it. Turn it
off with `set -g @agent_mgr_resurrect off`.

Don't bother adding the sidebar to `@resurrect-processes` — it cannot work. resurrect
finds a pane's program by looking at the *children* of the pane's process, and the
sidebar has none, because it **is** the pane's process. So resurrect saves no command
for it and skips it on restore, whatever that option says. Restoring the pane is the
part resurrect gets right; only the program needs putting back, which is all
`agent-mgr restore` does.

## How it works

One background daemon per tmux server polls every pane, infers status, and caches
the result into tmux pane options. Every sidebar and popup then reads that from a
single `list-panes` call — so ten open sidebars cost one poller, not ten. Focus
changes arrive as a signal from a tmux hook rather than being discovered by polling,
which is why it feels immediate while drawing almost never.

No tmux or git subprocess ever runs on the UI thread.

Status comes from two sources kept in separate pane-option namespaces. Hooks write
raw facts; the daemon reconciles them with passive detection and is the only writer
of the resolved status. Hook state wins for a pane while it is fresh and an agent
process is still alive there, and the daemon owns liveness for both — which is what
lets a hook write be trusted without it ever getting stuck.

Subagents inherit their parent's `$TMUX_PANE`, so while any subagent is live we
can't tell a parent's event from a child's. The ingest then accepts only the
assertions that hold either way: "work is happening" and "somebody needs you" pass
through, while "the turn is over" waits for the subagent list to drain.

## Status

Working: passive detection (Claude Code and Codex), Claude Code hooks, the sidebar,
the popup with window preview, navigation, search, filters, rename, session reorder,
tab glyphs.

Not yet: the `▸ bg` row has no writer — a backgrounded shell command is only visible
through `PostToolUse`, which fires on every tool call and is deliberately not
registered.

## Development

```sh
cargo test
cargo clippy --all-targets
```

The tests are pure — they never issue a tmux command that changes anything, which
is deliberate: several actions (`Enter`, `R`, `J`) would otherwise move or rename
things on whatever tmux server is hosting the test run. Each has its decision split
out from its I/O, and the tests exercise the decision.

`agent-mgr daemon --once` prints resolved per-pane state as TSV — the quickest way
to check detection without the TUI.

When testing against a live tmux, **use a throwaway socket**:

```sh
tmux -L probe -f /dev/null new-session -d -s t -x 150 -y 40
tmux -L probe new-window -t t "$PWD/target/release/agent-mgr"
tmux -L probe kill-server          # only ever with -L
```

`-f /dev/null` so you aren't looking at your own config, and `-L` on *every* call —
a bare `tmux` resolves through `$TMUX` to your real server. Run the plugin's own
commands inside the probe server for the same reason.

A hook can be fired by hand without an agent: it reads its payload from stdin and
attributes it to `$TMUX_PANE`, exactly as it inherits both from Claude Code.

```sh
echo '{"notification_type":"permission_prompt"}' \
  | TMUX_PANE=%3 sh hook.sh claude notification
```

Note that the daemon clears hook state on a pane with no agent process under it, so
to watch a hook survive a poll the pane needs a process with an agent-shaped name —
`ln -s "$(command -v sleep)" /tmp/fakebin/claude` is enough.

`AGENT_MGR_DEBUG_FRAMES=/tmp/frames` makes the binary write its draw count on exit,
so the no-flicker claim is something you can check rather than take on faith.
