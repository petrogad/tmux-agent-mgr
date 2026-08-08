//! Claude Code's hook interface, mapped onto our pane options.
//!
//! [`HOOK_REGISTRATIONS`] is the single source of truth for which triggers we ask
//! for. It is checked against the shipped `hooks/hooks.json` — the file Claude Code
//! actually reads — by [`tests::hooks_json_matches_the_registration_table`], because
//! nothing else connects the two: one is Rust, the other is JSON read by another
//! program entirely.
//!
//! Field names below come from the hooks reference at
//! <https://code.claude.com/docs/en/hooks>. Where a payload might carry either an
//! old or a new spelling we read both, and an unrecognized value degrades to "say
//! nothing" rather than to a guess — passive detection is still running underneath
//! and is the better answer than a confident wrong one.
//!
//! ## The subagent gate
//!
//! Subagents inherit the parent's `$TMUX_PANE` and fire their own lifecycle hooks
//! through it, so while any subagent is live we cannot tell a parent's event from
//! a child's. The rule that follows from that: **accept only the assertions that
//! stay true whichever of them sent it.**
//!
//! - "There is work in progress" and "somebody needs you" hold either way. A
//!   child running means the pane is running; a child's permission prompt is
//!   still the user's to answer, and it appears in the same terminal. So
//!   `UserPromptSubmit`, `PermissionDenied` and a blocking `Notification` always
//!   apply.
//! - "The turn is over", "it failed" and "the session is gone" do not. A
//!   subagent's `Stop` would park a busy pane at idle mid-turn. So `SessionStart`,
//!   `SessionEnd`, `Stop`, `StopFailure` and an `idle_prompt` notification are
//!   dropped until [`tmux::PANE_SUBAGENTS`] drains.
//!
//! Subagent and task events are keyed by id or are pure counters, so they always
//! apply — `SubagentStop` in particular must, or the list could never drain.
//!
//! The residual failure is a subagent that dies without its `SubagentStop`: the
//! pane then keeps reporting running/blocked but stops reporting "done" until the
//! daemon's 15-minute hook-staleness fallback hands the pane back to passive
//! detection, or its liveness sweep clears the pane outright. Both are far better
//! than a live pane wiped by a child's event.

use serde_json::Value;

use super::{PaneState, Write, has_field, json_str, sanitize};
use crate::tmux;

/// The agent name `hooks.json` passes as the first argument.
pub const AGENT: &str = "claude";

/// Labels written to [`tmux::PANE_HOOK_STATE`]. The daemon reads them back through
/// [`crate::model::parse_hook_state`]; [`tests::hook_state_labels_are_ones_the_model_parses`]
/// keeps the two ends honest.
const RUNNING: &str = "running";
const WAITING: &str = "waiting";
const IDLE: &str = "idle";
const ERROR: &str = "error";

/// `notification_type` for "the user has not typed for a while", which is Claude
/// sitting at its prompt rather than waiting on an answer.
const IDLE_PROMPT: &str = "idle_prompt";

/// Column budgets. A wait reason or a badge is rendered in a ~24-column sidebar,
/// and a cwd only has to survive being handed to `git`.
const REASON_LIMIT: usize = 64;
const ID_LIMIT: usize = 128;
const PATH_LIMIT: usize = 1024;

/// One Claude Code hook we register for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Event {
    SessionStart,
    SessionEnd,
    UserPromptSubmit,
    Notification,
    Stop,
    StopFailure,
    PermissionDenied,
    SubagentStart,
    SubagentStop,
    TaskCreated,
    TaskCompleted,
}

impl Event {
    /// Every variant, so the drift tests can enumerate the enum rather than trust
    /// a hand-maintained second list.
    pub const ALL: &'static [Event] = &[
        Event::SessionStart,
        Event::SessionEnd,
        Event::UserPromptSubmit,
        Event::Notification,
        Event::Stop,
        Event::StopFailure,
        Event::PermissionDenied,
        Event::SubagentStart,
        Event::SubagentStop,
        Event::TaskCreated,
        Event::TaskCompleted,
    ];

    /// The name `hooks.json` passes as our second argument: the trigger in
    /// kebab-case.
    pub fn cli_name(self) -> &'static str {
        match self {
            Self::SessionStart => "session-start",
            Self::SessionEnd => "session-end",
            Self::UserPromptSubmit => "user-prompt-submit",
            Self::Notification => "notification",
            Self::Stop => "stop",
            Self::StopFailure => "stop-failure",
            Self::PermissionDenied => "permission-denied",
            Self::SubagentStart => "subagent-start",
            Self::SubagentStop => "subagent-stop",
            Self::TaskCreated => "task-created",
            Self::TaskCompleted => "task-completed",
        }
    }

    pub fn from_cli_name(name: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|event| event.cli_name() == name)
    }

    /// `true` for events asserting that the turn is over, failed, or never
    /// started — the assertions that stop being true when a subagent is the one
    /// making them. See the module docs on the gate.
    ///
    /// [`Self::Notification`] is absent because it is both kinds at once: a
    /// permission prompt needs the user regardless of who raised it, while an
    /// `idle_prompt` claims the pane is quiet. [`notification`] gates that half.
    fn ends_the_turn(self) -> bool {
        matches!(
            self,
            Self::SessionStart | Self::SessionEnd | Self::Stop | Self::StopFailure
        )
    }
}

