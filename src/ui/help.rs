//! The keymap, written down once.
//!
//! This table is the help page *and* the drift guard. Every entry names a token
//! that must appear in `app/input.rs`, and a test asserts it does — so deleting or
//! renaming a binding without updating its documentation fails the build rather
//! than shipping a help page that lies. Documentation that can drift silently is
//! worse than none, because it is trusted.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::ui::text::{pad_to, truncate, width};
use crate::ui::theme::Theme;

/// One documented binding.
pub struct Binding {
    /// How the keys are shown to the user.
    pub keys: &'static str,
    pub description: &'static str,
    /// A literal that must occur in `app/input.rs` for this binding to be real.
    /// Never read at runtime — it exists so the test below can prove the help page
    /// still describes code that is there.
    #[allow(dead_code, reason = "drift anchor, read only by the test below")]
    pub token: &'static str,
}

/// Ordered roughly by how often you reach for them.
pub const KEYMAP: &[Binding] = &[
    Binding {
        keys: "j k ↓ ↑",
        description: "next / previous pane",
        token: "KeyCode::Char('j')",
    },
    Binding {
        keys: "N j/k",
        description: "move N panes",
        token: "push_count",
    },
    Binding {
        keys: "H L",
        description: "previous / next session",
        token: "jump_session",
    },
    Binding {
        keys: "J K",
        description: "move session down / up",
        token: "move_session",
    },
    Binding {
        keys: "g G",
        description: "first / last pane",
        token: "KeyCode::Char('g')",
    },
    Binding {
        keys: "N G",
        description: "go to pane N",
        token: "pending_count.take()",
    },
    Binding {
        keys: "C-d C-u",
        description: "page down / up",
        token: "KeyCode::Char('d') if ctrl",
    },
    Binding {
        keys: "Enter",
        description: "jump to pane",
        token: "activate_selection",
    },
    Binding {
        keys: "click",
        description: "jump to that pane",
        token: "MouseButton::Left",
    },
    Binding {
        keys: "Tab",
        description: "cycle status filter",
        token: "KeyCode::Tab",
    },
    Binding {
        keys: "/",
        description: "search panes",
        token: "KeyCode::Char('/')",
    },
    Binding {
        keys: "R",
        description: "rename window",
        token: "KeyCode::Char('R')",
    },
    Binding {
        keys: "r",
        description: "refresh now",
        token: "KeyCode::Char('r')",
    },
    Binding {
        keys: "a",
        description: "jot down a note",
        token: "open_note_entry",
    },
    // One entry rather than four: the panel's own keys are modal, and a flat page
    // that listed them beside the list's would imply they work in both places.
    Binding {
        keys: "n",
        description: "notes panel — j/k Space Enter",
        token: "open_notes",
    },
    Binding {
        keys: "?",
        description: "this help",
        token: "KeyCode::Char('?')",
    },
    Binding {
        keys: "q Esc",
        description: "close",
        token: "KeyCode::Char('q')",
    },
];

/// Render the keymap into at most `height` lines of `total_width` columns.
///
/// Degrades by dropping rows off the end rather than wrapping: a wrapped keymap in
/// a 24-column sidebar is unreadable, and the bindings are ordered by how often
/// they are wanted, so the ones that survive are the ones worth showing.
pub fn lines(total_width: usize, height: usize, theme: &Theme) -> Vec<Line<'static>> {
    // One key column wide enough for every entry keeps the descriptions aligned,
    // which is most of what makes a table like this scannable.
    let key_width = KEYMAP
        .iter()
        .map(|binding| width(binding.keys))
        .max()
        .unwrap_or(0)
        .min(total_width.saturating_sub(2));

    KEYMAP
        .iter()
        .take(height)
        .map(|binding| {
            let keys = truncate(binding.keys, key_width);
            let pad = pad_to(width(&keys), key_width);
            let room = total_width.saturating_sub(key_width + 2);
            let description = truncate(binding.description, room);
            let used = key_width + 2 + width(&description);
            Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    keys,
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(pad),
                Span::raw(" "),
                Span::styled(description, Style::default().fg(theme.text)),
                Span::raw(pad_to(used, total_width)),
            ])
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The keymap implementation, read at compile time so a binding cannot be
    /// removed without this module noticing.
    const INPUT_SOURCE: &str = include_str!("../app/input.rs");

    #[test]
    fn every_documented_binding_exists_in_the_keymap() {
        for binding in KEYMAP {
            assert!(
                INPUT_SOURCE.contains(binding.token),
                "help documents {:?} ({}), but {:?} is not in app/input.rs — \
                 the binding was renamed or removed and the help page now lies",
                binding.keys,
                binding.description,
                binding.token,
            );
        }
    }

    #[test]
    fn no_two_entries_document_the_same_key() {
        for (index, binding) in KEYMAP.iter().enumerate() {
            for other in &KEYMAP[index + 1..] {
                assert_ne!(
                    binding.keys, other.keys,
                    "duplicate help entry for {:?}",
                    binding.keys
                );
            }
        }
    }

    #[test]
    fn every_line_is_exactly_the_requested_width() {
        // Same contract as the pane rows: one cell too wide wraps and desyncs
        // everything below it.
        for total_width in [12, 24, 40, 80] {
            for line in lines(total_width, 99, &Theme::default()) {
                let text: String = line.spans.iter().map(|span| span.content.as_ref()).collect();
                assert_eq!(width(&text), total_width, "at width {total_width}: {text:?}");
            }
        }
    }

    #[test]
    fn a_short_page_keeps_the_most_wanted_bindings() {
        let short = lines(40, 3, &Theme::default());
        assert_eq!(short.len(), 3);
        let first: String = short[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(first.contains('j'), "motion should survive truncation: {first:?}");
    }

    #[test]
    fn a_zero_height_page_renders_nothing_rather_than_panicking() {
        assert!(lines(40, 0, &Theme::default()).is_empty());
    }
}
