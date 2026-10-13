// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::game::{Direction, GameCommand};
use crate::internal_message::InternalMessage;
use fidl_fuchsia_input::Key;
use fidl_fuchsia_ui_input3::{
    KeyEvent, KeyEventStatus, KeyEventType, KeyboardListenerRequest, KeyboardListenerRequestStream,
};
use fuchsia_async as fasync;
use futures::StreamExt;
use futures::channel::mpsc::UnboundedSender;
use log::warn;

fn map_key_to_command(key: Key) -> Option<GameCommand> {
    match key {
        Key::Up | Key::W => Some(GameCommand::Move(Direction::Up)),
        Key::Down | Key::S => Some(GameCommand::Move(Direction::Down)),
        Key::Left | Key::A => Some(GameCommand::Move(Direction::Left)),
        Key::Right | Key::D => Some(GameCommand::Move(Direction::Right)),
        Key::Space | Key::P => Some(GameCommand::TogglePause),
        Key::Enter | Key::R => Some(GameCommand::Restart),
        _ => None,
    }
}

fn handle_key_event(event: &KeyEvent) -> (KeyEventStatus, Option<GameCommand>) {
    if event.type_ == Some(KeyEventType::Pressed) {
        if let Some(key) = event.key {
            if let Some(cmd) = map_key_to_command(key) {
                return (KeyEventStatus::Handled, Some(cmd));
            }
        }
    }
    (KeyEventStatus::NotHandled, None)
}

pub(crate) fn spawn_keyboard_listener(
    scope: &fasync::ScopeHandle,
    mut stream: KeyboardListenerRequestStream,
    sender: UnboundedSender<InternalMessage>,
) {
    scope.spawn(async move {
        while let Some(request) = stream.next().await {
            match request {
                Ok(KeyboardListenerRequest::OnKeyEvent { event, responder, .. }) => {
                    let (status, cmd) = handle_key_event(&event);
                    if let Some(cmd) = cmd {
                        if sender.unbounded_send(InternalMessage::Command(cmd)).is_err() {
                            warn!("Failed to send GameCommand to main loop");
                        }
                    }
                    if let Err(e) = responder.send(status) {
                        warn!("Failed to respond to OnKeyEvent: {:?}", e);
                    }
                }
                Err(e) => {
                    warn!("Keyboard listener error: {:?}", e);
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    fn test_map_key_to_command() {
        assert_eq!(map_key_to_command(Key::Up), Some(GameCommand::Move(Direction::Up)));
        assert_eq!(map_key_to_command(Key::W), Some(GameCommand::Move(Direction::Up)));
        assert_eq!(map_key_to_command(Key::Down), Some(GameCommand::Move(Direction::Down)));
        assert_eq!(map_key_to_command(Key::S), Some(GameCommand::Move(Direction::Down)));
        assert_eq!(map_key_to_command(Key::Left), Some(GameCommand::Move(Direction::Left)));
        assert_eq!(map_key_to_command(Key::A), Some(GameCommand::Move(Direction::Left)));
        assert_eq!(map_key_to_command(Key::Right), Some(GameCommand::Move(Direction::Right)));
        assert_eq!(map_key_to_command(Key::D), Some(GameCommand::Move(Direction::Right)));
        assert_eq!(map_key_to_command(Key::Space), Some(GameCommand::TogglePause));
        assert_eq!(map_key_to_command(Key::P), Some(GameCommand::TogglePause));
        assert_eq!(map_key_to_command(Key::Enter), Some(GameCommand::Restart));
        assert_eq!(map_key_to_command(Key::R), Some(GameCommand::Restart));
        assert_eq!(map_key_to_command(Key::Escape), None);
    }

    #[fuchsia::test]
    fn test_handle_key_event_filters_event_type_and_mapping() {
        let pressed_mapped = KeyEvent {
            type_: Some(KeyEventType::Pressed),
            key: Some(Key::Space),
            ..Default::default()
        };
        assert_eq!(
            handle_key_event(&pressed_mapped),
            (KeyEventStatus::Handled, Some(GameCommand::TogglePause))
        );

        let released_mapped = KeyEvent {
            type_: Some(KeyEventType::Released),
            key: Some(Key::Space),
            ..Default::default()
        };
        assert_eq!(handle_key_event(&released_mapped), (KeyEventStatus::NotHandled, None));

        let pressed_unmapped = KeyEvent {
            type_: Some(KeyEventType::Pressed),
            key: Some(Key::Escape),
            ..Default::default()
        };
        assert_eq!(handle_key_event(&pressed_unmapped), (KeyEventStatus::NotHandled, None));
    }
}
