// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::input_device::{self, Handled, InputDeviceBinding, InputDeviceStatus, InputEvent};
use crate::utils::{Position, Size};
use crate::{metrics, mouse_binding};
use anyhow::{Context, Error, format_err};
use async_trait::async_trait;
use fidl_fuchsia_input_report::{InputDeviceProxy, InputReport};
use fuchsia_inspect::ArrayProperty;
use fuchsia_inspect::health::Reporter;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use zx;

use fidl::HandleBased;
use maplit::hashmap;
use metrics_registry::*;
use std::collections::{HashMap, HashSet};
use {
    fidl_fuchsia_input_report as fidl_input_report, fidl_fuchsia_ui_input as fidl_ui_input,
    fidl_fuchsia_ui_pointerinjector as pointerinjector,
};

/// A [`TouchScreenEvent`] represents a set of contacts and the phase those contacts are in.
///
/// For example, when a user touches a touch screen with two fingers, there will be two
/// [`TouchContact`]s. When a user removes one finger, there will still be two contacts
/// but one will be reported as removed.
///
/// The expected sequence for any given contact is:
/// 1. [`fidl_fuchsia_ui_input::PointerEventPhase::Add`]
/// 2. [`fidl_fuchsia_ui_input::PointerEventPhase::Down`]
/// 3. 0 or more [`fidl_fuchsia_ui_input::PointerEventPhase::Move`]
/// 4. [`fidl_fuchsia_ui_input::PointerEventPhase::Up`]
/// 5. [`fidl_fuchsia_ui_input::PointerEventPhase::Remove`]
///
/// Additionally, a [`fidl_fuchsia_ui_input::PointerEventPhase::Cancel`] may be sent at any time
/// signalling that the event is no longer directed towards the receiver.
#[derive(Debug, PartialEq)]
pub struct TouchScreenEvent {
    /// Deprecated. To be removed with https://fxbug.dev/42155652.
    /// The contacts associated with the touch event. For example, a two-finger touch would result
    /// in one touch event with two [`TouchContact`]s.
    ///
    /// Contacts are grouped based on their current phase (e.g., down, move).
    pub contacts: HashMap<fidl_ui_input::PointerEventPhase, Vec<TouchContact>>,

    /// The contacts associated with the touch event. For example, a two-finger touch would result
    /// in one touch event with two [`TouchContact`]s.
    ///
    /// Contacts are grouped based on their current phase (e.g., add, change).
    pub injector_contacts: HashMap<pointerinjector::EventPhase, Vec<TouchContact>>,

    /// Indicates whether any touch buttons are pressed.
    pub pressed_buttons: Vec<fidl_input_report::TouchButton>,

    /// The wake lease for this event.
    pub wake_lease: Option<zx::EventPair>,
}

impl Clone for TouchScreenEvent {
    fn clone(&self) -> Self {
        log::debug!("TouchScreenEvent cloned without wake lease.");
        Self {
            contacts: self.contacts.clone(),
            injector_contacts: self.injector_contacts.clone(),
            pressed_buttons: self.pressed_buttons.clone(),
            wake_lease: None,
        }
    }
}

impl Drop for TouchScreenEvent {
    fn drop(&mut self) {
        log::debug!("TouchScreenEvent dropped, had_wake_lease: {:?}", self.wake_lease);
    }
}

impl TouchScreenEvent {
    pub fn clone_with_wake_lease(&self) -> Self {
        log::debug!("TouchScreenEvent cloned with wake lease: {:?}", self.wake_lease);
        Self {
            contacts: self.contacts.clone(),
            injector_contacts: self.injector_contacts.clone(),
            pressed_buttons: self.pressed_buttons.clone(),
            wake_lease: self.wake_lease.as_ref().map(|lease| {
                lease
                    .duplicate_handle(zx::Rights::SAME_RIGHTS)
                    .expect("failed to duplicate event pair")
            }),
        }
    }

    pub fn record_inspect(&self, node: &fuchsia_inspect::Node) {
        let contacts_clone = self.injector_contacts.clone();
        node.record_child("injector_contacts", move |contacts_node| {
            for (phase, contacts) in contacts_clone.iter() {
                let phase_str = match phase {
                    pointerinjector::EventPhase::Add => "add",
                    pointerinjector::EventPhase::Change => "change",
                    pointerinjector::EventPhase::Remove => "remove",
                    pointerinjector::EventPhase::Cancel => "cancel",
                };
                contacts_node.record_child(phase_str, move |phase_node| {
                    for contact in contacts.iter() {
                        phase_node.record_child(contact.id.to_string(), move |contact_node| {
                            contact_node
                                .record_double("position_x_mm", f64::from(contact.position.x));
                            contact_node
                                .record_double("position_y_mm", f64::from(contact.position.y));
                            if let Some(pressure) = contact.pressure {
                                contact_node.record_int("pressure", pressure);
                            }
                            if let Some(contact_size) = contact.contact_size {
                                contact_node.record_double(
                                    "contact_width_mm",
                                    f64::from(contact_size.width),
                                );
                                contact_node.record_double(
                                    "contact_height_mm",
                                    f64::from(contact_size.height),
                                );
                            }
                        });
                    }
                });
            }
        });

        let pressed_buttons_node =
            node.create_string_array("pressed_buttons", self.pressed_buttons.len());
        self.pressed_buttons.iter().enumerate().for_each(|(i, &ref button)| {
            let button_name: String = match button {
                fidl_input_report::TouchButton::Palm => "palm".into(),
                unknown_value => {
                    format!("unknown({:?})", unknown_value)
                }
            };
            pressed_buttons_node.set(i, &button_name);
        });
        node.record(pressed_buttons_node);
    }
}

/// A [`TouchpadEvent`] represents a set of contacts.
///
/// For example, when a user touches a touch screen with two fingers, there will be two
/// [`TouchContact`]s in the vector.
#[derive(Clone, Debug, PartialEq)]
pub struct TouchpadEvent {
    /// The contacts associated with the touch event. For example, a two-finger touch would result
    /// in one touch event with two [`TouchContact`]s.
    pub injector_contacts: Vec<TouchContact>,

    /// The complete button state including this event.
    pub pressed_buttons: HashSet<mouse_binding::MouseButton>,
}

impl TouchpadEvent {
    pub fn record_inspect(&self, node: &fuchsia_inspect::Node) {
        let pressed_buttons_node =
            node.create_uint_array("pressed_buttons", self.pressed_buttons.len());
        self.pressed_buttons.iter().enumerate().for_each(|(i, button)| {
            pressed_buttons_node.set(i, *button);
        });
        node.record(pressed_buttons_node);

        // Populate TouchpadEvent contact details.
        let contacts_clone = self.injector_contacts.clone();
        node.record_child("injector_contacts", move |contacts_node| {
            for contact in contacts_clone.iter() {
                contacts_node.record_child(contact.id.to_string(), move |contact_node| {
                    contact_node.record_double("position_x_mm", f64::from(contact.position.x));
                    contact_node.record_double("position_y_mm", f64::from(contact.position.y));
                    if let Some(pressure) = contact.pressure {
                        contact_node.record_int("pressure", pressure);
                    }
                    if let Some(contact_size) = contact.contact_size {
                        contact_node
                            .record_double("contact_width_mm", f64::from(contact_size.width));
                        contact_node
                            .record_double("contact_height_mm", f64::from(contact_size.height));
                    }
                })
            }
        });
    }
}

/// [`TouchDeviceType`] indicates the type of touch device. Both Touch Screen and Windows Precision
/// Touchpad send touch event from driver but need different process inside input pipeline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TouchDeviceType {
    TouchScreen,
    WindowsPrecisionTouchpad,
}

/// A [`TouchContact`] represents a single contact (e.g., one touch of a multi-touch gesture) related
/// to a touch event.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TouchContact {
    /// The identifier of the contact. Unique per touch device.
    pub id: u32,

    /// The position of the touch event, in the units of the associated
    /// [`ContactDeviceDescriptor`]'s `range`.
    pub position: Position,

    /// The pressure associated with the contact, in the units of the associated
    /// [`ContactDeviceDescriptor`]'s `pressure_range`.
    pub pressure: Option<i64>,

    /// The size of the touch event, in the units of the associated
    /// [`ContactDeviceDescriptor`]'s `range`.
    pub contact_size: Option<Size>,
}

impl Eq for TouchContact {}