/// Binding between a Claude Code trigger name and the event we handle it as.
///
/// Nothing at runtime reads this — the same shape as
/// [`crate::tmux::conf_only`], and for the same reason. The strings it pins live
/// in `hooks/hooks.json`, on the far side of a boundary the compiler cannot see,
/// so the table exists to give the tests something typed to check that file
/// against.
#[allow(
    dead_code,
    reason = "the registration table is consumed by the drift tests, which check it against hooks/hooks.json"
)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HookRegistration {
    /// Trigger name as it appears in `hooks.json`.
    pub trigger: &'static str,
    pub event: Event,
}

/// Every hook we ask Claude Code for, and nothing more.
///
/// `PostToolUse` is deliberately absent: it fires on every tool call, and the only
/// thing we would do with it — an activity log — is explicitly out of scope. The
/// price is that [`tmux::PANE_BG_CMD`] has no writer, since a backgrounded shell
/// is only visible through that trigger.
#[allow(
    dead_code,
    reason = "read by the drift tests; see the note on HookRegistration"
)]
pub const HOOK_REGISTRATIONS: &[HookRegistration] = &[
    HookRegistration {
        trigger: "SessionStart",
        event: Event::SessionStart,
    },
    HookRegistration {
        trigger: "SessionEnd",
        event: Event::SessionEnd,
    },
    HookRegistration {
        trigger: "UserPromptSubmit",
        event: Event::UserPromptSubmit,
    },
    HookRegistration {
        trigger: "Notification",
        event: Event::Notification,
    },
    HookRegistration {
        trigger: "Stop",
        event: Event::Stop,
    },
    HookRegistration {
        trigger: "StopFailure",
        event: Event::StopFailure,
    },
    HookRegistration {
        trigger: "PermissionDenied",
        event: Event::PermissionDenied,
    },
    HookRegistration {
        trigger: "SubagentStart",
        event: Event::SubagentStart,
    },
    HookRegistration {
        trigger: "SubagentStop",
        event: Event::SubagentStop,
    },
    HookRegistration {
        trigger: "TaskCreated",
        event: Event::TaskCreated,
    },
    HookRegistration {
        trigger: "TaskCompleted",
        event: Event::TaskCompleted,
    },
];

/// What one hook fire should change about this pane. Pure: no tmux, no clock.
pub fn plan(event: Event, payload: &Value, state: &PaneState, now: u64) -> Vec<Write> {
    let children_live = !state.subagents.is_empty();
    if children_live && event.ends_the_turn() {
        return Vec::new();
    }
    // The agent in this pane is finished, so every fact we hold about it is now
    // stale. Nothing else to write, including the freshness stamp.
    if event == Event::SessionEnd {
        return vec![Write::Sweep];
    }

    let mut writes = vec![
        Write::Set(tmux::PANE_HOOK_AGENT, AGENT.to_owned()),
        Write::Set(tmux::PANE_HOOK_UPDATED, now.to_string()),
    ];
    context(payload, &mut writes);

    match event {
        Event::SessionStart => {
            writes.push(Write::Set(tmux::PANE_HOOK_STATE, IDLE.to_owned()));
            writes.push(Write::Unset(tmux::PANE_WAIT_REASON));
            // A fresh session owns neither the previous run's task list nor its
            // background command. The subagent list is left alone on purpose —
            // see the module docs on the gate.
            writes.push(Write::Unset(tmux::PANE_TASK_DONE));
            writes.push(Write::Unset(tmux::PANE_TASK_TOTAL));
            writes.push(Write::Unset(tmux::PANE_BG_CMD));
        }
        Event::UserPromptSubmit => {
            writes.push(Write::Set(tmux::PANE_HOOK_STATE, RUNNING.to_owned()));
            writes.push(Write::Unset(tmux::PANE_WAIT_REASON));
        }
        Event::Notification => notification(payload, children_live, &mut writes),
        Event::Stop => {
            writes.push(Write::Set(tmux::PANE_HOOK_STATE, IDLE.to_owned()));
            writes.push(Write::Unset(tmux::PANE_WAIT_REASON));
        }
        Event::StopFailure => {
            writes.push(Write::Set(tmux::PANE_HOOK_STATE, ERROR.to_owned()));
            writes.push(Write::Set(tmux::PANE_WAIT_REASON, failure_reason(payload)));
        }
        // An auto-mode classifier denied a tool call. The turn carries on — Claude
        // gets the denial as a tool result — so this is evidence of work, not of
        // waiting. If it does go on to ask the user, a Notification follows.
        Event::PermissionDenied => {
            writes.push(Write::Set(tmux::PANE_HOOK_STATE, RUNNING.to_owned()));
            writes.push(Write::Unset(tmux::PANE_WAIT_REASON));
        }
        Event::SubagentStart => subagent_start(payload, state, &mut writes),
        Event::SubagentStop => subagent_stop(payload, state, &mut writes),
        Event::TaskCreated => task_created(state, &mut writes),
        Event::TaskCompleted => task_completed(state, &mut writes),
        // Returned above, before the freshness stamp.
        Event::SessionEnd => {}
    }

    writes
}

