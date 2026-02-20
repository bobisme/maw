use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyEvent, MouseEvent};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppEvent {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Resize { width: u16, height: u16 },
    Paste(String),
    Tick,
}

fn normalize_event(event: Event) -> AppEvent {
    match event {
        Event::Key(key) => AppEvent::Key(key),
        Event::Mouse(mouse) => AppEvent::Mouse(mouse),
        Event::Resize(width, height) => AppEvent::Resize { width, height },
        Event::Paste(text) => AppEvent::Paste(text),
        _ => AppEvent::Tick,
    }
}

pub fn next_event(timeout: Duration) -> Result<AppEvent> {
    if !event::poll(timeout)? {
        return Ok(AppEvent::Tick);
    }

    Ok(normalize_event(event::read()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEventKind, KeyEventState, KeyModifiers};

    #[test]
    fn normalize_key_event() {
        let key = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        assert_eq!(normalize_event(Event::Key(key)), AppEvent::Key(key));
    }

    #[test]
    fn normalize_resize_event() {
        assert_eq!(
            normalize_event(Event::Resize(120, 40)),
            AppEvent::Resize {
                width: 120,
                height: 40
            }
        );
    }

    #[test]
    fn normalize_focus_event_to_tick() {
        assert_eq!(normalize_event(Event::FocusGained), AppEvent::Tick);
    }

    #[test]
    fn normalize_paste_event() {
        assert_eq!(
            normalize_event(Event::Paste("hello".to_owned())),
            AppEvent::Paste("hello".to_owned())
        );
    }

    #[test]
    fn key_event_roundtrip_includes_metadata() {
        let key = KeyEvent {
            code: KeyCode::Enter,
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        assert_eq!(normalize_event(Event::Key(key)), AppEvent::Key(key));
    }
}
