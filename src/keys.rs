//! Which key does what. The options menu reuses the movement keys and gives
//! the left and right keys their btop meaning: change the selected value.

use crate::app::Action;
use crate::config::Panel;
use ratatui::crossterm::event::{KeyCode, KeyModifiers};

/// The action for a key press, on the dashboard or, with `menu`, in the
/// options menu.
pub fn action(code: KeyCode, mods: KeyModifiers, menu: bool) -> Option<Action> {
    if let KeyCode::Char(c) = code {
        if c == 'c' && mods.contains(KeyModifiers::CONTROL) {
            return Some(Action::Quit);
        }
        if let Some(p) = Panel::from_key(c) {
            return Some(Action::Panel(p));
        }
    }
    Some(match code {
        KeyCode::Char('o') | KeyCode::F(2) => Action::Options,
        KeyCode::Up | KeyCode::Char('k') => Action::Up,
        KeyCode::Down | KeyCode::Char('j') => Action::Down,
        KeyCode::Home | KeyCode::Char('g') => Action::Top,
        KeyCode::End | KeyCode::Char('G') => Action::Bottom,
        _ if menu => return menu_action(code),
        KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => Action::Toggle,
        KeyCode::Esc | KeyCode::Char('h') | KeyCode::Left => Action::Collapse,
        KeyCode::Char('f') => Action::Filter,
        KeyCode::Char('r') => Action::Refresh,
        KeyCode::Char('p') => Action::Pause,
        KeyCode::Char('?') => Action::Help,
        KeyCode::Tab | KeyCode::BackTab => Action::Focus,
        KeyCode::Char('q') => Action::Quit,
        _ => return None,
    })
}

fn menu_action(code: KeyCode) -> Option<Action> {
    Some(match code {
        KeyCode::Left | KeyCode::Char('h') => Action::Change { up: false },
        KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter | KeyCode::Char(' ') => {
            Action::Change { up: true }
        }
        KeyCode::Tab => Action::Category { next: true },
        KeyCode::BackTab => Action::Category { next: false },
        KeyCode::Esc | KeyCode::Char('q') => Action::Options,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, menu: bool) -> Option<Action> {
        action(code, KeyModifiers::NONE, menu)
    }

    #[test]
    fn dashboard_keys() {
        assert_eq!(
            key(KeyCode::Char('2'), false),
            Some(Action::Panel(Panel::Runners))
        );
        assert_eq!(
            key(KeyCode::Char('5'), false),
            Some(Action::Panel(Panel::Held))
        );
        assert_eq!(key(KeyCode::Char('6'), false), None);
        assert_eq!(key(KeyCode::Char('o'), false), Some(Action::Options));
        assert_eq!(key(KeyCode::F(2), false), Some(Action::Options));
        assert_eq!(key(KeyCode::Left, false), Some(Action::Collapse));
        assert_eq!(key(KeyCode::Char('q'), false), Some(Action::Quit));
        assert_eq!(key(KeyCode::Tab, false), Some(Action::Focus));
    }

    #[test]
    fn menu_keys() {
        assert_eq!(
            key(KeyCode::Char('h'), true),
            Some(Action::Change { up: false })
        );
        assert_eq!(key(KeyCode::Enter, true), Some(Action::Change { up: true }));
        assert_eq!(
            key(KeyCode::Char(' '), true),
            Some(Action::Change { up: true })
        );
        assert_eq!(
            key(KeyCode::BackTab, true),
            Some(Action::Category { next: false })
        );
        assert_eq!(key(KeyCode::Char('j'), true), Some(Action::Down));
        assert_eq!(
            key(KeyCode::Char('3'), true),
            Some(Action::Panel(Panel::Host))
        );
        for close in [
            KeyCode::Esc,
            KeyCode::Char('q'),
            KeyCode::Char('o'),
            KeyCode::F(2),
        ] {
            assert_eq!(key(close, true), Some(Action::Options), "{close:?} closes");
        }
        assert_eq!(key(KeyCode::Char('f'), true), None, "no roster keys");
        assert_eq!(
            action(KeyCode::Char('c'), KeyModifiers::CONTROL, true),
            Some(Action::Quit)
        );
    }
}