/// Fields that ride along on most payloads.
///
/// Each is only written when the event actually reported it: `SessionStart` has no
/// `permission_mode`, and clearing the badge on every session start would make it
/// flicker off at the top of each turn. `permission_mode` present but default —
/// or empty — does unset it, because that is a real transition out of plan mode.
fn context(payload: &Value, writes: &mut Vec<Write>) {
    let session_id = sanitize(json_str(payload, "session_id"), ID_LIMIT);
    if !session_id.is_empty() {
        writes.push(Write::Set(tmux::PANE_SESSION_ID, session_id));
    }

    let cwd = sanitize(json_str(payload, "cwd"), PATH_LIMIT);
    if !cwd.is_empty() {
        writes.push(Write::Set(tmux::PANE_CWD, cwd));
    }

    if has_field(payload, "permission_mode") {
        let mode = sanitize(json_str(payload, "permission_mode"), REASON_LIMIT);
        // "default" is the no-badge case, and an unset option is how the renderer
        // spells that; storing the word would cost columns to say nothing.
        if mode.is_empty() || mode == "default" {
            writes.push(Write::Unset(tmux::PANE_PERMISSION_MODE));
        } else {
            writes.push(Write::Set(tmux::PANE_PERMISSION_MODE, mode));
        }
    }
}

/// A notification either means "you are being waited on" or it is informational.
/// Only the first kind is a state.
///
/// Being waited on is reported even when a subagent could be the sender: a
/// permission prompt raised on a child's behalf still appears in this pane and is
/// still the user's to answer. `idle_prompt` is the opposite claim and is gated
/// like the other turn-ending events.
fn notification(payload: &Value, children_live: bool, writes: &mut Vec<Write>) {
    let kind = sanitize(json_str(payload, "notification_type"), REASON_LIMIT);

    if notification_blocks(&kind) {
        writes.push(Write::Set(tmux::PANE_HOOK_STATE, WAITING.to_owned()));
        writes.push(Write::Set(tmux::PANE_WAIT_REASON, kind));
    } else if kind == IDLE_PROMPT && !children_live {
        writes.push(Write::Set(tmux::PANE_HOOK_STATE, IDLE.to_owned()));
        writes.push(Write::Unset(tmux::PANE_WAIT_REASON));
    }
    // Everything else — `auth_success`, `elicitation_complete`,
    // `elicitation_response`, `agent_completed`, and anything added upstream
    // later — is recorded as a hook heartbeat and nothing more. Inventing a state
    // for an unknown notification is how a pane ends up claiming it needs you
    // when it doesn't.
}

/// Whether a `notification_type` means Claude has stopped and is waiting on the
/// user.
///
/// The `contains` arm is deliberate: the permission prompt has already been
/// renamed once (`permission` → `permission_prompt`), and a pane silently no
/// longer reporting "blocked" is a much worse outcome than one whose reason text
/// reads slightly oddly.
fn notification_blocks(kind: &str) -> bool {
    matches!(
        kind,
        "permission_prompt" | "elicitation_dialog" | "agent_needs_input"
    ) || kind.contains("permission")
}

/// The label for a failed turn: the error category if we have one, else its
/// message. `error`/`error_details` are the older spellings.
fn failure_reason(payload: &Value) -> String {
    for key in ["error_type", "error", "error_message", "error_details"] {
        let value = sanitize(json_str(payload, key), REASON_LIMIT);
        if !value.is_empty() {
            return value;
        }
    }
    "failed".to_owned()
}

fn subagent_start(payload: &Value, state: &PaneState, writes: &mut Vec<Write>) {
    let id = sanitize(json_str(payload, "agent_id"), ID_LIMIT);
    let kind = sanitize(json_str(payload, "agent_type"), REASON_LIMIT);
    // Without an id there is no way to ever remove the entry, and a subagent list
    // that only grows is what keeps the lifecycle gate shut forever.
    if id.is_empty() {
        return;
    }
    let kind = if kind.is_empty() {
        "agent".to_owned()
    } else {
        kind
    };
    writes.push(Write::Set(
        tmux::PANE_SUBAGENTS,
        append_subagent(&state.subagents, &kind, &id),
    ));
}

fn subagent_stop(payload: &Value, state: &PaneState, writes: &mut Vec<Write>) {
    let id = sanitize(json_str(payload, "agent_id"), ID_LIMIT);
    if id.is_empty() {
        return;
    }
    match remove_subagent(&state.subagents, &id) {
        // Unset rather than store an empty string: `present()` and the lifecycle
        // gate both test for emptiness, and an unset option is the cheaper, more
        // obvious spelling of "none".
        Some(remaining) if remaining.is_empty() => writes.push(Write::Unset(tmux::PANE_SUBAGENTS)),
        Some(remaining) => writes.push(Write::Set(tmux::PANE_SUBAGENTS, remaining)),
        // An id we never recorded, e.g. one whose SubagentStart predated the
        // hooks being installed.
        None => {}
    }
}

