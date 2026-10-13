//! Colors and status glyphs.
//!
//! Defaults are indexed ANSI colors rather than RGB so they inherit the user's
//! terminal palette and stay legible on both light and dark backgrounds.
//! `@agent_mgr_theme mocha` swaps in Catppuccin Mocha instead — truecolor, and
//! dark-only, which is why it is opt-in. Every entry is then still overridable
//! through an `@agent_mgr_color_*` tmux option.

use ratatui::style::Color;

use crate::model::{AgentKind, AgentState};
use crate::tmux;

/// Spinner frames for a Working pane. Braille, matching what both agents show in
/// their own titles, so the sidebar reads as the same activity rather than a
/// second competing animation.
pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// tmux's own accent color, used for the active-pane marker so the sidebar
/// agrees with the rest of the UI about what "current" looks like.
const TMUX_ORANGE: Color = Color::Indexed(202);

pub struct Theme {
    pub accent: Color,
    pub session: Color,
    pub window: Color,
    pub selection_bg: Color,
    pub text: Color,
    pub muted: Color,
    pub working: Color,
    pub blocked: Color,
    pub idle: Color,
    pub done: Color,
    pub error: Color,
    pub unknown: Color,
    pub agent_claude: Color,
    pub agent_codex: Color,
    pub branch: Color,
    pub badge_plan: Color,
    pub badge_auto: Color,
    pub badge_danger: Color,
    pub wait_reason: Color,
    pub subagent: Color,
    pub task_progress: Color,
    /// The context card's working directory.
    pub folder: Color,
    /// Dividers: the rule above the context card.
    pub rule: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            accent: TMUX_ORANGE,
            session: Color::Indexed(75),
            window: Color::Indexed(245),
            selection_bg: Color::Indexed(237),
            text: Color::Indexed(252),
            muted: Color::Indexed(244),
            working: Color::Indexed(220),
            blocked: Color::Indexed(203),
            idle: Color::Indexed(108),
            done: Color::Indexed(80),
            error: Color::Indexed(196),
            unknown: Color::Indexed(240),
            agent_claude: Color::Indexed(215),
            agent_codex: Color::Indexed(114),
            branch: Color::Indexed(139),
            badge_plan: Color::Indexed(111),
            badge_auto: Color::Indexed(150),
            badge_danger: Color::Indexed(203),
            wait_reason: Color::Indexed(209),
            subagent: Color::Indexed(146),
            task_progress: Color::Indexed(115),
            folder: Color::Indexed(117),
            rule: Color::Indexed(238),
        }
    }
}

/// Catppuccin Mocha, by role. Named so the mapping below reads as a palette
/// choice rather than a column of hex.
mod mocha {
    use ratatui::style::Color;
    pub const PINK: Color = Color::Rgb(0xf5, 0xc2, 0xe7);
    pub const MAUVE: Color = Color::Rgb(0xcb, 0xa6, 0xf7);
    pub const RED: Color = Color::Rgb(0xf3, 0x8b, 0xa8);
    pub const MAROON: Color = Color::Rgb(0xeb, 0xa0, 0xac);
    pub const PEACH: Color = Color::Rgb(0xfa, 0xb3, 0x87);
    pub const YELLOW: Color = Color::Rgb(0xf9, 0xe2, 0xaf);
    pub const GREEN: Color = Color::Rgb(0xa6, 0xe3, 0xa1);
    pub const TEAL: Color = Color::Rgb(0x94, 0xe2, 0xd5);
    pub const SKY: Color = Color::Rgb(0x89, 0xdc, 0xeb);
    pub const SAPPHIRE: Color = Color::Rgb(0x74, 0xc7, 0xec);
    pub const BLUE: Color = Color::Rgb(0x89, 0xb4, 0xfa);
    pub const LAVENDER: Color = Color::Rgb(0xb4, 0xbe, 0xfe);
    pub const TEXT: Color = Color::Rgb(0xcd, 0xd6, 0xf4);
    pub const SUBTEXT0: Color = Color::Rgb(0xa6, 0xad, 0xc8);
    pub const OVERLAY1: Color = Color::Rgb(0x7f, 0x84, 0x9c);
    pub const OVERLAY0: Color = Color::Rgb(0x6c, 0x70, 0x86);
    pub const SURFACE1: Color = Color::Rgb(0x45, 0x47, 0x5a);
    pub const SURFACE0: Color = Color::Rgb(0x31, 0x32, 0x44);
}

impl Theme {
    /// Catppuccin Mocha. Mauve leads as the accent, each agent state gets its own
    /// hue, and the context card's pieces — folder, branch, PR, ticket — are four
    /// distinct colours so the row reads at a glance.
    pub fn mocha() -> Self {
        use mocha::*;
        Self {
            accent: MAUVE,
            session: BLUE,
            window: SUBTEXT0,
            selection_bg: SURFACE0,
            text: TEXT,
            muted: OVERLAY1,
            working: YELLOW,
            blocked: RED,
            idle: GREEN,
            done: TEAL,
            error: MAROON,
            unknown: OVERLAY0,
            agent_claude: PEACH,
            agent_codex: SKY,
            branch: PINK,
            badge_plan: SAPPHIRE,
            badge_auto: GREEN,
            badge_danger: RED,
            wait_reason: PEACH,
            subagent: LAVENDER,
            task_progress: TEAL,
            folder: SKY,
            rule: SURFACE1,
        }
    }

