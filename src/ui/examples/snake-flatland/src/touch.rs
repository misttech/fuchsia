// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::game::{Direction, GameCommand};
use crate::internal_message::InternalMessage;
use fidl_fuchsia_ui_pointer::{
    self as fptr, EventPhase, TouchEvent, TouchInteractionId, TouchInteractionStatus, TouchResponse,
};
use fuchsia_async as fasync;
use futures::channel::mpsc::UnboundedSender;
use log::error;
use std::collections::HashMap;

const SWIPE_THRESHOLD_PX: f32 = 40.0;

/// Applies `ViewParameters.viewport_to_view_transform` (a 3x3 column-major homogeneous matrix)
/// to convert viewport coordinates `(x, y)` into local view coordinates.
pub(crate) fn viewport_to_view_coordinates(
    x: f32,
    y: f32,
    view_parameters: Option<&fptr::ViewParameters>,
) -> (f32, f32) {
    let Some(params) = view_parameters else {
        return (x, y);
    };
    let m = &params.viewport_to_view_transform;
    let xp = m[0] * x + m[3] * y + m[6];
    let yp = m[1] * x + m[4] * y + m[7];
    let wp = m[2] * x + m[5] * y + m[8];
    if wp != 0.0 { (xp / wp, yp / wp) } else { (xp, yp) }
}

#[derive(Default)]
struct Interaction {
    buffered_messages: Vec<InternalMessage>,
    status: Option<TouchInteractionStatus>,
    start_pos: Option<(f32, f32)>,
    anchor: Option<(f32, f32)>,
    swiped: bool,
    phase_ended: bool,
}

impl Interaction {
    fn dispatch_or_buffer(
        &mut self,
        sender: &UnboundedSender<InternalMessage>,
        msg: InternalMessage,
    ) {
        match self.status {
            None => self.buffered_messages.push(msg),
            Some(TouchInteractionStatus::Granted) => {
                let _ = sender.unbounded_send(msg);
            }
            Some(TouchInteractionStatus::Denied) => {}
        }
    }

    fn handle_sample(
        &mut self,
        phase: EventPhase,
        x: f32,
        y: f32,
        view_parameters: Option<&fptr::ViewParameters>,
        sender: &UnboundedSender<InternalMessage>,
    ) {
        match phase {
            EventPhase::Add => {
                self.start_pos = Some((x, y));
                self.anchor = Some((x, y));
                self.swiped = false;
                self.phase_ended = false;
            }
            EventPhase::Change => {
                if let Some((ax, ay)) = self.anchor {
                    let dx = x - ax;
                    let dy = y - ay;
                    if dx.abs() >= SWIPE_THRESHOLD_PX || dy.abs() >= SWIPE_THRESHOLD_PX {
                        let dir = if dx.abs() >= dy.abs() {
                            if dx > 0.0 { Direction::Right } else { Direction::Left }
                        } else if dy > 0.0 {
                            Direction::Down
                        } else {
                            Direction::Up
                        };
                        self.anchor = Some((x, y));
                        self.swiped = true;
                        self.dispatch_or_buffer(
                            sender,
                            InternalMessage::Command(GameCommand::Move(dir)),
                        );
                    }
                }
            }
            EventPhase::Remove => {
                if !self.swiped {
                    let (raw_x, raw_y) = self.start_pos.unwrap_or((x, y));
                    let (tap_x, tap_y) =
                        viewport_to_view_coordinates(raw_x, raw_y, view_parameters);
                    self.dispatch_or_buffer(
                        sender,
                        InternalMessage::PointerTap { x: tap_x, y: tap_y },
                    );
                }
                self.phase_ended = true;
            }
            EventPhase::Cancel => {
                self.buffered_messages.clear();
                self.phase_ended = true;
            }
        }
    }

    fn handle_result(
        &mut self,
        status: TouchInteractionStatus,
        sender: &UnboundedSender<InternalMessage>,
    ) {
        self.status = Some(status);
        let buffered = std::mem::take(&mut self.buffered_messages);
        if status == TouchInteractionStatus::Granted {
            for msg in buffered {
                let _ = sender.unbounded_send(msg);
            }
        }
    }

    /// Returns true once the interaction is denied, or its pointer phase has ended and any
    /// buffered messages have been resolved by a grant decision.
    fn is_complete(&self) -> bool {
        self.status == Some(TouchInteractionStatus::Denied)
            || (self.phase_ended && (self.status.is_some() || self.buffered_messages.is_empty()))
    }
}

fn fill_touch_event_responses(events: &[TouchEvent], responses: &mut Vec<TouchResponse>) {
    responses.clear();
    responses.extend(events.iter().map(|evt| {
        let mut response = TouchResponse::default();
        if evt.pointer_sample.is_some() {
            response.response_type = Some(fptr::TouchResponseType::Yes);
        }
        response
    }));
}