/// Append `Type:id`. Ids are what make two parallel `Explore` subagents two
/// entries rather than one.
fn append_subagent(current: &str, kind: &str, id: &str) -> String {
    let entry = format!("{kind}:{id}");
    if current.is_empty() {
        entry
    } else {
        format!("{current},{entry}")
    }
}

/// Drop the entry with this id. `None` when the id is not in the list, so the
/// caller can tell "nothing to do" from "the list is now empty".
fn remove_subagent(current: &str, id: &str) -> Option<String> {
    if current.is_empty() || id.is_empty() {
        return None;
    }
    let suffix = format!(":{id}");
    let entries: Vec<&str> = current.split(',').collect();
    let index = entries.iter().position(|entry| entry.ends_with(&suffix))?;
    let remaining: Vec<&str> = entries
        .iter()
        .enumerate()
        .filter(|(position, _)| *position != index)
        .map(|(_, entry)| *entry)
        .collect();
    Some(remaining.join(","))
}

/// Task progress is two counters rather than a list of ids, because the row only
/// ever renders `done/total` and options are a cramped place to keep a set.
fn task_created(state: &PaneState, writes: &mut Vec<Write>) {
    // A list with everything done is a finished list; the next task starts a new
    // one instead of extending it, so `tasks 3/3` becomes `0/1` rather than `3/4`.
    let (done, total) = if state.task_total > 0 && state.task_done >= state.task_total {
        (0, 1)
    } else {
        (state.task_done, state.task_total + 1)
    };
    writes.push(Write::Set(tmux::PANE_TASK_DONE, done.to_string()));
    writes.push(Write::Set(tmux::PANE_TASK_TOTAL, total.to_string()));
}