impl From<&fidl_fuchsia_input_report::ContactInputReport> for TouchContact {
    fn from(fidl_contact: &fidl_fuchsia_input_report::ContactInputReport) -> TouchContact {
        let contact_size =
            if fidl_contact.contact_width.is_some() && fidl_contact.contact_height.is_some() {
                Some(Size {
                    width: fidl_contact.contact_width.unwrap() as f32,
                    height: fidl_contact.contact_height.unwrap() as f32,
                })
            } else {
                None
            };

        TouchContact {
            id: fidl_contact.contact_id.unwrap_or_default(),
            position: Position {
                x: fidl_contact.position_x.unwrap_or_default() as f32,
                y: fidl_contact.position_y.unwrap_or_default() as f32,
            },
            pressure: fidl_contact.pressure,
            contact_size,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TouchScreenDeviceDescriptor {
    /// The id of the connected touch screen input device.
    pub device_id: u32,

    /// The descriptors for the possible contacts associated with the device.
    pub contacts: Vec<ContactDeviceDescriptor>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TouchpadDeviceDescriptor {
    /// The id of the connected touchpad input device.
    pub device_id: u32,

    /// The descriptors for the possible contacts associated with the device.
    pub contacts: Vec<ContactDeviceDescriptor>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum TouchDeviceDescriptor {
    TouchScreen(TouchScreenDeviceDescriptor),
    Touchpad(TouchpadDeviceDescriptor),
}

/// A [`ContactDeviceDescriptor`] describes the possible values touch contact properties can take on.
///
/// This descriptor can be used, for example, to determine where on a screen a touch made contact.
///
/// # Example
///
/// ```
/// // Determine the scaling factor between the display and the touch device's x range.
/// let scaling_factor =
///     display_width / (contact_descriptor._x_range.end - contact_descriptor._x_range.start);
/// // Use the scaling factor to scale the contact report's x position.
/// let hit_location =
///     scaling_factor * (contact_report.position_x - contact_descriptor._x_range.start);
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContactDeviceDescriptor {
    /// The range of possible x values for this touch contact.
    pub x_range: fidl_input_report::Range,

    /// The range of possible y values for this touch contact.
    pub y_range: fidl_input_report::Range,

    /// The unit of measure for `x_range`.
    pub x_unit: fidl_input_report::Unit,

    /// The unit of measure for `y_range`.
    pub y_unit: fidl_input_report::Unit,

    /// The range of possible pressure values for this touch contact.
    pub pressure_range: Option<fidl_input_report::Range>,

    /// The range of possible widths for this touch contact.
    pub width_range: Option<fidl_input_report::Range>,

    /// The range of possible heights for this touch contact.
    pub height_range: Option<fidl_input_report::Range>,
}

/// A [`TouchBinding`] represents a connection to a touch input device.
///
/// The [`TouchBinding`] parses and exposes touch descriptor properties (e.g., the range of
/// possible x values for touch contacts) for the device it is associated with.
/// It also parses [`InputReport`]s from the device, and sends them to the device binding owner over
/// `event_sender`.
pub struct TouchBinding {
    /// The channel to stream InputEvents to.
    event_sender: UnboundedSender<Vec<InputEvent>>,

    /// Holds information about this device.
    device_descriptor: TouchDeviceDescriptor,

    /// Touch device type of the touch device.
    touch_device_type: TouchDeviceType,

    /// Proxy to the device.
    device_proxy: InputDeviceProxy,
}

#[async_trait]
impl input_device::InputDeviceBinding for TouchBinding {
    fn input_event_sender(&self) -> UnboundedSender<Vec<InputEvent>> {
        self.event_sender.clone()
    }

    fn get_device_descriptor(&self) -> input_device::InputDeviceDescriptor {
        match self.device_descriptor.clone() {
            TouchDeviceDescriptor::TouchScreen(desc) => {
                input_device::InputDeviceDescriptor::TouchScreen(desc)
            }
            TouchDeviceDescriptor::Touchpad(desc) => {
                input_device::InputDeviceDescriptor::Touchpad(desc)
            }
        }
    }
}

impl TouchBinding {
    /// Creates a new [`InputDeviceBinding`] from the `device_proxy`.
    ///
    /// The binding will start listening for input reports immediately and send new InputEvents
    /// to the device binding owner over `input_event_sender`.
    ///
    /// # Parameters
    /// - `device_proxy`: The proxy to bind the new [`InputDeviceBinding`] to.
    /// - `device_id`: The id of the connected touch device.
    /// - `input_event_sender`: The channel to send new InputEvents to.
    /// - `device_node`: The inspect node for this device binding
    /// - `metrics_logger`: The metrics logger.
    ///
    /// # Errors
    /// If there was an error binding to the proxy.
    pub async fn new(
        device_proxy: InputDeviceProxy,
        device_id: u32,
        input_event_sender: UnboundedSender<Vec<InputEvent>>,
        device_node: fuchsia_inspect::Node,
        feature_flags: input_device::InputPipelineFeatureFlags,
        metrics_logger: metrics::MetricsLogger,
    ) -> Result<Self, Error> {
        let (device_binding, mut inspect_status) =
            Self::bind_device(device_proxy.clone(), device_id, input_event_sender, device_node)
                .await?;
        device_binding
            .set_touchpad_mode(true)
            .await
            .with_context(|| format!("enabling touchpad mode for device {}", device_id))?;
        inspect_status.health_node.set_ok();
        input_device::initialize_report_stream(
            device_proxy,
            device_binding.get_device_descriptor(),
            device_binding.input_event_sender(),
            inspect_status,
            metrics_logger,
            feature_flags,
            Self::process_reports,
        );

        Ok(device_binding)
    }

    /// Binds the provided input device to a new instance of `Self`.
    ///
    /// # Parameters
    /// - `device`: The device to use to initialize the binding.
    /// - `device_id`: The id of the connected touch device.
    /// - `input_event_sender`: The channel to send new InputEvents to.
    /// - `device_node`: The inspect node for this device binding
    ///
    /// # Errors
    /// If the device descriptor could not be retrieved, or the descriptor could not be parsed
    /// correctly.
    async fn bind_device(
        device_proxy: InputDeviceProxy,
        device_id: u32,
        input_event_sender: UnboundedSender<Vec<InputEvent>>,
        device_node: fuchsia_inspect::Node,
    ) -> Result<(Self, InputDeviceStatus), Error> {
        let mut input_device_status = InputDeviceStatus::new(device_node);
        let device_descriptor: fidl_input_report::DeviceDescriptor = match device_proxy
            .get_descriptor()
            .await
        {
            Ok(descriptor) => descriptor,
            Err(_) => {
                input_device_status.health_node.set_unhealthy("Could not get device descriptor.");
                return Err(format_err!("Could not get descriptor for device_id: {}", device_id));
            }
        };

        let touch_device_type = get_device_type(&device_proxy).await;

        match device_descriptor.touch {
            Some(fidl_fuchsia_input_report::TouchDescriptor {
                input:
                    Some(fidl_fuchsia_input_report::TouchInputDescriptor {
                        contacts: Some(contact_descriptors),
                        max_contacts: _,
                        touch_type: _,
                        buttons: _,
                        ..
                    }),
                ..
            }) => Ok((
                TouchBinding {
                    event_sender: input_event_sender,
                    device_descriptor: match touch_device_type {
                        TouchDeviceType::TouchScreen => {
                            TouchDeviceDescriptor::TouchScreen(TouchScreenDeviceDescriptor {
                                device_id,
                                contacts: contact_descriptors
                                    .iter()
                                    .map(TouchBinding::parse_contact_descriptor)
                                    .filter_map(Result::ok)
                                    .collect(),
                            })
                        }
                        TouchDeviceType::WindowsPrecisionTouchpad => {
                            TouchDeviceDescriptor::Touchpad(TouchpadDeviceDescriptor {
                                device_id,
                                contacts: contact_descriptors
                                    .iter()
                                    .map(TouchBinding::parse_contact_descriptor)
                                    .filter_map(Result::ok)
                                    .collect(),
                            })
                        }
                    },
                    touch_device_type,
                    device_proxy,
                },
                input_device_status,
            )),
            descriptor => {
                input_device_status
                    .health_node
                    .set_unhealthy("Touch Device Descriptor failed to parse.");
                Err(format_err!("Touch Descriptor failed to parse: \n {:?}", descriptor))
            }
        }
    }

    async fn set_touchpad_mode(&self, enable: bool) -> Result<(), Error> {
        match self.touch_device_type {
            TouchDeviceType::TouchScreen => Ok(()),
            TouchDeviceType::WindowsPrecisionTouchpad => {
                // `get_feature_report` to only modify the input_mode and
                // keep other feature as is.
                let mut report = match self.device_proxy.get_feature_report().await? {
                    Ok(report) => report,
                    Err(e) => return Err(format_err!("get_feature_report failed: {}", e)),
                };
                let mut touch =
                    report.touch.unwrap_or_else(fidl_input_report::TouchFeatureReport::default);
                touch.input_mode = match enable {
                            true => Some(fidl_input_report::TouchConfigurationInputMode::WindowsPrecisionTouchpadCollection),
                            false => Some(fidl_input_report::TouchConfigurationInputMode::MouseCollection),
                        };
                report.touch = Some(touch);
                match self.device_proxy.set_feature_report(&report).await? {
                    Ok(()) => {
                        // TODO(https://fxbug.dev/42056283): Remove log message.
                        log::info!("touchpad: set touchpad_enabled to {}", enable);
                        Ok(())
                    }
                    Err(e) => Err(format_err!("set_feature_report failed: {}", e)),
                }
            }
        }
    }

    /// Parses an [`InputReport`] into one or more [`InputEvent`]s.
    ///
    /// The [`InputEvent`]s are sent to the device binding owner via [`input_event_sender`].
    ///
    /// # Parameters
    /// - `reports`: The incoming [`InputReport`].
    /// - `previous_report`: The previous [`InputReport`] seen for the same device. This can be
    ///                    used to determine, for example, which keys are no longer present in
    ///                    a keyboard report to generate key released events. If `None`, no
    ///                    previous report was found.
    /// - `device_descriptor`: The descriptor for the input device generating the input reports.
    /// - `input_event_sender`: The sender for the device binding's input event stream.
    ///
    /// # Returns
    /// An [`InputReport`] which will be passed to the next call to [`process_reports`], as
    /// [`previous_report`]. If `None`, the next call's [`previous_report`] will be `None`.
    /// A [`UnboundedReceiver<InputEvent>`] which will poll asynchronously generated events to be
    /// recorded by `inspect_status` in `input_device::initialize_report_stream()`. If device
    /// binding does not generate InputEvents asynchronously, this will be `None`.
    ///
    /// The returned [`InputReport`] is guaranteed to have no `wake_lease`.
    fn process_reports(
        reports: Vec<InputReport>,
        previous_report: Option<InputReport>,
        device_descriptor: &input_device::InputDeviceDescriptor,
        input_event_sender: &mut UnboundedSender<Vec<InputEvent>>,
        inspect_status: &InputDeviceStatus,
        metrics_logger: &metrics::MetricsLogger,
        feature_flags: &input_device::InputPipelineFeatureFlags,
    ) -> (Option<InputReport>, Option<UnboundedReceiver<InputEvent>>) {
        fuchsia_trace::duration!(
            "input",
            "touch-binding-process-report",
            "num_reports" => reports.len(),
        );
        match device_descriptor {
            input_device::InputDeviceDescriptor::TouchScreen(_) => process_touch_screen_reports(
                reports,
                previous_report,
                device_descriptor,
                input_event_sender,
                inspect_status,
                metrics_logger,
                feature_flags.enable_merge_touch_events,
            ),
            input_device::InputDeviceDescriptor::Touchpad(_) => process_touchpad_reports(
                reports,
                previous_report,
                device_descriptor,
                input_event_sender,
                inspect_status,
                metrics_logger,
            ),
            _ => (previous_report, None),
        }
    }

    /// Parses a fidl_input_report contact descriptor into a [`ContactDeviceDescriptor`]
    ///
    /// # Parameters
    /// - `contact_device_descriptor`: The contact descriptor to parse.
    ///
    /// # Errors
    /// If the contact description fails to parse because required fields aren't present.
    fn parse_contact_descriptor(
        contact_device_descriptor: &fidl_input_report::ContactInputDescriptor,
    ) -> Result<ContactDeviceDescriptor, Error> {
        match contact_device_descriptor {
            fidl_input_report::ContactInputDescriptor {
                position_x: Some(x_axis),
                position_y: Some(y_axis),
                pressure: pressure_axis,
                contact_width: width_axis,
                contact_height: height_axis,
                ..
            } => Ok(ContactDeviceDescriptor {
                x_range: x_axis.range,
                y_range: y_axis.range,
                x_unit: x_axis.unit,
                y_unit: y_axis.unit,
                pressure_range: pressure_axis.map(|axis| axis.range),
                width_range: width_axis.map(|axis| axis.range),
                height_range: height_axis.map(|axis| axis.range),
            }),
            descriptor => {
                Err(format_err!("Touch Contact Descriptor failed to parse: \n {:?}", descriptor))
            }
        }
    }
}

fn is_move_only(event: &InputEvent) -> bool {
    matches!(
        &event.device_event,
        input_device::InputDeviceEvent::TouchScreen(event)
            if event
                .injector_contacts
                .get(&pointerinjector::EventPhase::Add)
                .map_or(true, |c| c.is_empty())
                && event
                    .injector_contacts
                    .get(&pointerinjector::EventPhase::Remove)
                    .map_or(true, |c| c.is_empty())
                && event
                    .injector_contacts
                    .get(&pointerinjector::EventPhase::Cancel)
                    .map_or(true, |c| c.is_empty())
    )
}

fn has_pressed_buttons(event: &InputEvent) -> bool {
    match &event.device_event {
        input_device::InputDeviceEvent::TouchScreen(event) => !event.pressed_buttons.is_empty(),
        _ => false,
    }
}

fn process_touch_screen_reports(
    reports: Vec<InputReport>,
    mut previous_report: Option<InputReport>,
    device_descriptor: &input_device::InputDeviceDescriptor,
    input_event_sender: &mut UnboundedSender<Vec<InputEvent>>,
    inspect_status: &InputDeviceStatus,
    metrics_logger: &metrics::MetricsLogger,
    enable_merge_touch_events: bool,
) -> (Option<InputReport>, Option<UnboundedReceiver<InputEvent>>) {
    let num_reports = reports.len();
    let mut batch: Vec<InputEvent> = Vec::with_capacity(num_reports);
    for report in reports {
        inspect_status.count_received_report(&report);
        let (prev_report, event) = process_single_touch_screen_report(
            report,
            previous_report,
            device_descriptor,
            inspect_status,
        );
        previous_report = prev_report;
        if let Some(event) = event {
            batch.push(event);
        }
    }

    if !batch.is_empty() {
        if enable_merge_touch_events {
            // Pre-calculate move-only status for all events
            let mut is_event_move_only: Vec<bool> = Vec::with_capacity(batch.len());
            let mut pressed_buttons: Vec<bool> = Vec::with_capacity(batch.len());
            for event in &batch {
                is_event_move_only.push(is_move_only(event));
                pressed_buttons.push(has_pressed_buttons(event));
            }
            let size_of_batch = batch.len();

            // Merge consecutive move-only events into a single event.
            let mut merged_batch = Vec::with_capacity(size_of_batch);

            // Use into_iter().enumerate() to move elements without cloning
            for (i, current_event) in batch.into_iter().enumerate() {
                let current_is_move = is_event_move_only[i];
                let current_pressed_buttons = pressed_buttons[i];
                let is_last_event = i == size_of_batch - 1;

                // Check if the NEXT event is also move-only
                let next_is_move =
                    if i + 1 < size_of_batch { is_event_move_only[i + 1] } else { false };

                let next_pressed_buttons = if i + 1 < size_of_batch {
                    pressed_buttons[i + 1]
                } else {
                    current_pressed_buttons
                };

                // If both are move-only, skip the current one (it's redundant).
                // always keep the last event
                if !is_last_event
                    // both are move-only
                    && (current_is_move && next_is_move)
                    // same pressed buttons
                    && (current_pressed_buttons == next_pressed_buttons)
                {
                    continue;
                }

                merged_batch.push(current_event);
            }

            batch = merged_batch;
        }

        let events_to_send: Vec<InputEvent> = batch
            .iter()
            .map(|e| {
                let event = e.clone_with_wake_lease();
                // Unwrap is safe because trace_id is set when the event is created.
                // This unwrap will not move the trace_id out of the event because trace_id has Copy trait.
                let trace_id: fuchsia_trace::Id = event.trace_id.unwrap();
                fuchsia_trace::flow_begin!("input", "event_in_input_pipeline", trace_id);
                event
            })
            .collect();
        fuchsia_trace::instant!(
            "input",
            "events_to_input_handlers",
            fuchsia_trace::Scope::Thread,
            "num_reports" => num_reports,
            "num_events_generated" => events_to_send.len()
        );
        match input_event_sender.unbounded_send(events_to_send) {
            Err(e) => {
                metrics_logger.log_error(
                    InputPipelineErrorMetricDimensionEvent::TouchFailedToSendTouchScreenEvent,
                    std::format!("Failed to send TouchScreenEvent with error: {:?}", e),
                );
            }
            _ => {
                inspect_status.count_generated_events(&batch);
            }
        }
    }
    (previous_report, None)
}

fn process_single_touch_screen_report(
    mut report: InputReport,
    previous_report: Option<InputReport>,
    device_descriptor: &input_device::InputDeviceDescriptor,
    inspect_status: &InputDeviceStatus,
) -> (Option<InputReport>, Option<InputEvent>) {
    fuchsia_trace::flow_end!("input", "input_report", report.trace_id.unwrap_or(0).into());

    // Input devices can have multiple types so ensure `report` is a TouchInputReport.
    let touch_report: &fidl_fuchsia_input_report::TouchInputReport = match &report.touch {
        Some(touch) => touch,
        None => {
            inspect_status.count_filtered_report();
            return (previous_report, None);
        }
    };

    let (previous_contacts, previous_buttons): (
        HashMap<u32, TouchContact>,
        Vec<fidl_fuchsia_input_report::TouchButton>,
    ) = previous_report
        .as_ref()
        .and_then(|unwrapped_report| unwrapped_report.touch.as_ref())
        .map(touch_contacts_and_buttons_from_touch_report)
        .unwrap_or_default();
    let (current_contacts, current_buttons): (
        HashMap<u32, TouchContact>,
        Vec<fidl_fuchsia_input_report::TouchButton>,
    ) = touch_contacts_and_buttons_from_touch_report(touch_report);

    // Don't send an event if there are no new contacts or pressed buttons.
    if previous_contacts.is_empty()
        && current_contacts.is_empty()
        && previous_buttons.is_empty()
        && current_buttons.is_empty()
    {
        inspect_status.count_filtered_report();
        return (Some(report), None);
    }

    // A touch input report containing button state should persist prior contact state,
    // for the sake of pointer event continuity in future reports containing contact data.
    if current_contacts.is_empty()
        && !previous_contacts.is_empty()
        && (!current_buttons.is_empty() || !previous_buttons.is_empty())
    {
        if let Some(touch_report) = report.touch.as_mut() {
            touch_report.contacts =
                previous_report.unwrap().touch.as_ref().unwrap().contacts.clone();
        }
    }

    // Contacts which exist only in current.
    let added_contacts: Vec<TouchContact> = Vec::from_iter(
        current_contacts
            .values()
            .cloned()
            .filter(|contact| !previous_contacts.contains_key(&contact.id)),
    );
    // Contacts which exist in both previous and current.
    let moved_contacts: Vec<TouchContact> = Vec::from_iter(
        current_contacts
            .values()
            .cloned()
            .filter(|contact| previous_contacts.contains_key(&contact.id)),
    );
    // Contacts which exist only in previous.
    let removed_contacts: Vec<TouchContact> =
        Vec::from_iter(previous_contacts.values().cloned().filter(|contact| {
            current_buttons.is_empty()
                && previous_buttons.is_empty()
                && !current_contacts.contains_key(&contact.id)
        }));

    let trace_id = fuchsia_trace::Id::random();
    let event = create_touch_screen_event(
        hashmap! {
            fidl_ui_input::PointerEventPhase::Add => added_contacts.clone(),
            fidl_ui_input::PointerEventPhase::Down => added_contacts.clone(),
            fidl_ui_input::PointerEventPhase::Move => moved_contacts.clone(),
            fidl_ui_input::PointerEventPhase::Up => removed_contacts.clone(),
            fidl_ui_input::PointerEventPhase::Remove => removed_contacts.clone(),
        },
        hashmap! {
            pointerinjector::EventPhase::Add => added_contacts,
            pointerinjector::EventPhase::Change => moved_contacts,
            pointerinjector::EventPhase::Remove => removed_contacts,
        },
        current_buttons,
        device_descriptor,
        trace_id,
        report.wake_lease.take(),
    );

    (Some(report), Some(event))
}

fn process_touchpad_reports(
    reports: Vec<InputReport>,
    mut previous_report: Option<InputReport>,
    device_descriptor: &input_device::InputDeviceDescriptor,
    input_event_sender: &mut UnboundedSender<Vec<InputEvent>>,
    inspect_status: &InputDeviceStatus,
    metrics_logger: &metrics::MetricsLogger,
) -> (Option<InputReport>, Option<UnboundedReceiver<InputEvent>>) {
    for report in reports {
        inspect_status.count_received_report(&report);
        let prev_report = process_single_touchpad_report(
            report,
            previous_report,
            device_descriptor,
            input_event_sender,
            inspect_status,
            metrics_logger,
        );
        previous_report = prev_report;
    }
    (previous_report, None)
}

fn process_single_touchpad_report(
    report: InputReport,
    previous_report: Option<InputReport>,
    device_descriptor: &input_device::InputDeviceDescriptor,
    input_event_sender: &mut UnboundedSender<Vec<InputEvent>>,
    inspect_status: &InputDeviceStatus,
    metrics_logger: &metrics::MetricsLogger,
) -> Option<InputReport> {
    if let Some(trace_id) = report.trace_id {
        fuchsia_trace::flow_end!("input", "input_report", trace_id.into());
    }

    // Input devices can have multiple types so ensure `report` is a TouchInputReport.
    let touch_report: &fidl_fuchsia_input_report::TouchInputReport = match &report.touch {
        Some(touch) => touch,
        None => {
            inspect_status.count_filtered_report();
            return previous_report;
        }
    };

    let current_contacts: Vec<TouchContact> = touch_report
        .contacts
        .as_ref()
        .and_then(|unwrapped_contacts| {
            // Once the contacts are found, convert them into `TouchContact`s.
            Some(unwrapped_contacts.iter().map(TouchContact::from).collect())
        })
        .unwrap_or_default();

    let buttons: HashSet<mouse_binding::MouseButton> = match &touch_report.pressed_buttons {
        Some(buttons) => HashSet::from_iter(buttons.iter().filter_map(|button| match button {
            fidl_fuchsia_input_report::TouchButton::Palm => Some(1),
            fidl_fuchsia_input_report::TouchButton::SwipeUp
            | fidl_fuchsia_input_report::TouchButton::SwipeLeft
            | fidl_fuchsia_input_report::TouchButton::SwipeRight
            | fidl_fuchsia_input_report::TouchButton::SwipeDown => {
                // TODO(https://fxbug.dev/487728300): Support swipe buttons.
                log::warn!("Swipe buttons {:?} are not supported", button);
                None
            }
            fidl_fuchsia_input_report::TouchButton::__SourceBreaking { unknown_ordinal } => {
                log::warn!("unknown TouchButton ordinal {unknown_ordinal:?}");
                None
            }
        })),
        None => HashSet::new(),
    };

    let trace_id = fuchsia_trace::Id::random();
    fuchsia_trace::flow_begin!("input", "event_in_input_pipeline", trace_id);
    send_touchpad_event(
        current_contacts,
        buttons,
        device_descriptor,
        input_event_sender,
        trace_id,
        inspect_status,
        metrics_logger,
    );

    Some(report)
}

fn touch_contacts_and_buttons_from_touch_report(
    touch_report: &fidl_fuchsia_input_report::TouchInputReport,
) -> (HashMap<u32, TouchContact>, Vec<fidl_fuchsia_input_report::TouchButton>) {
    // First unwrap all the optionals in the input report to get to the contacts.
    let contacts: Vec<TouchContact> = touch_report
        .contacts
        .as_ref()
        .and_then(|unwrapped_contacts| {
            // Once the contacts are found, convert them into `TouchContact`s.
            Some(unwrapped_contacts.iter().map(TouchContact::from).collect())
        })
        .unwrap_or_default();

    (
        contacts.into_iter().map(|contact| (contact.id, contact)).collect(),
        touch_report.pressed_buttons.clone().unwrap_or_default(),
    )
}

/// Create a TouchScreenEvent.
///
/// # Parameters
/// - `contacts`: The contact points relevant to the new TouchScreenEvent.
/// - `injector_contacts`: The contact points relevant to the new TouchScreenEvent, used to send
///                        pointer events into Scenic.
/// - `device_descriptor`: The descriptor for the input device generating the input reports.
/// - `trace_id`: The trace id to distinguish the event.
/// - `wake_lease`: The wake lease to send with the event.
fn create_touch_screen_event(
    contacts: HashMap<fidl_ui_input::PointerEventPhase, Vec<TouchContact>>,
    injector_contacts: HashMap<pointerinjector::EventPhase, Vec<TouchContact>>,
    pressed_buttons: Vec<fidl_input_report::TouchButton>,
    device_descriptor: &input_device::InputDeviceDescriptor,
    trace_id: fuchsia_trace::Id,
    wake_lease: Option<zx::EventPair>,
) -> InputEvent {
    input_device::InputEvent {
        device_event: input_device::InputDeviceEvent::TouchScreen(TouchScreenEvent {
            contacts,
            injector_contacts,
            pressed_buttons,
            wake_lease,
        }),
        device_descriptor: device_descriptor.clone(),
        event_time: zx::MonotonicInstant::get(),
        handled: Handled::No,
        trace_id: Some(trace_id),
    }
}

/// Sends a TouchpadEvent over `input_event_sender`.
///
/// # Parameters
/// - `injector_contacts`: The contact points relevant to the new TouchpadEvent.
/// - `pressed_buttons`: The pressing button of the new TouchpadEvent.
/// - `device_descriptor`: The descriptor for the input device generating the input reports.
/// - `input_event_sender`: The sender for the device binding's input event stream.
fn send_touchpad_event(
    injector_contacts: Vec<TouchContact>,
    pressed_buttons: HashSet<mouse_binding::MouseButton>,
    device_descriptor: &input_device::InputDeviceDescriptor,
    input_event_sender: &mut UnboundedSender<Vec<input_device::InputEvent>>,
    trace_id: fuchsia_trace::Id,
    inspect_status: &InputDeviceStatus,
    metrics_logger: &metrics::MetricsLogger,
) {
    let event = input_device::InputEvent {
        device_event: input_device::InputDeviceEvent::Touchpad(TouchpadEvent {
            injector_contacts,
            pressed_buttons,
        }),
        device_descriptor: device_descriptor.clone(),
        event_time: zx::MonotonicInstant::get(),
        handled: Handled::No,
        trace_id: Some(trace_id),
    };

    match input_event_sender.unbounded_send(vec![event.clone()]) {
        Err(e) => {
            metrics_logger.log_error(
                InputPipelineErrorMetricDimensionEvent::TouchFailedToSendTouchpadEvent,
                std::format!("Failed to send TouchpadEvent with error: {:?}", e),
            );
        }
        _ => inspect_status.count_generated_event(event),
    }
}

/// [`get_device_type`] check if the touch device is a touchscreen or Windows Precision Touchpad.
///
/// Windows Precision Touchpad reports `MouseCollection` or `WindowsPrecisionTouchpadCollection`
/// in `TouchFeatureReport`. Fallback all error responses on `get_feature_report` to TouchScreen
/// because some touch screen does not report this method.
async fn get_device_type(input_device: &fidl_input_report::InputDeviceProxy) -> TouchDeviceType {
    match input_device.get_feature_report().await {
        Ok(Ok(fidl_input_report::FeatureReport {
            touch:
                Some(fidl_input_report::TouchFeatureReport {
                    input_mode:
                        Some(
                            fidl_input_report::TouchConfigurationInputMode::MouseCollection
                            | fidl_input_report::TouchConfigurationInputMode::WindowsPrecisionTouchpadCollection,
                        ),
                    ..
                }),
            ..
        })) => TouchDeviceType::WindowsPrecisionTouchpad,
        _ => TouchDeviceType::TouchScreen,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing_utilities::{
        self, create_touch_contact, create_touch_input_report, create_touch_screen_event,
        create_touch_screen_event_with_buttons, create_touchpad_event,
    };
    use crate::utils::Position;
    use assert_matches::assert_matches;
    use diagnostics_assertions::AnyProperty;
    use fidl_test_util::spawn_stream_handler;
    use fuchsia_async as fasync;
    use futures::StreamExt;
    use pretty_assertions::assert_eq;
    use test_case::test_case;

    #[fasync::run_singlethreaded(test)]
    async fn process_empty_reports() {
        let previous_report_time = zx::MonotonicInstant::get().into_nanos();
        let previous_report = create_touch_input_report(
            vec![],
            /* pressed_buttons= */ None,
            previous_report_time,
        );
        let report_time = zx::MonotonicInstant::get().into_nanos();
        let report =
            create_touch_input_report(vec![], /* pressed_buttons= */ None, report_time);

        let descriptor =
            input_device::InputDeviceDescriptor::TouchScreen(TouchScreenDeviceDescriptor {
                device_id: 1,
                contacts: vec![],
            });
        let (mut event_sender, mut event_receiver) = futures::channel::mpsc::unbounded();

        let inspector = fuchsia_inspect::Inspector::default();
        let test_node = inspector.root().create_child("TestDevice_Touch");
        let mut inspect_status = InputDeviceStatus::new(test_node);
        inspect_status.health_node.set_ok();

        let (returned_report, _) = TouchBinding::process_reports(
            vec![report],
            Some(previous_report),
            &descriptor,
            &mut event_sender,
            &inspect_status,
            &metrics::MetricsLogger::default(),
            &input_device::InputPipelineFeatureFlags::default(),
        );
        assert!(returned_report.is_some());
        assert_eq!(returned_report.unwrap().event_time, Some(report_time));

        // Assert there are no pending events on the receiver.
        let event = event_receiver.try_next();
        assert!(event.is_err());

        diagnostics_assertions::assert_data_tree!(inspector, root: {
            "TestDevice_Touch": contains {
                reports_received_count: 1u64,
                reports_filtered_count: 1u64,
                events_generated: 0u64,
                last_received_timestamp_ns: report_time as u64,
                last_generated_timestamp_ns: 0u64,
                "fuchsia.inspect.Health": {
                    status: "OK",
                    // Timestamp value is unpredictable and not relevant in this context,
                    // so we only assert that the property is present.
                    start_timestamp_nanos: AnyProperty
                },
            }
        });
    }

    // Tests that a input report with a new contact generates an event with an add and a down.
    #[fasync::run_singlethreaded(test)]
    async fn add_and_down() {
        const TOUCH_ID: u32 = 2;

        let descriptor =
            input_device::InputDeviceDescriptor::TouchScreen(TouchScreenDeviceDescriptor {
                device_id: 1,
                contacts: vec![],
            });
        let (event_time_i64, event_time_u64) = testing_utilities::event_times();

        let contact = fidl_fuchsia_input_report::ContactInputReport {
            contact_id: Some(TOUCH_ID),
            position_x: Some(0),
            position_y: Some(0),
            pressure: None,
            contact_width: None,
            contact_height: None,
            ..Default::default()
        };
        let reports = vec![create_touch_input_report(
            vec![contact],
            /* pressed_buttons= */ None,
            event_time_i64,
        )];

        let expected_events = vec![create_touch_screen_event(
            hashmap! {
                fidl_ui_input::PointerEventPhase::Add
                    => vec![create_touch_contact(TOUCH_ID, Position { x: 0.0, y: 0.0 })],
                fidl_ui_input::PointerEventPhase::Down
                    => vec![create_touch_contact(TOUCH_ID, Position { x: 0.0, y: 0.0 })],
            },
            event_time_u64,
            &descriptor,
        )];

        assert_input_report_sequence_generates_events!(
            input_reports: reports,
            expected_events: expected_events,
            device_descriptor: descriptor,
            device_type: TouchBinding,
        );
    }

    // Tests that up and remove events are sent when a touch is released.
    #[fasync::run_singlethreaded(test)]
    async fn up_and_remove() {
        const TOUCH_ID: u32 = 2;

        let descriptor =
            input_device::InputDeviceDescriptor::TouchScreen(TouchScreenDeviceDescriptor {
                device_id: 1,
                contacts: vec![],
            });
        let (event_time_i64, event_time_u64) = testing_utilities::event_times();

        let contact = fidl_fuchsia_input_report::ContactInputReport {
            contact_id: Some(TOUCH_ID),
            position_x: Some(0),
            position_y: Some(0),
            pressure: None,
            contact_width: None,
            contact_height: None,
            ..Default::default()
        };
        let reports = vec![
            create_touch_input_report(
                vec![contact],
                /* pressed_buttons= */ None,
                event_time_i64,
            ),
            create_touch_input_report(vec![], /* pressed_buttons= */ None, event_time_i64),
        ];

        let expected_events = vec![
            create_touch_screen_event(
                hashmap! {
                    fidl_ui_input::PointerEventPhase::Add
                        => vec![create_touch_contact(TOUCH_ID, Position { x: 0.0, y: 0.0 })],
                    fidl_ui_input::PointerEventPhase::Down
                        => vec![create_touch_contact(TOUCH_ID, Position { x: 0.0, y: 0.0 })],
                },
                event_time_u64,
                &descriptor,
            ),
            create_touch_screen_event(
                hashmap! {
                    fidl_ui_input::PointerEventPhase::Up
                        => vec![create_touch_contact(TOUCH_ID, Position { x: 0.0, y: 0.0 })],
                    fidl_ui_input::PointerEventPhase::Remove
                        => vec![create_touch_contact(TOUCH_ID, Position { x: 0.0, y: 0.0 })],
                },
                event_time_u64,
                &descriptor,
            ),
        ];

        assert_input_report_sequence_generates_events!(
            input_reports: reports,
            expected_events: expected_events,
            device_descriptor: descriptor,
            device_type: TouchBinding,
        );
    }

    // Tests that a move generates the correct event.
    #[fasync::run_singlethreaded(test)]
    async fn add_down_move() {
        const TOUCH_ID: u32 = 2;
        let first = Position { x: 10.0, y: 30.0 };
        let second = Position { x: first.x * 2.0, y: first.y * 2.0 };

        let descriptor =
            input_device::InputDeviceDescriptor::TouchScreen(TouchScreenDeviceDescriptor {
                device_id: 1,
                contacts: vec![],
            });
        let (event_time_i64, event_time_u64) = testing_utilities::event_times();

        let first_contact = fidl_fuchsia_input_report::ContactInputReport {
            contact_id: Some(TOUCH_ID),
            position_x: Some(first.x as i64),
            position_y: Some(first.y as i64),
            pressure: None,
            contact_width: None,
            contact_height: None,
            ..Default::default()
        };
        let second_contact = fidl_fuchsia_input_report::ContactInputReport {
            contact_id: Some(TOUCH_ID),
            position_x: Some(first.x as i64 * 2),
            position_y: Some(first.y as i64 * 2),
            pressure: None,
            contact_width: None,
            contact_height: None,
            ..Default::default()
        };

        let reports = vec![
            create_touch_input_report(
                vec![first_contact],
                /* pressed_buttons= */ None,
                event_time_i64,
            ),
            create_touch_input_report(
                vec![second_contact],
                /* pressed_buttons= */ None,
                event_time_i64,
            ),
        ];

        let expected_events = vec![
            create_touch_screen_event(
                hashmap! {
                    fidl_ui_input::PointerEventPhase::Add
                        => vec![create_touch_contact(TOUCH_ID, first)],
                    fidl_ui_input::PointerEventPhase::Down
                        => vec![create_touch_contact(TOUCH_ID, first)],
                },
                event_time_u64,
                &descriptor,
            ),
            create_touch_screen_event(
                hashmap! {
                    fidl_ui_input::PointerEventPhase::Move
                        => vec![create_touch_contact(TOUCH_ID, second)],
                },
                event_time_u64,
                &descriptor,
            ),
        ];

        assert_input_report_sequence_generates_events!(
            input_reports: reports,
            expected_events: expected_events,
            device_descriptor: descriptor,
            device_type: TouchBinding,
        );
    }

    #[fasync::run_singlethreaded(test)]
    async fn sent_event_has_trace_id() {
        let previous_report_time = zx::MonotonicInstant::get().into_nanos();
        let previous_report = create_touch_input_report(
            vec![],
            /* pressed_buttons= */ None,
            previous_report_time,
        );

        let report_time = zx::MonotonicInstant::get().into_nanos();
        let contact = fidl_fuchsia_input_report::ContactInputReport {
            contact_id: Some(222),
            position_x: Some(333),
            position_y: Some(444),
            ..Default::default()
        };
        let report =
            create_touch_input_report(vec![contact], /* pressed_buttons= */ None, report_time);

        let descriptor =
            input_device::InputDeviceDescriptor::TouchScreen(TouchScreenDeviceDescriptor {
                device_id: 1,
                contacts: vec![],
            });
        let (mut event_sender, mut event_receiver) = futures::channel::mpsc::unbounded();

        let inspector = fuchsia_inspect::Inspector::default();
        let test_node = inspector.root().create_child("TestDevice_Touch");
        let mut inspect_status = InputDeviceStatus::new(test_node);
        inspect_status.health_node.set_ok();

        let _ = TouchBinding::process_reports(
            vec![report],
            Some(previous_report),
            &descriptor,
            &mut event_sender,
            &inspect_status,
            &metrics::MetricsLogger::default(),
            &input_device::InputPipelineFeatureFlags::default(),
        );
        assert_matches!(event_receiver.try_next(), Ok(Some(events)) if events.len() == 1 && events[0].trace_id.is_some());
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn enables_touchpad_mode_automatically() {
        let (set_feature_report_sender, set_feature_report_receiver) =
            futures::channel::mpsc::unbounded();
        let input_device_proxy = spawn_stream_handler(move |input_device_request| {
            let set_feature_report_sender = set_feature_report_sender.clone();
            async move {
                match input_device_request {
                    fidl_input_report::InputDeviceRequest::GetDescriptor { responder } => {
                        let _ = responder.send(&get_touchpad_device_descriptor(
                            true, /* has_mouse_descriptor */
                        ));
                    }
                    fidl_input_report::InputDeviceRequest::GetFeatureReport { responder } => {
                        let _ = responder.send(Ok(&fidl_input_report::FeatureReport {
                            touch: Some(fidl_input_report::TouchFeatureReport {
                                input_mode: Some(
                                    fidl_input_report::TouchConfigurationInputMode::MouseCollection,
                                ),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }));
                    }
                    fidl_input_report::InputDeviceRequest::SetFeatureReport {
                        responder,
                        report,
                    } => {
                        match set_feature_report_sender.unbounded_send(report) {
                            Ok(_) => {
                                let _ = responder.send(Ok(()));
                            }
                            Err(e) => {
                                panic!("try_send set_feature_report_request failed: {}", e);
                            }
                        };
                    }
                    fidl_input_report::InputDeviceRequest::GetInputReportsReader { .. } => {
                        // Do not panic as `initialize_report_stream()` will call this protocol.
                    }
                    r => panic!("unsupported request {:?}", r),
                }
            }
        });

        let (device_event_sender, _) = futures::channel::mpsc::unbounded();

        // Create a test inspect node as required by TouchBinding::new()
        let inspector = fuchsia_inspect::Inspector::default();
        let test_node = inspector.root().create_child("test_node");

        // Create a `TouchBinding` to exercise its call to `SetFeatureReport`. But drop
        // the binding immediately, so that `set_feature_report_receiver.collect()`
        // does not hang.
        TouchBinding::new(
            input_device_proxy,
            0,
            device_event_sender,
            test_node,
            input_device::InputPipelineFeatureFlags::default(),
            metrics::MetricsLogger::default(),
        )
        .await
        .unwrap();
        assert_matches!(
            set_feature_report_receiver.collect::<Vec<_>>().await.as_slice(),
            [fidl_input_report::FeatureReport {
                touch: Some(fidl_input_report::TouchFeatureReport {
                    input_mode: Some(
                        fidl_input_report::TouchConfigurationInputMode::WindowsPrecisionTouchpadCollection
                    ),
                    ..
                }),
                ..
            }]
        );
    }

    #[test_case(true, None, TouchDeviceType::TouchScreen; "touch screen")]
    #[test_case(false, None, TouchDeviceType::TouchScreen; "no mouse descriptor, no touch_input_mode")]
    #[test_case(true, Some(fidl_input_report::TouchConfigurationInputMode::MouseCollection), TouchDeviceType::WindowsPrecisionTouchpad; "touchpad in mouse mode")]
    #[test_case(true, Some(fidl_input_report::TouchConfigurationInputMode::WindowsPrecisionTouchpadCollection), TouchDeviceType::WindowsPrecisionTouchpad; "touchpad in touchpad mode")]
    #[fuchsia::test(allow_stalls = false)]
    async fn identifies_correct_touch_device_type(
        has_mouse_descriptor: bool,
        touch_input_mode: Option<fidl_input_report::TouchConfigurationInputMode>,
        expect_touch_device_type: TouchDeviceType,
    ) {
        let input_device_proxy = spawn_stream_handler(move |input_device_request| async move {
            match input_device_request {
                fidl_input_report::InputDeviceRequest::GetDescriptor { responder } => {
                    let _ = responder.send(&get_touchpad_device_descriptor(has_mouse_descriptor));
                }
                fidl_input_report::InputDeviceRequest::GetFeatureReport { responder } => {
                    let _ = responder.send(Ok(&fidl_input_report::FeatureReport {
                        touch: Some(fidl_input_report::TouchFeatureReport {
                            input_mode: touch_input_mode,
                            ..Default::default()
                        }),
                        ..Default::default()
                    }));
                }
                fidl_input_report::InputDeviceRequest::SetFeatureReport { responder, .. } => {
                    let _ = responder.send(Ok(()));
                }
                r => panic!("unsupported request {:?}", r),
            }
        });

        let (device_event_sender, _) = futures::channel::mpsc::unbounded();

        // Create a test inspect node as required by TouchBinding::new()
        let inspector = fuchsia_inspect::Inspector::default();
        let test_node = inspector.root().create_child("test_node");

        let binding = TouchBinding::new(
            input_device_proxy,
            0,
            device_event_sender,
            test_node,
            input_device::InputPipelineFeatureFlags::default(),
            metrics::MetricsLogger::default(),
        )
        .await
        .unwrap();
        pretty_assertions::assert_eq!(binding.touch_device_type, expect_touch_device_type);
    }

    /// Returns an |fidl_fuchsia_input_report::DeviceDescriptor| for
    /// touchpad related tests.
    fn get_touchpad_device_descriptor(
        has_mouse_descriptor: bool,
    ) -> fidl_fuchsia_input_report::DeviceDescriptor {
        fidl_input_report::DeviceDescriptor {
            mouse: match has_mouse_descriptor {
                true => Some(fidl_input_report::MouseDescriptor::default()),
                false => None,
            },
            touch: Some(fidl_input_report::TouchDescriptor {
                input: Some(fidl_input_report::TouchInputDescriptor {
                    contacts: Some(vec![fidl_input_report::ContactInputDescriptor {
                        position_x: Some(fidl_input_report::Axis {
                            range: fidl_input_report::Range { min: 1, max: 2 },
                            unit: fidl_input_report::Unit {
                                type_: fidl_input_report::UnitType::None,
                                exponent: 0,
                            },
                        }),
                        position_y: Some(fidl_input_report::Axis {
                            range: fidl_input_report::Range { min: 2, max: 3 },
                            unit: fidl_input_report::Unit {
                                type_: fidl_input_report::UnitType::Other,
                                exponent: 100000,
                            },
                        }),
                        pressure: Some(fidl_input_report::Axis {
                            range: fidl_input_report::Range { min: 3, max: 4 },
                            unit: fidl_input_report::Unit {
                                type_: fidl_input_report::UnitType::Grams,
                                exponent: -991,
                            },
                        }),
                        contact_width: Some(fidl_input_report::Axis {
                            range: fidl_input_report::Range { min: 5, max: 6 },
                            unit: fidl_input_report::Unit {
                                type_: fidl_input_report::UnitType::EnglishAngularVelocity,
                                exponent: 123,
                            },
                        }),
                        contact_height: Some(fidl_input_report::Axis {
                            range: fidl_input_report::Range { min: 7, max: 8 },
                            unit: fidl_input_report::Unit {
                                type_: fidl_input_report::UnitType::Pascals,
                                exponent: 100,
                            },
                        }),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[fasync::run_singlethreaded(test)]
    async fn send_touchpad_event_button() {
        const TOUCH_ID: u32 = 1;
        const PRIMARY_BUTTON: u8 = 1;

        let descriptor = input_device::InputDeviceDescriptor::Touchpad(TouchpadDeviceDescriptor {
            device_id: 1,
            contacts: vec![],
        });
        let (event_time_i64, event_time_u64) = testing_utilities::event_times();

        let contact = fidl_fuchsia_input_report::ContactInputReport {
            contact_id: Some(TOUCH_ID),
            position_x: Some(0),
            position_y: Some(0),
            pressure: None,
            contact_width: None,
            contact_height: None,
            ..Default::default()
        };
        let reports = vec![create_touch_input_report(
            vec![contact],
            Some(vec![fidl_fuchsia_input_report::TouchButton::__SourceBreaking {
                unknown_ordinal: PRIMARY_BUTTON,
            }]),
            event_time_i64,
        )];

        let expected_events = vec![create_touchpad_event(
            vec![create_touch_contact(TOUCH_ID, Position { x: 0.0, y: 0.0 })],
            vec![fidl_fuchsia_input_report::TouchButton::__SourceBreaking {
                unknown_ordinal: PRIMARY_BUTTON,
            }]
            .into_iter()
            .collect(),
            event_time_u64,
            &descriptor,
        )];

        assert_input_report_sequence_generates_events!(
            input_reports: reports,
            expected_events: expected_events,
            device_descriptor: descriptor,
            device_type: TouchBinding,
        );
    }

    #[fasync::run_singlethreaded(test)]
    async fn send_touchpad_event_2_fingers_down_up() {
        const TOUCH_ID_1: u32 = 1;
        const TOUCH_ID_2: u32 = 2;

        let descriptor = input_device::InputDeviceDescriptor::Touchpad(TouchpadDeviceDescriptor {
            device_id: 1,
            contacts: vec![],
        });
        let (event_time_i64, event_time_u64) = testing_utilities::event_times();

        let contact1 = fidl_fuchsia_input_report::ContactInputReport {
            contact_id: Some(TOUCH_ID_1),
            position_x: Some(0),
            position_y: Some(0),
            pressure: None,
            contact_width: None,
            contact_height: None,
            ..Default::default()
        };
        let contact2 = fidl_fuchsia_input_report::ContactInputReport {
            contact_id: Some(TOUCH_ID_2),
            position_x: Some(10),
            position_y: Some(10),
            pressure: None,
            contact_width: None,
            contact_height: None,
            ..Default::default()
        };
        let reports = vec![
            create_touch_input_report(
                vec![contact1, contact2],
                /* pressed_buttons= */ None,
                event_time_i64,
            ),
            create_touch_input_report(vec![], /* pressed_buttons= */ None, event_time_i64),
        ];

        let expected_events = vec![
            create_touchpad_event(
                vec![
                    create_touch_contact(TOUCH_ID_1, Position { x: 0.0, y: 0.0 }),
                    create_touch_contact(TOUCH_ID_2, Position { x: 10.0, y: 10.0 }),
                ],
                HashSet::new(),
                event_time_u64,
                &descriptor,
            ),
            create_touchpad_event(vec![], HashSet::new(), event_time_u64, &descriptor),
        ];

        assert_input_report_sequence_generates_events!(
            input_reports: reports,
            expected_events: expected_events,
            device_descriptor: descriptor,
            device_type: TouchBinding,
        );
    }

    #[test_case(Position{x: 0.0, y: 0.0}, Position{x: 5.0, y: 5.0}; "down move")]
    #[test_case(Position{x: 0.0, y: 0.0}, Position{x: 0.0, y: 0.0}; "down hold")]
    #[fasync::run_singlethreaded(test)]
    async fn send_touchpad_event_1_finger(p0: Position, p1: Position) {
        const TOUCH_ID: u32 = 1;

        let descriptor = input_device::InputDeviceDescriptor::Touchpad(TouchpadDeviceDescriptor {
            device_id: 1,
            contacts: vec![],
        });
        let (event_time_i64, event_time_u64) = testing_utilities::event_times();

        let contact1 = fidl_fuchsia_input_report::ContactInputReport {
            contact_id: Some(TOUCH_ID),
            position_x: Some(p0.x as i64),
            position_y: Some(p0.y as i64),
            pressure: None,
            contact_width: None,
            contact_height: None,
            ..Default::default()
        };
        let contact2 = fidl_fuchsia_input_report::ContactInputReport {
            contact_id: Some(TOUCH_ID),
            position_x: Some(p1.x as i64),
            position_y: Some(p1.y as i64),
            pressure: None,
            contact_width: None,
            contact_height: None,
            ..Default::default()
        };
        let reports = vec![
            create_touch_input_report(
                vec![contact1],
                /* pressed_buttons= */ None,
                event_time_i64,
            ),
            create_touch_input_report(
                vec![contact2],
                /* pressed_buttons= */ None,
                event_time_i64,
            ),
        ];

        let expected_events = vec![
            create_touchpad_event(
                vec![create_touch_contact(TOUCH_ID, p0)],
                HashSet::new(),
                event_time_u64,
                &descriptor,
            ),
            create_touchpad_event(
                vec![create_touch_contact(TOUCH_ID, p1)],
                HashSet::new(),
                event_time_u64,
                &descriptor,
            ),
        ];

        assert_input_report_sequence_generates_events!(
            input_reports: reports,
            expected_events: expected_events,
            device_descriptor: descriptor,
            device_type: TouchBinding,
        );
    }

    // Tests that a pressed button with no contacts generates an event with the
    // button.
    #[test_case(true; "merge touch events enabled")]
    #[test_case(false; "merge touch events disabled")]
    #[fasync::run_singlethreaded(test)]
    async fn send_pressed_button_no_contact(enable_merge_touch_events: bool) {
        let descriptor =
            input_device::InputDeviceDescriptor::TouchScreen(TouchScreenDeviceDescriptor {
                device_id: 1,
                contacts: vec![],
            });
        let (event_time_i64, event_time_u64) = testing_utilities::event_times();

        let reports = vec![create_touch_input_report(
            vec![],
            Some(vec![fidl_fuchsia_input_report::TouchButton::Palm]),
            event_time_i64,
        )];

        let expected_events = vec![create_touch_screen_event_with_buttons(
            hashmap! {},
            vec![fidl_fuchsia_input_report::TouchButton::Palm],
            event_time_u64,
            &descriptor,
        )];

        assert_input_report_sequence_generates_events_with_feature_flags!(
            input_reports: reports,
            expected_events: expected_events,
            device_descriptor: descriptor,
            device_type: TouchBinding,
            feature_flags: input_device::InputPipelineFeatureFlags {
                enable_merge_touch_events,
                ..Default::default()
            },
        );
    }

    // Tests that a pressed button with a contact generates an event with
    // contact and button.
    #[test_case(true; "merge touch events enabled")]
    #[test_case(false; "merge touch events disabled")]
    #[fasync::run_singlethreaded(test)]
    async fn send_pressed_button_with_contact(enable_merge_touch_events: bool) {
        const TOUCH_ID: u32 = 2;

        let descriptor =
            input_device::InputDeviceDescriptor::TouchScreen(TouchScreenDeviceDescriptor {
                device_id: 1,
                contacts: vec![],
            });
        let (event_time_i64, event_time_u64) = testing_utilities::event_times();

        let contact = fidl_fuchsia_input_report::ContactInputReport {
            contact_id: Some(TOUCH_ID),
            position_x: Some(0),
            position_y: Some(0),
            pressure: None,
            contact_width: None,
            contact_height: None,
            ..Default::default()
        };
        let reports = vec![create_touch_input_report(
            vec![contact],
            Some(vec![fidl_fuchsia_input_report::TouchButton::Palm]),
            event_time_i64,
        )];

        let expected_events = vec![create_touch_screen_event_with_buttons(
            hashmap! {
                fidl_ui_input::PointerEventPhase::Add
                    => vec![create_touch_contact(TOUCH_ID, Position { x: 0.0, y: 0.0 })],
                fidl_ui_input::PointerEventPhase::Down
                    => vec![create_touch_contact(TOUCH_ID, Position { x: 0.0, y: 0.0 })],
            },
            vec![fidl_fuchsia_input_report::TouchButton::Palm],
            event_time_u64,
            &descriptor,
        )];

        assert_input_report_sequence_generates_events_with_feature_flags!(
            input_reports: reports,
            expected_events: expected_events,
            device_descriptor: descriptor,
            device_type: TouchBinding,
            feature_flags: input_device::InputPipelineFeatureFlags {
                enable_merge_touch_events,
                ..Default::default()
            },
        );
    }

    // Tests that multiple pressed buttons with contacts generates an event
    // with contact and buttons.
    #[test_case(true; "merge touch events enabled")]
    #[test_case(false; "merge touch events disabled")]
    #[fasync::run_singlethreaded(test)]
    async fn send_multiple_pressed_buttons_with_contact(enable_merge_touch_events: bool) {
        const TOUCH_ID: u32 = 2;

        let descriptor =
            input_device::InputDeviceDescriptor::TouchScreen(TouchScreenDeviceDescriptor {
                device_id: 1,
                contacts: vec![],
            });
        let (event_time_i64, event_time_u64) = testing_utilities::event_times();

        let contact = fidl_fuchsia_input_report::ContactInputReport {
            contact_id: Some(TOUCH_ID),
            position_x: Some(0),
            position_y: Some(0),
            pressure: None,
            contact_width: None,
            contact_height: None,
            ..Default::default()
        };
        let reports = vec![create_touch_input_report(
            vec![contact],
            Some(vec![
                fidl_fuchsia_input_report::TouchButton::Palm,
                fidl_fuchsia_input_report::TouchButton::__SourceBreaking { unknown_ordinal: 2 },
            ]),
            event_time_i64,
        )];

        let expected_events = vec![create_touch_screen_event_with_buttons(
            hashmap! {
                fidl_ui_input::PointerEventPhase::Add
                    => vec![create_touch_contact(TOUCH_ID, Position { x: 0.0, y: 0.0 })],
                fidl_ui_input::PointerEventPhase::Down
                    => vec![create_touch_contact(TOUCH_ID, Position { x: 0.0, y: 0.0 })],
            },
            vec![
                fidl_fuchsia_input_report::TouchButton::Palm,
                fidl_fuchsia_input_report::TouchButton::__SourceBreaking { unknown_ordinal: 2 },
            ],
            event_time_u64,
            &descriptor,
        )];

        assert_input_report_sequence_generates_events_with_feature_flags!(
            input_reports: reports,
            expected_events: expected_events,
            device_descriptor: descriptor,
            device_type: TouchBinding,
            feature_flags: input_device::InputPipelineFeatureFlags {
                enable_merge_touch_events,
                ..Default::default()
            },
        );
    }

    // Tests that no buttons and no contacts generates no events.
    #[test_case(true; "merge touch events enabled")]
    #[test_case(false; "merge touch events disabled")]
    #[fasync::run_singlethreaded(test)]
    async fn send_no_buttons_no_contacts(enable_merge_touch_events: bool) {
        let descriptor =
            input_device::InputDeviceDescriptor::TouchScreen(TouchScreenDeviceDescriptor {
                device_id: 1,
                contacts: vec![],
            });
        let (event_time_i64, _) = testing_utilities::event_times();

        let reports = vec![create_touch_input_report(vec![], Some(vec![]), event_time_i64)];

        let expected_events: Vec<input_device::InputEvent> = vec![];

        assert_input_report_sequence_generates_events_with_feature_flags!(
            input_reports: reports,
            expected_events: expected_events,
            device_descriptor: descriptor,
            device_type: TouchBinding,
            feature_flags: input_device::InputPipelineFeatureFlags {
                enable_merge_touch_events,
                ..Default::default()
            },
        );
    }

    // Tests a buttons event after a contact event does not remove contacts.
    #[test_case(true; "merge touch events enabled")]
    #[test_case(false; "merge touch events disabled")]
    #[fasync::run_singlethreaded(test)]
    async fn send_button_does_not_remove_contacts(enable_merge_touch_events: bool) {
        const TOUCH_ID: u32 = 2;

        let descriptor =
            input_device::InputDeviceDescriptor::TouchScreen(TouchScreenDeviceDescriptor {
                device_id: 1,
                contacts: vec![],
            });
        let (event_time_i64, event_time_u64) = testing_utilities::event_times();

        let contact = fidl_fuchsia_input_report::ContactInputReport {
            contact_id: Some(TOUCH_ID),
            position_x: Some(0),
            position_y: Some(0),
            pressure: None,
            contact_width: None,
            contact_height: None,
            ..Default::default()
        };
        let reports = vec![
            create_touch_input_report(vec![contact], None, event_time_i64),
            create_touch_input_report(
                vec![],
                Some(vec![fidl_fuchsia_input_report::TouchButton::Palm]),
                event_time_i64,
            ),
            create_touch_input_report(vec![], Some(vec![]), event_time_i64),
        ];

        let expected_events = vec![
            create_touch_screen_event_with_buttons(
                hashmap! {
                    fidl_ui_input::PointerEventPhase::Add
                        => vec![create_touch_contact(TOUCH_ID, Position { x: 0.0, y: 0.0 })],
                    fidl_ui_input::PointerEventPhase::Down
                        => vec![create_touch_contact(TOUCH_ID, Position { x: 0.0, y: 0.0 })],
                },
                vec![],
                event_time_u64,
                &descriptor,
            ),
            create_touch_screen_event_with_buttons(
                hashmap! {},
                vec![fidl_fuchsia_input_report::TouchButton::Palm],
                event_time_u64,
                &descriptor,
            ),
            create_touch_screen_event_with_buttons(
                hashmap! {},
                vec![],
                event_time_u64,
                &descriptor,
            ),
        ];

        assert_input_report_sequence_generates_events_with_feature_flags!(
            input_reports: reports,
            expected_events: expected_events,
            device_descriptor: descriptor,
            device_type: TouchBinding,
            feature_flags: input_device::InputPipelineFeatureFlags {
                enable_merge_touch_events,
                ..Default::default()
            },
        );
    }

    #[fasync::run_singlethreaded(test)]
    async fn process_reports_batches_events() {
        const TOUCH_ID: u32 = 2;

        let descriptor =
            input_device::InputDeviceDescriptor::TouchScreen(TouchScreenDeviceDescriptor {
                device_id: 1,
                contacts: vec![],
            });
        let (event_time_i64, _) = testing_utilities::event_times();

        let contact1 = fidl_fuchsia_input_report::ContactInputReport {
            contact_id: Some(TOUCH_ID),
            position_x: Some(0),
            position_y: Some(0),
            ..Default::default()
        };
        let contact2 = fidl_fuchsia_input_report::ContactInputReport {
            contact_id: Some(TOUCH_ID),
            position_x: Some(10),
            position_y: Some(10),
            ..Default::default()
        };
        let reports = vec![
            create_touch_input_report(vec![contact1], None, event_time_i64),
            create_touch_input_report(vec![contact2], None, event_time_i64),
        ];

        let (mut event_sender, mut event_receiver) = futures::channel::mpsc::unbounded();

        let inspector = fuchsia_inspect::Inspector::default();
        let test_node = inspector.root().create_child("TestDevice_Touch");
        let mut inspect_status = InputDeviceStatus::new(test_node);
        inspect_status.health_node.set_ok();

        let _ = TouchBinding::process_reports(
            reports,
            None,
            &descriptor,
            &mut event_sender,
            &inspect_status,
            &metrics::MetricsLogger::default(),
            &input_device::InputPipelineFeatureFlags::default(),
        );

        // Expect EXACTLY one batch containing two events.
        let batch = event_receiver.try_next().expect("Expected a batch of events");
        let events = batch.expect("Expected events in the batch");
        assert_eq!(events.len(), 2);

        // Verify no more batches.
        assert!(event_receiver.try_next().is_err());
    }

    #[fasync::run_singlethreaded(test)]
    async fn process_reports_merges_touch_events_when_enabled() {
        const TOUCH_ID: u32 = 2;
        let descriptor =
            input_device::InputDeviceDescriptor::TouchScreen(TouchScreenDeviceDescriptor {
                device_id: 1,
                contacts: vec![],
            });
        let (event_time_i64, _) = testing_utilities::event_times();

        let contact_add = fidl_fuchsia_input_report::ContactInputReport {
            contact_id: Some(TOUCH_ID),
            position_x: Some(0),
            position_y: Some(0),
            ..Default::default()
        };
        let contact_move1 = fidl_fuchsia_input_report::ContactInputReport {
            contact_id: Some(TOUCH_ID),
            position_x: Some(10),
            position_y: Some(10),
            ..Default::default()
        };
        let contact_move2 = fidl_fuchsia_input_report::ContactInputReport {
            contact_id: Some(TOUCH_ID),
            position_x: Some(20),
            position_y: Some(20),
            ..Default::default()
        };
        let contact_move3 = fidl_fuchsia_input_report::ContactInputReport {
            contact_id: Some(TOUCH_ID),
            position_x: Some(30),
            position_y: Some(30),
            ..Default::default()
        };
        let reports = vec![
            create_touch_input_report(vec![contact_add], None, event_time_i64),
            create_touch_input_report(vec![contact_move1], None, event_time_i64),
            create_touch_input_report(vec![contact_move2], None, event_time_i64),
            create_touch_input_report(vec![contact_move3], None, event_time_i64),
            create_touch_input_report(vec![], None, event_time_i64),
        ];

        let (mut event_sender, mut event_receiver) = futures::channel::mpsc::unbounded();
        let inspector = fuchsia_inspect::Inspector::default();
        let mut inspect_status =
            InputDeviceStatus::new(inspector.root().create_child("TestDevice_Touch"));
        inspect_status.health_node.set_ok();

        let _ = TouchBinding::process_reports(
            reports,
            None,
            &descriptor,
            &mut event_sender,
            &inspect_status,
            &metrics::MetricsLogger::default(),
            &input_device::InputPipelineFeatureFlags {
                enable_merge_touch_events: true,
                ..Default::default()
            },
        );

        let batch = event_receiver.try_next().unwrap().unwrap();

        // Expected events: Add, Move(30), Remove.
        assert_eq!(batch.len(), 3);

        // Verify Add event
        assert_matches!(
            &batch[0].device_event,
            input_device::InputDeviceEvent::TouchScreen(event)
                if event.injector_contacts.get(&pointerinjector::EventPhase::Add).is_some()
        );
        // Verify Move event (merged to the last one)
        assert_matches!(
            &batch[1].device_event,
            input_device::InputDeviceEvent::TouchScreen(event)
                if event.injector_contacts.get(&pointerinjector::EventPhase::Change).map(|c| c[0].position.x) == Some(30.0)
        );
        // Verify Remove event
        assert_matches!(
            &batch[2].device_event,
            input_device::InputDeviceEvent::TouchScreen(event)
                if event.injector_contacts.get(&pointerinjector::EventPhase::Remove).is_some()
        );
    }

    #[fasync::run_singlethreaded(test)]
    async fn process_reports_does_not_merge_touch_events_when_disabled() {
        const TOUCH_ID: u32 = 2;
        let descriptor =
            input_device::InputDeviceDescriptor::TouchScreen(TouchScreenDeviceDescriptor {
                device_id: 1,
                contacts: vec![],
            });
        let (event_time_i64, _) = testing_utilities::event_times();

        let contact_add = fidl_fuchsia_input_report::ContactInputReport {
            contact_id: Some(TOUCH_ID),
            position_x: Some(0),
            position_y: Some(0),
            ..Default::default()
        };
        let contact_move1 = fidl_fuchsia_input_report::ContactInputReport {
            contact_id: Some(TOUCH_ID),
            position_x: Some(10),
            position_y: Some(10),
            ..Default::default()
        };
        let contact_move2 = fidl_fuchsia_input_report::ContactInputReport {
            contact_id: Some(TOUCH_ID),
            position_x: Some(20),
            position_y: Some(20),
            ..Default::default()
        };
        let contact_move3 = fidl_fuchsia_input_report::ContactInputReport {
            contact_id: Some(TOUCH_ID),
            position_x: Some(30),
            position_y: Some(30),
            ..Default::default()
        };
        let reports = vec![
            create_touch_input_report(vec![contact_add], None, event_time_i64),
            create_touch_input_report(vec![contact_move1], None, event_time_i64),
            create_touch_input_report(vec![contact_move2], None, event_time_i64),
            create_touch_input_report(vec![contact_move3], None, event_time_i64),
            create_touch_input_report(vec![], None, event_time_i64),
        ];

        let (mut event_sender, mut event_receiver) = futures::channel::mpsc::unbounded();
        let inspector = fuchsia_inspect::Inspector::default();
        let mut inspect_status =
            InputDeviceStatus::new(inspector.root().create_child("TestDevice_Touch"));
        inspect_status.health_node.set_ok();

        let _ = TouchBinding::process_reports(
            reports,
            None,
            &descriptor,
            &mut event_sender,
            &inspect_status,
            &metrics::MetricsLogger::default(),
            &input_device::InputPipelineFeatureFlags {
                enable_merge_touch_events: false,
                ..Default::default()
            },
        );

        let batch = event_receiver.try_next().unwrap().unwrap();

        // Expected events: Add, Move(10), Move(20), Move(30), Remove.
        assert_eq!(batch.len(), 5);
    }
}
