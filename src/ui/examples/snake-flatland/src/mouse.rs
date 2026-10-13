// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::internal_message::InternalMessage;
use crate::touch::viewport_to_view_coordinates;
use fidl_fuchsia_ui_pointer as fptr;
use fuchsia_async as fasync;
use futures::channel::mpsc::UnboundedSender;
use log::error;

fn process_mouse_events(
    events: &[fptr::MouseEvent],
    was_pressed: &mut bool,
    view_parameters: &mut Option<fptr::ViewParameters>,
    sender: &UnboundedSender<InternalMessage>,
) {
    for e in events {
        if let Some(params) = &e.view_parameters {
            *view_parameters = Some(*params);
        }

        if let Some(pointer_sample) = &e.pointer_sample {
            let is_pressed =
                pointer_sample.pressed_buttons.as_ref().is_some_and(|buttons| !buttons.is_empty());
            let is_down_edge = is_pressed && !*was_pressed;
            *was_pressed = is_pressed;

            if is_down_edge {
                if let Some([raw_x, raw_y]) = pointer_sample.position_in_viewport {
                    let (x, y) =
                        viewport_to_view_coordinates(raw_x, raw_y, view_parameters.as_ref());
                    let _ = sender.unbounded_send(InternalMessage::PointerTap { x, y });
                }
            }
        }
    }
}

pub(crate) fn spawn_mouse_source_watcher(
    scope: &fasync::ScopeHandle,
    mouse_source: fptr::MouseSourceProxy,
    sender: UnboundedSender<InternalMessage>,
) {
    scope.spawn(async move {
        let mut was_pressed = false;
        let mut view_parameters: Option<fptr::ViewParameters> = None;

        loop {
            match mouse_source.watch().await {
                Ok(events) => {
                    process_mouse_events(&events, &mut was_pressed, &mut view_parameters, &sender);
                }
                Err(_) => {
                    error!("MouseSource connection closed");
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

    fn mouse_sample(
        x: f32,
        y: f32,
        buttons: Option<Vec<u8>>,
        view_parameters: Option<fptr::ViewParameters>,
    ) -> fptr::MouseEvent {
        fptr::MouseEvent {
            pointer_sample: Some(fptr::MousePointerSample {
                position_in_viewport: Some([x, y]),
                pressed_buttons: buttons,
                ..Default::default()
            }),
            view_parameters,
            ..Default::default()
        }
    }

    #[fuchsia::test]
    async fn test_process_mouse_events_emits_taps_only_on_down_edges_and_scales_coords() {
        let (sender, receiver) = unbounded();
        let mut was_pressed = false;
        let mut view_parameters = None;

        let rect = fptr::Rectangle { min: [0.0, 0.0], max: [1000.0, 1000.0] };
        let half_scale_params = fptr::ViewParameters {
            view: rect,
            viewport: rect,
            viewport_to_view_transform: [0.5, 0.0, 0.0, 0.0, 0.5, 0.0, 0.0, 0.0, 1.0],
        };

        // Sequence: press at (200, 400) with 0.5 scale -> held drag at (220, 420) -> release at
        // (240, 440) -> second press at (300, 500) in a second batch without repeating
        // view_parameters.
        let batch1 = vec![
            mouse_sample(200.0, 400.0, Some(vec![1]), Some(half_scale_params)),
            mouse_sample(220.0, 420.0, Some(vec![1]), None),
            mouse_sample(240.0, 440.0, None, None),
        ];
        process_mouse_events(&batch1, &mut was_pressed, &mut view_parameters, &sender);

        let batch2 = vec![mouse_sample(300.0, 500.0, Some(vec![1]), None)];
        process_mouse_events(&batch2, &mut was_pressed, &mut view_parameters, &sender);
        drop(sender);

        let msgs: Vec<InternalMessage> = receiver.collect().await;
        assert_eq!(
            msgs,
            vec![
                InternalMessage::PointerTap { x: 100.0, y: 200.0 },
                InternalMessage::PointerTap { x: 150.0, y: 250.0 },
            ]
        );
    }
}