fn task_completed(state: &PaneState, writes: &mut Vec<Write>) {
    // No total on record means the list was created before the hooks were
    // installed. Counting a lone completion would render `1/1`, claiming a
    // one-task list that finished — a fabricated fact rather than a missing one.
    if state.task_total == 0 {
        return;
    }
    let done = (state.task_done + 1).min(state.task_total);
    writes.push(Write::Set(tmux::PANE_TASK_DONE, done.to_string()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AgentState, parse_hook_state};
    use serde_json::json;

    /// The files that sit on the other side of a boundary the compiler cannot
    /// see: JSON read by Claude Code, and a shell script it executes.
    const SHIPPED_HOOKS: &str = include_str!("../../hooks/hooks.json");
    const SHIPPED_HOOK_SH: &str = include_str!("../../hook.sh");
    const SHIPPED_PLUGIN: &str = include_str!("../../.claude-plugin/plugin.json");
    const SHIPPED_MARKETPLACE: &str = include_str!("../../.claude-plugin/marketplace.json");

    /// The smallest payload each event can arrive with and still be meaningful.
    fn minimal_payload(event: Event) -> Value {
        match event {
            Event::SubagentStart | Event::SubagentStop => {
                json!({"agent_type": "Explore", "agent_id": "sub-1"})
            }
            _ => json!({}),
        }
    }

    fn plan_of(event: Event, payload: Value) -> Vec<Write> {
        plan(event, &payload, &PaneState::default(), 1000)
    }

    fn set_value(writes: &[Write], key: &str) -> Option<String> {
        writes.iter().find_map(|write| match write {
            Write::Set(name, value) if *name == key => Some(value.clone()),
            _ => None,
        })
    }

    fn unsets(writes: &[Write], key: &str) -> bool {
        writes
            .iter()
            .any(|write| matches!(write, Write::Unset(name) if *name == key))
    }

    // ─── drift across the Rust/JSON/shell boundaries ──────────────────

    #[test]
    fn the_table_and_the_enum_cover_each_other_exactly() {
        assert_eq!(
            HOOK_REGISTRATIONS.len(),
            Event::ALL.len(),
            "every event needs exactly one registration"
        );
        for event in Event::ALL {
            assert_eq!(
                HOOK_REGISTRATIONS
                    .iter()
                    .filter(|registration| registration.event == *event)
                    .count(),
                1,
                "{event:?} must appear in HOOK_REGISTRATIONS exactly once"
            );
        }
    }

    #[test]
    fn cli_names_round_trip_and_are_the_kebab_case_of_their_trigger() {
        // The name is what `hooks.json` passes on the command line, so a typo
        // here is a hook that fires and is silently discarded.
        for registration in HOOK_REGISTRATIONS {
            let event = registration.event;
            assert_eq!(Event::from_cli_name(event.cli_name()), Some(event));
            assert_eq!(
                kebab_case(registration.trigger),
                event.cli_name(),
                "{} should map to {}",
                registration.trigger,
                kebab_case(registration.trigger)
            );
        }
        assert_eq!(Event::from_cli_name("post-tool-use"), None);
        assert_eq!(Event::from_cli_name(""), None);
    }

    fn kebab_case(trigger: &str) -> String {
        let mut out = String::new();
        for (index, ch) in trigger.char_indices() {
            if ch.is_uppercase() && index > 0 {
                out.push('-');
            }
            out.extend(ch.to_lowercase());
        }
        out
    }

    #[test]
    fn hooks_json_matches_the_registration_table() {
        // The whole point of the table: `hooks.json` is read by Claude Code, not
        // by us, so a trigger added on one side and not the other simply never
        // fires — with nothing anywhere to complain about it.
        let parsed: Value = serde_json::from_str(SHIPPED_HOOKS).expect("hooks.json is valid JSON");
        let hooks = parsed
            .get("hooks")
            .and_then(Value::as_object)
            .expect("hooks.json has a top-level `hooks` object");

        for registration in HOOK_REGISTRATIONS {
            let entry = hooks
                .get(registration.trigger)
                .unwrap_or_else(|| panic!("hooks.json does not register {}", registration.trigger));
            let commands = entry.to_string();
            assert!(
                commands.contains("hook.sh"),
                "{} must run hook.sh",
                registration.trigger
            );
            assert!(
                commands.contains(&format!("{AGENT} {}", registration.event.cli_name())),
                "{} must pass `{AGENT} {}`, got {commands}",
                registration.trigger,
                registration.event.cli_name()
            );
        }

        for trigger in hooks.keys() {
            assert!(
                HOOK_REGISTRATIONS
                    .iter()
                    .any(|registration| registration.trigger == trigger),
                "hooks.json registers {trigger}, which nothing handles"
            );
        }
    }

    #[test]
    fn every_event_produces_a_plan_so_no_match_arm_is_missing() {
        for event in Event::ALL {
            let writes = plan_of(*event, minimal_payload(*event));
            assert!(
                !writes.is_empty(),
                "{event:?} planned no writes — a match arm is missing or bailed out"
            );
        }
    }

    #[test]
    fn hook_state_labels_are_ones_the_model_parses() {
        // These strings cross a module boundary as data: we write them, the
        // daemon reads them back through the model.
        assert_eq!(parse_hook_state(RUNNING), Some(AgentState::Working));
        assert_eq!(parse_hook_state(WAITING), Some(AgentState::Blocked));
        assert_eq!(parse_hook_state(IDLE), Some(AgentState::Idle));
        assert_eq!(parse_hook_state(ERROR), Some(AgentState::Error));
    }

    #[test]
    fn every_key_we_write_is_hook_owned_so_the_daemon_sweep_can_clear_it() {
        // Writing outside this namespace would either fight the daemon for the
        // resolved status or leave a key nothing ever cleans up.
        for event in Event::ALL {
            let state = PaneState {
                subagents: String::new(),
                task_done: 1,
                task_total: 2,
            };
            for write in plan(*event, &minimal_payload(*event), &state, 10) {
                let key = match write {
                    Write::Set(key, _) | Write::Unset(key) => key,
                    Write::Sweep => continue,
                };
                assert!(
                    tmux::HOOK_OWNED_PANE_OPTIONS.contains(&key),
                    "{event:?} writes {key}, which is not hook-owned"
                );
            }
        }
    }

    #[test]
    fn hook_sh_resolves_the_binary_the_way_the_plugin_publishes_it() {
        assert!(
            SHIPPED_HOOK_SH.contains(tmux::CFG_BIN),
            "hook.sh should ask tmux for {} rather than guessing a path",
            tmux::CFG_BIN
        );
        assert!(
            SHIPPED_HOOK_SH.contains("hook"),
            "hook.sh must invoke the hook subcommand"
        );
    }

    #[test]
    fn the_plugin_manifests_agree_with_the_crate() {
        let plugin: Value = serde_json::from_str(SHIPPED_PLUGIN).expect("plugin.json is valid JSON");
        assert_eq!(
            plugin.get("name").and_then(Value::as_str),
            Some(env!("CARGO_PKG_NAME"))
        );
        assert_eq!(
            plugin.get("version").and_then(Value::as_str),
            Some(env!("CARGO_PKG_VERSION")),
            "bump .claude-plugin/plugin.json alongside Cargo.toml"
        );

        let marketplace: Value =
            serde_json::from_str(SHIPPED_MARKETPLACE).expect("marketplace.json is valid JSON");
        let names: Vec<&str> = marketplace
            .get("plugins")
            .and_then(Value::as_array)
            .expect("marketplace.json lists plugins")
            .iter()
            .filter_map(|plugin| plugin.get("name").and_then(Value::as_str))
            .collect();
        assert!(
            names.contains(&env!("CARGO_PKG_NAME")),
            "marketplace.json must offer {}",
            env!("CARGO_PKG_NAME")
        );
    }

    // ─── the subagent gate ────────────────────────────────────────────

    fn with_children() -> PaneState {
        PaneState {
            subagents: "Explore:sub-1".to_owned(),
            ..PaneState::default()
        }
    }

    #[test]
    fn turn_ending_events_are_dropped_while_a_subagent_could_be_the_sender() {
        // A child's Stop is indistinguishable from its parent's, and acting on it
        // would park a busy pane at idle mid-turn.
        let state = with_children();
        for event in Event::ALL.iter().filter(|event| event.ends_the_turn()) {
            assert!(
                plan(*event, &minimal_payload(*event), &state, 10).is_empty(),
                "{event:?} must not act on a pane with live subagents"
            );
        }
    }

    #[test]
    fn being_waited_on_is_reported_even_when_a_child_could_be_the_sender() {
        // The case that matters most: hook state outranks passive detection, so a
        // dropped permission prompt is not merely missing detail — it leaves the
        // pane asserting "working" while it sits waiting for an answer. And a
        // prompt raised for a subagent still appears in this pane.
        let state = with_children();
        let blocked = plan(
            Event::Notification,
            &json!({"notification_type": "permission_prompt"}),
            &state,
            10,
        );
        assert_eq!(
            set_value(&blocked, tmux::PANE_HOOK_STATE).as_deref(),
            Some(WAITING)
        );
        assert_eq!(
            set_value(&blocked, tmux::PANE_WAIT_REASON).as_deref(),
            Some("permission_prompt")
        );

        // Both of these say "work is happening", which a live child only confirms.
        for event in [Event::UserPromptSubmit, Event::PermissionDenied] {
            let writes = plan(event, &json!({}), &state, 10);
            assert_eq!(
                set_value(&writes, tmux::PANE_HOOK_STATE).as_deref(),
                Some(RUNNING),
                "{event:?} should still report work in progress"
            );
        }
    }

    #[test]
    fn an_idle_prompt_is_gated_like_the_other_turn_ending_claims() {
        // The one half of Notification that asserts quiet rather than attention.
        let writes = plan(
            Event::Notification,
            &json!({"notification_type": IDLE_PROMPT}),
            &with_children(),
            10,
        );
        assert!(set_value(&writes, tmux::PANE_HOOK_STATE).is_none());
        assert!(!unsets(&writes, tmux::PANE_WAIT_REASON));
    }

    #[test]
    fn subagent_and_task_events_still_apply_while_subagents_are_live() {
        // These are keyed by id or are pure counters, so a child firing one is
        // harmless — and SubagentStop in particular *must* get through, or the
        // list could never drain.
        let state = PaneState {
            subagents: "Explore:sub-1".to_owned(),
            task_done: 0,
            task_total: 2,
        };
        let stop = plan(
            Event::SubagentStop,
            &json!({"agent_id": "sub-1"}),
            &state,
            10,
        );
        assert!(unsets(&stop, tmux::PANE_SUBAGENTS));

        let done = plan(Event::TaskCompleted, &json!({}), &state, 10);
        assert_eq!(set_value(&done, tmux::PANE_TASK_DONE).as_deref(), Some("1"));
    }

    // ─── lifecycle ────────────────────────────────────────────────────

    #[test]
    fn session_end_sweeps_and_writes_nothing_else() {
        assert_eq!(plan_of(Event::SessionEnd, json!({})), vec![Write::Sweep]);
    }

    #[test]
    fn session_start_records_the_agent_and_clears_the_previous_runs_detail() {
        let state = PaneState {
            subagents: String::new(),
            task_done: 3,
            task_total: 3,
        };
        let writes = plan(
            Event::SessionStart,
            &json!({"session_id": "s-1", "cwd": "/repo", "source": "startup"}),
            &state,
            4242,
        );
        assert_eq!(
            set_value(&writes, tmux::PANE_HOOK_AGENT).as_deref(),
            Some(AGENT)
        );
        assert_eq!(
            set_value(&writes, tmux::PANE_HOOK_UPDATED).as_deref(),
            Some("4242")
        );
        assert_eq!(
            set_value(&writes, tmux::PANE_HOOK_STATE).as_deref(),
            Some(IDLE)
        );
        assert_eq!(set_value(&writes, tmux::PANE_CWD).as_deref(), Some("/repo"));
        assert_eq!(
            set_value(&writes, tmux::PANE_SESSION_ID).as_deref(),
            Some("s-1")
        );
        assert!(unsets(&writes, tmux::PANE_TASK_TOTAL));
        assert!(unsets(&writes, tmux::PANE_TASK_DONE));
        assert!(unsets(&writes, tmux::PANE_WAIT_REASON));
    }

    #[test]
    fn a_prompt_starts_a_run_and_a_stop_ends_it() {
        let started = plan_of(Event::UserPromptSubmit, json!({"prompt": "hi"}));
        assert_eq!(
            set_value(&started, tmux::PANE_HOOK_STATE).as_deref(),
            Some(RUNNING)
        );
        assert!(unsets(&started, tmux::PANE_WAIT_REASON));

        let stopped = plan_of(Event::Stop, json!({"last_assistant_message": "done"}));
        assert_eq!(
            set_value(&stopped, tmux::PANE_HOOK_STATE).as_deref(),
            Some(IDLE)
        );
    }

    #[test]
    fn permission_mode_is_synced_only_when_the_event_reports_it() {
        // SessionStart carries no permission_mode; clearing the badge there would
        // make it blink off at the start of every session.
        let silent = plan_of(Event::SessionStart, json!({"cwd": "/repo"}));
        assert!(set_value(&silent, tmux::PANE_PERMISSION_MODE).is_none());
        assert!(!unsets(&silent, tmux::PANE_PERMISSION_MODE));

        let planning = plan_of(Event::UserPromptSubmit, json!({"permission_mode": "plan"}));
        assert_eq!(
            set_value(&planning, tmux::PANE_PERMISSION_MODE).as_deref(),
            Some("plan")
        );

        // Back to default is a real transition, and "no badge" is spelled unset.
        let normal = plan_of(Event::Stop, json!({"permission_mode": "default"}));
        assert!(unsets(&normal, tmux::PANE_PERMISSION_MODE));
    }

    #[test]
    fn a_failed_turn_reports_its_error_category() {
        let writes = plan_of(
            Event::StopFailure,
            json!({"error_type": "rate_limit", "error_message": "slow down"}),
        );
        assert_eq!(
            set_value(&writes, tmux::PANE_HOOK_STATE).as_deref(),
            Some(ERROR)
        );
        assert_eq!(
            set_value(&writes, tmux::PANE_WAIT_REASON).as_deref(),
            Some("rate_limit"),
            "the category is shorter and more useful than the prose"
        );

        // Older payloads only carry a message, and a bare failure still says so.
        let legacy = plan_of(Event::StopFailure, json!({"error_message": "overloaded"}));
        assert_eq!(
            set_value(&legacy, tmux::PANE_WAIT_REASON).as_deref(),
            Some("overloaded")
        );
        let bare = plan_of(Event::StopFailure, json!({}));
        assert_eq!(
            set_value(&bare, tmux::PANE_WAIT_REASON).as_deref(),
            Some("failed")
        );
    }

    #[test]
    fn a_denied_tool_call_still_reads_as_working() {
        // The classifier denied it, Claude got a tool result back and carried on.
        let writes = plan_of(
            Event::PermissionDenied,
            json!({"tool_name": "Bash", "denial_reason": "network"}),
        );
        assert_eq!(
            set_value(&writes, tmux::PANE_HOOK_STATE).as_deref(),
            Some(RUNNING)
        );
    }

    // ─── notifications ────────────────────────────────────────────────

    #[test]
    fn a_permission_prompt_blocks_the_pane_and_names_the_reason() {
        let writes = plan_of(
            Event::Notification,
            json!({"notification_type": "permission_prompt", "message": "Allow Bash?"}),
        );
        assert_eq!(
            set_value(&writes, tmux::PANE_HOOK_STATE).as_deref(),
            Some(WAITING)
        );
        assert_eq!(
            set_value(&writes, tmux::PANE_WAIT_REASON).as_deref(),
            Some("permission_prompt")
        );
    }

    #[test]
    fn blocking_notification_types_include_the_ones_that_are_not_permissions() {
        assert!(notification_blocks("permission_prompt"));
        assert!(notification_blocks("elicitation_dialog"));
        assert!(notification_blocks("agent_needs_input"));
        // The permission prompt has been renamed before; match the family.
        assert!(notification_blocks("permission"));
        assert!(notification_blocks("tool_permission_request"));

        assert!(!notification_blocks("auth_success"));
        assert!(!notification_blocks("elicitation_complete"));
        assert!(!notification_blocks("agent_completed"));
        assert!(!notification_blocks(IDLE_PROMPT));
        assert!(!notification_blocks(""));
    }

    #[test]
    fn an_idle_prompt_is_idle_not_blocked() {
        // Claude sitting at its own prompt is not waiting on an answer.
        let writes = plan_of(
            Event::Notification,
            json!({"notification_type": IDLE_PROMPT}),
        );
        assert_eq!(
            set_value(&writes, tmux::PANE_HOOK_STATE).as_deref(),
            Some(IDLE)
        );
        assert!(unsets(&writes, tmux::PANE_WAIT_REASON));
    }

    #[test]
    fn an_informational_notification_only_refreshes_the_heartbeat() {
        // Guessing a state from an unrecognized notification is how a pane ends
        // up claiming it needs you when it doesn't.
        for kind in ["auth_success", "elicitation_complete", "some_new_thing"] {
            let writes = plan_of(Event::Notification, json!({"notification_type": kind}));
            assert!(
                set_value(&writes, tmux::PANE_HOOK_STATE).is_none(),
                "{kind} must not assert a state"
            );
            assert!(!unsets(&writes, tmux::PANE_WAIT_REASON));
            assert!(set_value(&writes, tmux::PANE_HOOK_UPDATED).is_some());
        }
    }

    // ─── subagents ────────────────────────────────────────────────────

    #[test]
    fn subagents_are_tracked_by_id_so_parallel_explores_stay_distinct() {
        assert_eq!(append_subagent("", "Explore", "a1"), "Explore:a1");
        assert_eq!(
            append_subagent("Explore:a1", "Explore", "a2"),
            "Explore:a1,Explore:a2"
        );

        let list = "Explore:a1,Plan:b2";
        assert_eq!(remove_subagent(list, "a1").as_deref(), Some("Plan:b2"));
        assert_eq!(remove_subagent(list, "b2").as_deref(), Some("Explore:a1"));
        assert_eq!(remove_subagent("Explore:a1", "a1").as_deref(), Some(""));
        assert_eq!(remove_subagent(list, "nope"), None);
        assert_eq!(remove_subagent("", "a1"), None);
    }

    #[test]
    fn a_subagent_without_an_id_is_not_recorded() {
        // An unremovable entry would hold the lifecycle gate shut for good.
        let writes = plan_of(Event::SubagentStart, json!({"agent_type": "Explore"}));
        assert!(set_value(&writes, tmux::PANE_SUBAGENTS).is_none());
        assert!(
            set_value(&writes, tmux::PANE_HOOK_UPDATED).is_some(),
            "the fire itself still proves hooks are wired up"
        );
    }

    #[test]
    fn a_subagent_with_no_type_still_gets_an_entry() {
        let writes = plan_of(Event::SubagentStart, json!({"agent_id": "a9"}));
        assert_eq!(
            set_value(&writes, tmux::PANE_SUBAGENTS).as_deref(),
            Some("agent:a9")
        );
    }

    #[test]
    fn stopping_an_unknown_subagent_leaves_the_list_alone() {
        let state = PaneState {
            subagents: "Explore:a1".to_owned(),
            ..PaneState::default()
        };
        let writes = plan(Event::SubagentStop, &json!({"agent_id": "b2"}), &state, 10);
        assert!(set_value(&writes, tmux::PANE_SUBAGENTS).is_none());
        assert!(!unsets(&writes, tmux::PANE_SUBAGENTS));
    }

    // ─── task progress ────────────────────────────────────────────────

    #[test]
    fn task_counters_accumulate_across_a_list() {
        let mut state = PaneState::default();
        for expected_total in 1..=3 {
            let writes = plan(Event::TaskCreated, &json!({}), &state, 10);
            assert_eq!(
                set_value(&writes, tmux::PANE_TASK_TOTAL).as_deref(),
                Some(expected_total.to_string().as_str())
            );
            state.task_total = expected_total;
        }

        let writes = plan(Event::TaskCompleted, &json!({}), &state, 10);
        assert_eq!(set_value(&writes, tmux::PANE_TASK_DONE).as_deref(), Some("1"));
    }

    #[test]
    fn a_finished_list_is_replaced_rather_than_extended() {
        let state = PaneState {
            subagents: String::new(),
            task_done: 3,
            task_total: 3,
        };
        let writes = plan(Event::TaskCreated, &json!({}), &state, 10);
        assert_eq!(set_value(&writes, tmux::PANE_TASK_DONE).as_deref(), Some("0"));
        assert_eq!(
            set_value(&writes, tmux::PANE_TASK_TOTAL).as_deref(),
            Some("1"),
            "a new list, not 3/4"
        );
    }

    #[test]
    fn completions_never_exceed_the_total_and_need_a_list_to_count_against() {
        let full = PaneState {
            subagents: String::new(),
            task_done: 2,
            task_total: 2,
        };
        let writes = plan(Event::TaskCompleted, &json!({}), &full, 10);
        assert_eq!(set_value(&writes, tmux::PANE_TASK_DONE).as_deref(), Some("2"));

        // Tasks created before the hooks were installed: we have no total, and
        // inventing 1/1 would report a list that never existed.
        let unknown = PaneState::default();
        let writes = plan(Event::TaskCompleted, &json!({}), &unknown, 10);
        assert!(set_value(&writes, tmux::PANE_TASK_DONE).is_none());
    }

    // ─── payload hygiene ──────────────────────────────────────────────

    #[test]
    fn a_multiline_error_cannot_break_the_row_encoding() {
        // Every option we write comes back as one field of one line of
        // `list-panes` output.
        let writes = plan_of(
            Event::StopFailure,
            json!({"error_type": "server\nerror\twith breaks"}),
        );
        let reason = set_value(&writes, tmux::PANE_WAIT_REASON).unwrap();
        assert_eq!(reason, "server error with breaks");
    }

    #[test]
    fn an_empty_payload_still_records_that_hooks_are_live() {
        // A hook can fire with a body we cannot parse; the fire itself is the
        // fact that matters, since it is what flips the pane to source=hook.
        let writes = plan_of(Event::Stop, Value::Null);
        assert_eq!(
            set_value(&writes, tmux::PANE_HOOK_AGENT).as_deref(),
            Some(AGENT)
        );
        assert!(set_value(&writes, tmux::PANE_CWD).is_none());
        assert!(set_value(&writes, tmux::PANE_SESSION_ID).is_none());
    }
}
