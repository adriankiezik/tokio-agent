use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub(crate) fn is_cancel_key(key: &KeyEvent) -> bool {
    key.code == KeyCode::Esc
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
}