    /// Read overrides from `@agent_mgr_color_*`. One `show -g` would be cheaper
    /// than one call per color, but this runs once at startup, so clarity wins.
    pub fn from_tmux() -> Self {
        let mut theme = match tmux::global(tmux::CFG_THEME).as_deref() {
            Some("mocha") => Self::mocha(),
            _ => Self::default(),
        };
        let overrides: [(&str, &mut Color); 8] = [
            ("accent", &mut theme.accent),
            ("session", &mut theme.session),
            ("working", &mut theme.working),
            ("blocked", &mut theme.blocked),
            ("idle", &mut theme.idle),
            ("done", &mut theme.done),
            ("error", &mut theme.error),
            ("branch", &mut theme.branch),
        ];
        for (name, slot) in overrides {
            if let Some(color) = tmux::global(&format!("@agent_mgr_color_{name}"))
                .as_deref()
                .and_then(parse_color)
            {
                *slot = color;
            }
        }
        theme
    }

    pub fn state_color(&self, state: AgentState, seen: bool) -> Color {
        match state {
            AgentState::Working => self.working,
            AgentState::Blocked => self.blocked,
            AgentState::Error => self.error,
            AgentState::Idle if !seen => self.done,
            AgentState::Idle => self.idle,
            AgentState::Unknown => self.unknown,
        }
    }

    pub fn agent_color(&self, agent: Option<AgentKind>) -> Color {
        match agent {
            Some(AgentKind::Claude) => self.agent_claude,
            Some(AgentKind::Codex) => self.agent_codex,
            None => self.muted,
        }
    }
}

/// Parse a color option: `#RRGGBB`, a 0–255 palette index, or a tmux-style
/// `colour123`. Anything else is rejected so a typo leaves the default in place
/// instead of rendering black-on-black.
fn parse_color(value: &str) -> Option<Color> {
    let value = value.trim();
    if let Some(hex) = value.strip_prefix('#') {
        if hex.len() != 6 {
            return None;
        }
        let red = u8::from_str_radix(&hex[0..2], 16).ok()?;
        let green = u8::from_str_radix(&hex[2..4], 16).ok()?;
        let blue = u8::from_str_radix(&hex[4..6], 16).ok()?;
        return Some(Color::Rgb(red, green, blue));
    }
    let digits = value
        .strip_prefix("colour")
        .or_else(|| value.strip_prefix("color"))
        .unwrap_or(value);
    digits.parse::<u8>().ok().map(Color::Indexed)
}

/// Glyph for a pane's state. Deliberately the same vocabulary the window-tab
/// markers use, so a `●` means one thing everywhere.
pub fn state_icon(state: AgentState, seen: bool) -> &'static str {
    match state {
        AgentState::Working => "●",
        AgentState::Blocked => "◉",
        AgentState::Error => "✕",
        AgentState::Idle if !seen => "●",
        AgentState::Idle => "○",
        AgentState::Unknown => "·",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex_palette_index_and_tmux_colour_names() {
        assert_eq!(parse_color("#ff8800"), Some(Color::Rgb(0xff, 0x88, 0x00)));
        assert_eq!(parse_color("202"), Some(Color::Indexed(202)));
        assert_eq!(parse_color("colour202"), Some(Color::Indexed(202)));
        assert_eq!(parse_color("color202"), Some(Color::Indexed(202)));
        assert_eq!(parse_color("  75 "), Some(Color::Indexed(75)));
    }

    #[test]
    fn rejects_junk_so_a_typo_keeps_the_default() {
        assert_eq!(parse_color(""), None);
        assert_eq!(parse_color("#fff"), None);
        assert_eq!(parse_color("#gggggg"), None);
        assert_eq!(parse_color("299"), None);
        assert_eq!(parse_color("blue-ish"), None);
    }

    #[test]
    fn a_finished_run_is_coloured_apart_from_a_quiet_idle_one() {
        let theme = Theme::default();
        assert_ne!(
            theme.state_color(AgentState::Idle, false),
            theme.state_color(AgentState::Idle, true),
            "an unread finished run must stand out from a settled idle pane"
        );
    }

    #[test]
    fn every_state_has_a_single_cell_icon() {
        use crate::ui::text::width;
        for (state, seen) in [
            (AgentState::Working, true),
            (AgentState::Blocked, true),
            (AgentState::Error, true),
            (AgentState::Idle, false),
            (AgentState::Idle, true),
            (AgentState::Unknown, true),
        ] {
            assert_eq!(width(state_icon(state, seen)), 1, "{state:?} icon is not 1 cell");
        }
        for frame in SPINNER {
            assert_eq!(width(frame), 1, "spinner frame {frame} is not 1 cell");
        }
    }
}