fn process_touch_events(
    events: &[TouchEvent],
    interactions: &mut HashMap<TouchInteractionId, Interaction>,
    view_parameters: &mut Option<fptr::ViewParameters>,
    sender: &UnboundedSender<InternalMessage>,
) {
    for e in events {
        if let Some(params) = &e.view_parameters {
            *view_parameters = Some(*params);
        }

        if let Some(fptr::TouchPointerSample {
            interaction: Some(id),
            phase: Some(phase),
            position_in_viewport: Some([x, y]),
            ..
        }) = &e.pointer_sample
        {
            let interaction = interactions.entry(*id).or_default();
            interaction.handle_sample(*phase, *x, *y, view_parameters.as_ref(), sender);
            if interaction.is_complete() {
                interactions.remove(id);
            }
        }

        if let Some(fptr::TouchInteractionResult { interaction: id, status }) =
            &e.interaction_result
        {
            if let Some(interaction) = interactions.get_mut(id) {
                interaction.handle_result(*status, sender);
                if interaction.is_complete() {
                    interactions.remove(id);
                }
            }
        }
    }
}

pub(crate) fn spawn_touch_source_watcher(
    scope: &fasync::ScopeHandle,
    touch_source: fptr::TouchSourceProxy,
    sender: UnboundedSender<InternalMessage>,
) {
    scope.spawn(async move {
        let mut pending_responses: Vec<TouchResponse> = Vec::new();
        let mut interactions: HashMap<TouchInteractionId, Interaction> = HashMap::new();
        let mut view_parameters: Option<fptr::ViewParameters> = None;

        loop {
            match touch_source.watch(&pending_responses).await {
                Ok(events) => {
                    fill_touch_event_responses(&events, &mut pending_responses);
                    process_touch_events(&events, &mut interactions, &mut view_parameters, &sender);
                }
                Err(_) => {
                    error!("TouchSource connection closed");
                    return;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use futures::channel::mpsc::unbounded;

    fn sample_event(
        id: TouchInteractionId,
        phase: EventPhase,
        x: f32,
        y: f32,
        result: Option<TouchInteractionStatus>,
    ) -> TouchEvent {
        TouchEvent {
            pointer_sample: Some(fptr::TouchPointerSample {
                interaction: Some(id),
                phase: Some(phase),
                position_in_viewport: Some([x, y]),
                ..Default::default()
            }),
            interaction_result: result
                .map(|status| fptr::TouchInteractionResult { interaction: id, status }),
            ..Default::default()
        }
    }

    fn result_only_event(id: TouchInteractionId, status: TouchInteractionStatus) -> TouchEvent {
        TouchEvent {
            interaction_result: Some(fptr::TouchInteractionResult { interaction: id, status }),
            ..Default::default()
        }
    }

    fn scale_view_parameters(scale: f32) -> fptr::ViewParameters {
        let rect = fptr::Rectangle { min: [0.0, 0.0], max: [1000.0, 1000.0] };
        fptr::ViewParameters {
            view: rect,
            viewport: rect,
            viewport_to_view_transform: [scale, 0.0, 0.0, 0.0, scale, 0.0, 0.0, 0.0, 1.0],
        }
    }

    #[fuchsia::test]
    async fn test_tap_flushes_when_remove_arrives_with_granted() {
        let (sender, receiver) = unbounded();
        let mut interactions = HashMap::new();
        let mut view_parameters = None;
        let id = TouchInteractionId { device_id: 1, pointer_id: 1, interaction_id: 1 };

        let events = vec![
            sample_event(id, EventPhase::Add, 120.0, 240.0, None),
            sample_event(
                id,
                EventPhase::Remove,
                120.0,
                240.0,
                Some(TouchInteractionStatus::Granted),
            ),
        ];
        process_touch_events(&events, &mut interactions, &mut view_parameters, &sender);
        drop(sender);
        assert!(interactions.is_empty());

        let msgs: Vec<InternalMessage> = receiver.collect().await;
        assert_eq!(msgs, vec![InternalMessage::PointerTap { x: 120.0, y: 240.0 }]);
    }

    #[fuchsia::test]
    async fn test_swipe_emits_move_without_conflicting_tap() {
        let (sender, receiver) = unbounded();
        let mut interactions = HashMap::new();
        let mut view_parameters = None;
        let id = TouchInteractionId { device_id: 1, pointer_id: 1, interaction_id: 2 };

        let events = vec![
            sample_event(id, EventPhase::Add, 100.0, 300.0, Some(TouchInteractionStatus::Granted)),
            sample_event(id, EventPhase::Change, 100.0, 240.0, None),
            sample_event(id, EventPhase::Remove, 100.0, 240.0, None),
        ];
        process_touch_events(&events, &mut interactions, &mut view_parameters, &sender);
        drop(sender);
        assert!(interactions.is_empty());

        let msgs: Vec<InternalMessage> = receiver.collect().await;
        assert_eq!(msgs, vec![InternalMessage::Command(GameCommand::Move(Direction::Up))]);
    }

    #[fuchsia::test]
    async fn test_denied_tap_and_mid_stream_denial_clear_interactions_without_messages() {
        let (sender, receiver) = unbounded();
        let mut interactions = HashMap::new();
        let mut view_parameters = None;

        // Case 1: full tap with Denied.
        let id1 = TouchInteractionId { device_id: 1, pointer_id: 1, interaction_id: 3 };
        let events1 = vec![
            sample_event(id1, EventPhase::Add, 50.0, 60.0, None),
            sample_event(id1, EventPhase::Remove, 50.0, 60.0, Some(TouchInteractionStatus::Denied)),
        ];
        process_touch_events(&events1, &mut interactions, &mut view_parameters, &sender);
        assert!(interactions.is_empty());

        // Case 2: Add with finger still down, then a result-only Denied event.
        let id2 = TouchInteractionId { device_id: 1, pointer_id: 1, interaction_id: 4 };
        process_touch_events(
            &[sample_event(id2, EventPhase::Add, 50.0, 60.0, None)],
            &mut interactions,
            &mut view_parameters,
            &sender,
        );
        assert_eq!(interactions.len(), 1);
        process_touch_events(
            &[result_only_event(id2, TouchInteractionStatus::Denied)],
            &mut interactions,
            &mut view_parameters,
            &sender,
        );
        assert!(interactions.is_empty());

        drop(sender);
        let msgs: Vec<InternalMessage> = receiver.collect().await;
        assert!(msgs.is_empty());
    }

    #[fuchsia::test]
    async fn test_deferred_grant_and_canceled_swipe() {
        let (sender, receiver) = unbounded();
        let mut interactions = HashMap::new();
        let mut view_parameters = None;

        // Add and Remove in first batch, Granted in second batch.
        let id1 = TouchInteractionId { device_id: 1, pointer_id: 1, interaction_id: 5 };
        process_touch_events(
            &[
                sample_event(id1, EventPhase::Add, 80.0, 90.0, None),
                sample_event(id1, EventPhase::Remove, 80.0, 90.0, None),
            ],
            &mut interactions,
            &mut view_parameters,
            &sender,
        );
        assert_eq!(interactions.len(), 1);
        process_touch_events(
            &[result_only_event(id1, TouchInteractionStatus::Granted)],
            &mut interactions,
            &mut view_parameters,
            &sender,
        );
        assert!(interactions.is_empty());

        // Add, Change past swipe threshold, Cancel, then Granted: no messages.
        let id2 = TouchInteractionId { device_id: 1, pointer_id: 1, interaction_id: 6 };
        process_touch_events(
            &[
                sample_event(id2, EventPhase::Add, 100.0, 100.0, None),
                sample_event(id2, EventPhase::Change, 200.0, 100.0, None),
                sample_event(id2, EventPhase::Cancel, 200.0, 100.0, None),
                result_only_event(id2, TouchInteractionStatus::Granted),
            ],
            &mut interactions,
            &mut view_parameters,
            &sender,
        );
        assert!(interactions.is_empty());

        drop(sender);
        let msgs: Vec<InternalMessage> = receiver.collect().await;
        assert_eq!(msgs, vec![InternalMessage::PointerTap { x: 80.0, y: 90.0 }]);
    }

    #[fuchsia::test]
    async fn test_view_parameters_scale_tap_coordinates_across_batches() {
        let (sender, receiver) = unbounded();
        let mut interactions = HashMap::new();
        let mut view_parameters = None;
        let id = TouchInteractionId { device_id: 1, pointer_id: 1, interaction_id: 7 };

        // First batch delivers ViewParameters with a 0.5 viewport-to-view scale.
        let param_event =
            TouchEvent { view_parameters: Some(scale_view_parameters(0.5)), ..Default::default() };
        process_touch_events(&[param_event], &mut interactions, &mut view_parameters, &sender);

        // Second batch delivers a tap at (200, 400) in viewport coordinates.
        process_touch_events(
            &[
                sample_event(id, EventPhase::Add, 200.0, 400.0, None),
                sample_event(
                    id,
                    EventPhase::Remove,
                    200.0,
                    400.0,
                    Some(TouchInteractionStatus::Granted),
                ),
            ],
            &mut interactions,
            &mut view_parameters,
            &sender,
        );
        drop(sender);

        let msgs: Vec<InternalMessage> = receiver.collect().await;
        assert_eq!(msgs, vec![InternalMessage::PointerTap { x: 100.0, y: 200.0 }]);
    }

    #[fuchsia::test]
    fn test_fill_touch_event_responses_matches_events_length() {
        let id = TouchInteractionId { device_id: 1, pointer_id: 1, interaction_id: 8 };
        let events = vec![
            sample_event(id, EventPhase::Add, 10.0, 20.0, None),
            result_only_event(id, TouchInteractionStatus::Granted),
        ];
        let mut responses = Vec::new();
        fill_touch_event_responses(&events, &mut responses);
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0].response_type, Some(fptr::TouchResponseType::Yes));
        assert_eq!(responses[1].response_type, None);
    }
}
