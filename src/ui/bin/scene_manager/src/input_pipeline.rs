// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::lib::factory_reset_handler::FactoryResetHandler;
use crate::lib::ime_handler::ImeHandler;
use crate::lib::input_device::InputPipelineFeatureFlags;
use crate::lib::input_pipeline::{InputDeviceBindingMap, InputPipeline, InputPipelineAssembly};
use crate::lib::light_sensor::{
    Calibration as LightSensorCalibration, Configuration as LightSensorConfiguration,
    FactoryFileLoader,
};
use crate::lib::light_sensor_handler::{
    CalibratedLightSensorHandler, make_light_sensor_handler_and_spawn_led_watcher,
};
use crate::lib::media_buttons_handler::MediaButtonsHandler;
use crate::lib::modifier_handler::{ModifierHandler, ModifierMeaningHandler};
use crate::lib::mouse_injector_handler::MouseInjectorHandler;

use crate::lib::touch_injector_handler::TouchInjectorHandler;
use crate::lib::{CursorMessage, Dispatcher, Incoming, input_device, keymap_handler, metrics};
use crate::scene_management::SceneManagerTrait;
use anyhow::{Context, Error};
use fidl_fuchsia_factory::MiscFactoryStoreProviderProxy;
use fidl_fuchsia_input_injection::InputDeviceRegistryRequestStream;
use fidl_fuchsia_lightsensor::SensorRequestStream as LightSensorRequestStream;
use fidl_fuchsia_recovery_policy::DeviceRequestStream;
use fidl_fuchsia_recovery_ui::FactoryResetCountdownRequestStream;
use fidl_fuchsia_settings as fsettings;
use fidl_fuchsia_ui_brightness::ControlProxy as BrightnessControlProxy;
use fidl_fuchsia_ui_pointerinjector_configuration::SetupProxy;
use fidl_fuchsia_ui_policy::{DeviceListenerRegistryRequest, DeviceListenerRegistryRequestStream};
use focus_chain_provider::FocusChainProviderPublisher;
use fsettings::LightProxy;
use fuchsia_async as fasync;
use fuchsia_inspect as inspect;
use futures::{StreamExt, TryStreamExt};
use log::{error, info, warn};
use sorted_vec_map::SortedVecSet;
use std::rc::Rc;

/// Begins handling input events. The returned future will complete when
/// input events are no longer being handled.
///
/// # Parameters
/// - `scene_manager`: The scene manager used by the session.
/// - `input_device_registry_request_stream_receiver`: A receiving end of a MPSC channel for
///   `InputDeviceRegistry` messages.
/// - `light_sensor_request_stream_receiver`: A receiving end of an MPSC channel for
///   `Sensor` messages.
/// - `node`: The inspect node to insert individual inspect handler nodes into.
/// - `focus_chain_publisher`: Forwards focus chain changes to downstream watchers.
/// - `light_sensor_configuration`: An optional configuration used for light sensor requests.
pub async fn handle_input(
    incoming: &Incoming,
    scene_manager: Rc<dyn SceneManagerTrait>,
    input_device_registry_request_stream_receiver: futures::channel::mpsc::UnboundedReceiver<
        InputDeviceRegistryRequestStream,
    >,
    light_sensor_request_stream_receiver: Option<
        futures::channel::mpsc::UnboundedReceiver<LightSensorRequestStream>,
    >,
    media_buttons_listener_registry_request_stream_receiver: futures::channel::mpsc::UnboundedReceiver<
        DeviceListenerRegistryRequestStream,
    >,
    factory_reset_countdown_request_stream_receiver: futures::channel::mpsc::UnboundedReceiver<
        FactoryResetCountdownRequestStream,
    >,
    factory_reset_device_request_stream_receiver: futures::channel::mpsc::UnboundedReceiver<
        DeviceRequestStream,
    >,
    node: inspect::Node,
    display_ownership_event: zx::Event,
    focus_chain_publisher: FocusChainProviderPublisher,
    supported_input_devices: Vec<String>,
    light_sensor_configuration: Option<LightSensorConfiguration>,
    enable_merge_touch_events: bool,
) -> Result<InputPipeline, Error> {
    let input_handlers_node = node.create_child("input_handlers");
    let metrics_logger = metrics::MetricsLogger::new(incoming);

    let factory_reset_handler =
        FactoryResetHandler::new(incoming.clone(), &input_handlers_node, metrics_logger.clone());
    let media_buttons_handler =
        MediaButtonsHandler::new(&input_handlers_node, metrics_logger.clone());
    let touch_injector_handler = create_touchscreen_handler(
        incoming,
        scene_manager.clone(),
        &input_handlers_node,
        metrics_logger.clone(),
    )
    .await?;

    let supported_input_devices =
        input_device::InputDeviceType::list_from_structured_config_list(&supported_input_devices);

    let light_sensor_handler = if let Some(light_sensor_configuration) = light_sensor_configuration
    {
        if supported_input_devices.contains(&input_device::InputDeviceType::LightSensor) {
            let light_proxy = incoming
                .connect_protocol::<LightProxy>()
                .context("unable to connnect to light proxy for light sensor")?;
            let brightness_proxy = incoming
                .connect_protocol::<BrightnessControlProxy>()
                .context("unable to connnect to brightness control proxy for light sensor")?;
            let factory_store_proxy = incoming
                .connect_protocol::<MiscFactoryStoreProviderProxy>()
                .context("unable to connect to factory proxy for light sensor")?;
            let factory_file_loader = FactoryFileLoader::new(factory_store_proxy)
                .context("unable to connect to factory file loader for light sensor")?;
            let calibration = if let Some(configuration) = light_sensor_configuration.calibration {
                LightSensorCalibration::new(configuration, &factory_file_loader)
                    .await
                    .map_err(|e| {
                        warn!(
                            "Calculations will use uncalibrated data. No light sensor \
                               calibration: {e:?}"
                        )
                    })
                    .ok()
            } else {
                info!(
                    "Calculations will use uncalibrated data. No light sensor \
                           calibration: Configuration not supplied"
                );
                None
            };
            let (handler, task) = make_light_sensor_handler_and_spawn_led_watcher(
                light_proxy,
                brightness_proxy,
                calibration,
                light_sensor_configuration.sensor,
                &input_handlers_node,
            )
            .await
            .context("unable to create light sensor handler")?;
            if let Some(task) = task {
                task.detach();
            }
            Some(handler)
        } else {
            None
        }
    } else {
        None
    };

    // Create parent node of inspect nodes for device bindings.
    let injected_devices_node = node.create_child("injected_input_devices");

    let input_pipeline = InputPipeline::new(
        incoming,
        supported_input_devices.clone(),
        build_input_pipeline_assembly(
            incoming,
            scene_manager,
            &node,
            display_ownership_event,
            factory_reset_handler.clone(),
            media_buttons_handler.clone(),
            light_sensor_handler.clone(),
            touch_injector_handler.clone(),
            SortedVecSet::from_iter(supported_input_devices.iter()),
            focus_chain_publisher,
            input_handlers_node,
            metrics_logger.clone(),
        )
        .await,
        node,
        InputPipelineFeatureFlags { enable_merge_touch_events },
        metrics_logger.clone(),
    )
    .context("Failed to create InputPipeline.")?;

    if let (Some(light_sensor_handler), Some(light_sensor_request_stream_receiver)) =
        (light_sensor_handler, light_sensor_request_stream_receiver)
    {
        let light_sensor_fut = handle_light_sensor_request_stream(
            light_sensor_request_stream_receiver,
            light_sensor_handler,
        );
        fasync::Task::local(light_sensor_fut).detach();
    }

    let input_device_registry_fut = handle_input_device_registry_request_streams(
        input_device_registry_request_stream_receiver,
        input_pipeline.input_device_types().clone(),
        input_pipeline.input_event_sender().clone(),
        input_pipeline.input_device_bindings().clone(),
        injected_devices_node,
        input_pipeline.feature_flags.clone(),
        metrics_logger.clone(),
    );
    fasync::Task::local(input_device_registry_fut).detach();

    let factory_reset_countdown_fut = handle_factory_reset_countdown_request_stream(
        factory_reset_countdown_request_stream_receiver,
        factory_reset_handler.clone(),
    );
    fasync::Task::local(factory_reset_countdown_fut).detach();

    let factory_reset_device_device_fut = handle_recovery_policy_device_request_stream(
        factory_reset_device_request_stream_receiver,
        factory_reset_handler.clone(),
    );
    fasync::Task::local(factory_reset_device_device_fut).detach();

    let media_buttons_listener_registry_fut = handle_device_listener_registry_request_stream(
        media_buttons_listener_registry_request_stream_receiver,
        media_buttons_handler.clone(),
        touch_injector_handler.clone(),
    );
    fasync::Task::local(media_buttons_listener_registry_fut).detach();

    Ok(input_pipeline)
}

fn setup_pointer_injector_config_request_stream(
    scene_manager: Rc<dyn SceneManagerTrait>,
) -> SetupProxy {
    let (setup_proxy, setup_request_stream) = fidl::endpoints::create_proxy_and_stream::<
        fidl_fuchsia_ui_pointerinjector_configuration::SetupMarker,
    >();

    crate::scene_management::handle_pointer_injector_configuration_setup_request_stream(
        setup_request_stream,
        scene_manager,
    );

    setup_proxy
}

async fn create_touchscreen_handler(
    incoming: &Incoming,
    scene_manager: Rc<dyn SceneManagerTrait>,
    input_handlers_node: &inspect::Node,
    metrics_logger: metrics::MetricsLogger,
) -> Result<Rc<TouchInjectorHandler>, Error> {
    let setup_proxy = setup_pointer_injector_config_request_stream(scene_manager.clone());
    let size = scene_manager.get_pointerinjection_display_size();
    let touch_handler = TouchInjectorHandler::new_with_config_proxy(
        incoming,
        setup_proxy,
        size,
        input_handlers_node,
        metrics_logger,
    )
    .await;
    match touch_handler {
        Ok(touch_handler) => {
            fasync::Task::local(touch_handler.clone().watch_viewport()).detach();
            Ok(touch_handler)
        }
        Err(e) => {
            error!("Touch injector handler was not created: {:?}", e);
            Err(e)
        }
    }
}

async fn add_mouse_handler(
    incoming: &Incoming,
    scene_manager: Rc<dyn SceneManagerTrait>,
    mut assembly: InputPipelineAssembly,
    sender: futures::channel::mpsc::Sender<CursorMessage>,
    input_handlers_node: &inspect::Node,
    metrics_logger: metrics::MetricsLogger,
) -> InputPipelineAssembly {
    let setup_proxy = setup_pointer_injector_config_request_stream(scene_manager.clone());
    let size = scene_manager.get_pointerinjection_display_size();
    let mouse_handler = MouseInjectorHandler::new_with_config_proxy(
        incoming,
        setup_proxy,
        size,
        sender,
        input_handlers_node,
        metrics_logger,
    )
    .await;
    match mouse_handler {
        Ok(mouse_handler) => {
            fasync::Task::local(mouse_handler.clone().watch_viewport()).detach();
            assembly = assembly.add_handler(mouse_handler);
        }
        Err(e) => error!(
            "build_input_pipeline_assembly(): Mouse injector handler was not installed: {:?}",
            e
        ),
    };
    assembly
}

/// Registers the keyboard handlers that deal with keyboard.
async fn register_keyboard_related_input_handlers(
    incoming: &Incoming,
    assembly: InputPipelineAssembly,
    display_ownership_event: zx::Event,
    focus_chain_publisher: FocusChainProviderPublisher,
    input_handlers_node: &inspect::Node,
    metrics_logger: metrics::MetricsLogger,
) -> InputPipelineAssembly {
    let mut assembly = assembly;

    // Display ownership deals with keyboard events.
    assembly = assembly.add_display_ownership(display_ownership_event, input_handlers_node);
    assembly = add_modifier_handler(assembly, input_handlers_node, metrics_logger.clone());

    assembly = add_keymap_handler(assembly, input_handlers_node, metrics_logger.clone());
    assembly =
        add_key_meaning_modifier_handler(assembly, input_handlers_node, metrics_logger.clone());

    // ime_handler is the last handler for key event handling, it sends out key events to
    // listeners. Please double check tracing events, when changing the handlers assembly order.
    assembly = add_ime(incoming, assembly, input_handlers_node, metrics_logger.clone()).await;

    // Forward focus to Text Manager.
    // This requires `fuchsia.ui.focus.FocusChainListenerRegistry`
    assembly = assembly.add_focus_listener(incoming, focus_chain_publisher);
    assembly
}

// TODO(b/512079275): also remove this helper.
/// Installs the handlers for mouse input.
async fn register_mouse_related_input_handlers(
    incoming: &Incoming,
    assembly: InputPipelineAssembly,
    scene_manager: Rc<dyn SceneManagerTrait>,
    input_handlers_node: &inspect::Node,
    metrics_logger: metrics::MetricsLogger,
) -> InputPipelineAssembly {
    let (sender, mut receiver) = futures::channel::mpsc::channel(0);

    // mouse injector handler is the last handler for mouse event handling, it sends out mouse
    // events to scenic. Please double check tracing events, when changing the handlers assembly
    // order.
    let assembly = add_mouse_handler(
        incoming,
        scene_manager.clone(),
        assembly,
        sender,
        input_handlers_node,
        metrics_logger,
    )
    .await;

    let scene_manager = scene_manager.clone();
    fasync::Task::local(async move {
        while let Some(message) = receiver.next().await {
            match message {
                CursorMessage::SetPosition(position) => scene_manager.set_cursor_position(position),
                CursorMessage::SetVisibility(visible) => {
                    scene_manager.set_cursor_visibility(visible)
                }
            }
        }
    })
    .detach();
    assembly
}

async fn build_input_pipeline_assembly(
    incoming: &Incoming,
    scene_manager: Rc<dyn SceneManagerTrait>,
    node: &inspect::Node,
    display_ownership_event: zx::Event,
    factory_reset_handler: Rc<FactoryResetHandler>,
    media_buttons_handler: Rc<MediaButtonsHandler>,
    light_sensor_handler: Option<Rc<CalibratedLightSensorHandler>>,
    touch_injector_handler: Rc<TouchInjectorHandler>,
    supported_input_devices: SortedVecSet<&input_device::InputDeviceType>,
    focus_chain_publisher: FocusChainProviderPublisher,
    input_handlers_node: inspect::Node,
    metrics_logger: metrics::MetricsLogger,
) -> InputPipelineAssembly {
    let mut assembly = InputPipelineAssembly::new(metrics_logger.clone());
    {
        // Keep this handler first because it keeps performance measurement counters
        // for the rest of the pipeline at entry.
        assembly = add_inspect_handler(
            node.create_child("input_pipeline_entry"),
            assembly,
            &supported_input_devices,
            /* displays_recent_events = */ true,
        );

        if supported_input_devices.contains(&input_device::InputDeviceType::Keyboard) {
            info!("Registering keyboard-related input handlers.");
            assembly = register_keyboard_related_input_handlers(
                incoming,
                assembly,
                display_ownership_event,
                focus_chain_publisher,
                &input_handlers_node,
                metrics_logger.clone(),
            )
            .await;
        }

        if supported_input_devices.contains(&input_device::InputDeviceType::ConsumerControls) {
            info!("Registering consumer controls-related input handlers.");
            // Add factory reset handler before media buttons handler.
            assembly = assembly.add_handler(factory_reset_handler);

            // media_buttons_handler is the last handler for media button handling, it sends out
            // button events to listeners. Please double check tracing events, when changing the
            // handlers assembly order.
            assembly = assembly.add_handler(media_buttons_handler);
        }

        if supported_input_devices.contains(&input_device::InputDeviceType::LightSensor) {
            if let Some(light_sensor_handler) = light_sensor_handler {
                info!("Registering light sensor-related input handlers.");
                assembly = assembly.add_handler(light_sensor_handler);
            }
        }

        if supported_input_devices.contains(&input_device::InputDeviceType::Mouse) {
            info!("Registering mouse-related input handlers.");
            assembly = register_mouse_related_input_handlers(
                incoming,
                assembly,
                scene_manager.clone(),
                &input_handlers_node,
                metrics_logger.clone(),
            )
            .await;
        }

        if supported_input_devices.contains(&input_device::InputDeviceType::Touch) {
            info!("Registering touchscreen-related input handlers.");
            // TouchInjectorHandler is the last handler for touch event handling. It sends touch
            // pointer events to Scenic and touch button events to registered listeners. Please
            // double check tracing events when changing the handler's assembly order.
            assembly = assembly.add_handler(touch_injector_handler);
        }
    }

    // Keep this handler last because it keeps performance measurement counters
    // for the rest of the pipeline at exit.  We compare these values to the
    // values at entry.
    assembly = add_inspect_handler(
        node.create_child("input_pipeline_exit"),
        assembly,
        &supported_input_devices,
        /* displays_recent_events = */ false,
    );

    // Record input_handlers_node to it's parent node so that it does not get dropped
    // from the Inspect tree when we exit this scope.
    node.record(input_handlers_node);

    assembly
}

/// Hooks up the modifier keys handler.
fn add_modifier_handler(
    assembly: InputPipelineAssembly,
    input_handlers_node: &inspect::Node,
    metrics_logger: metrics::MetricsLogger,
) -> InputPipelineAssembly {
    assembly.add_handler(ModifierHandler::new(input_handlers_node, metrics_logger))
}

/// Hooks up the modifier keys handler based on key meanings.  This must come
/// after the keymap handler.
fn add_key_meaning_modifier_handler(
    assembly: InputPipelineAssembly,
    input_handlers_node: &inspect::Node,
    metrics_logger: metrics::MetricsLogger,
) -> InputPipelineAssembly {
    assembly.add_handler(ModifierMeaningHandler::new(input_handlers_node, metrics_logger))
}

/// Hooks up the inspect handler.
fn add_inspect_handler(
    node: inspect::Node,
    assembly: InputPipelineAssembly,
    supported_input_devices: &SortedVecSet<&input_device::InputDeviceType>,
    displays_recent_events: bool,
) -> InputPipelineAssembly {
    assembly.add_handler(crate::lib::inspect_handler::make_inspect_handler(
        node,
        supported_input_devices,
        displays_recent_events,
    ))
}

/// Hooks up the keymapper.
///
/// Converts HID key events to `KeyMeaning` events.
fn add_keymap_handler(
    assembly: InputPipelineAssembly,
    input_handlers_node: &inspect::Node,
    metrics_logger: metrics::MetricsLogger,
) -> InputPipelineAssembly {
    assembly.add_handler(keymap_handler::KeymapHandler::new(input_handlers_node, metrics_logger))
}

async fn add_ime(
    incoming: &Incoming,
    mut assembly: InputPipelineAssembly,
    input_handlers_node: &inspect::Node,
    metrics_logger: metrics::MetricsLogger,
) -> InputPipelineAssembly {
    if let Ok(ime_handler) = ImeHandler::new(incoming, input_handlers_node, metrics_logger).await {
        assembly = assembly.add_handler(ime_handler);
    }
    assembly
}

pub async fn handle_device_listener_registry_request_stream(
    mut stream_receiver: futures::channel::mpsc::UnboundedReceiver<
        DeviceListenerRegistryRequestStream,
    >,
    media_buttons_handler: Rc<MediaButtonsHandler>,
    touch_injector_handler: Rc<TouchInjectorHandler>,
) {
    while let Some(mut stream) = stream_receiver.next().await {
        let media_buttons_handler = media_buttons_handler.clone();
        let touch_injector_handler = touch_injector_handler.clone();
        fasync::Task::local(async move {
            loop {
                match stream.try_next().await {
                    Ok(Some(DeviceListenerRegistryRequest::RegisterListener {
                        listener,
                        responder,
                    })) => {
                        media_buttons_handler.register_listener_proxy(listener.into_proxy()).await;
                        let _ = responder.send();
                    }
                    Ok(Some(DeviceListenerRegistryRequest::RegisterTouchButtonsListener {
                        listener,
                        responder,
                    })) => {
                        touch_injector_handler.register_listener_proxy(listener.into_proxy()).await;
                        let _ = responder.send();
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        break;
                    }
                    Err(e) => {
                        warn!("Error handling device listener registry request stream: {}", e);
                        break;
                    }
                }
            }
        })
        .detach();
    }
}

pub async fn handle_factory_reset_countdown_request_stream(
    mut stream_receiver: futures::channel::mpsc::UnboundedReceiver<
        FactoryResetCountdownRequestStream,
    >,
    factory_reset_handler: Rc<FactoryResetHandler>,
) {
    while let Some(stream) = stream_receiver.next().await {
        let factory_reset_handler = factory_reset_handler.clone();
        fasync::Task::local(async move {
            match factory_reset_handler.handle_factory_reset_countdown_request_stream(stream).await
            {
                Ok(()) => (),
                Err(e) => {
                    warn!("failure while serving FactoryResetCountdown: {}", e);
                }
            }
        })
        .detach();
    }
}

pub async fn handle_light_sensor_request_stream(
    mut stream_receiver: futures::channel::mpsc::UnboundedReceiver<LightSensorRequestStream>,
    light_sensor_handler: Rc<CalibratedLightSensorHandler>,
) {
    while let Some(stream) = stream_receiver.next().await {
        let light_sensor_handler = light_sensor_handler.clone();
        fasync::Task::local(async move {
            match light_sensor_handler.handle_light_sensor_request_stream(stream).await {
                Ok(()) => (),
                Err(e) => {
                    warn!("failure while serving fuchsia.lightsensor.Sensor: {e}");
                }
            }
        })
        .detach();
    }
}

pub async fn handle_recovery_policy_device_request_stream(
    mut stream_receiver: futures::channel::mpsc::UnboundedReceiver<DeviceRequestStream>,
    factory_reset_handler: Rc<FactoryResetHandler>,
) {
    while let Some(stream) = stream_receiver.next().await {
        let factory_reset_handler = factory_reset_handler.clone();
        fasync::Task::local(async move {
            match factory_reset_handler.handle_recovery_policy_device_request_stream(stream).await {
                Ok(()) => (),
                Err(e) => {
                    warn!("failure while serving fuchsia.recovery.policy.Device: {}", e);
                }
            }
        })
        .detach();
    }
}

pub async fn handle_input_device_registry_request_streams(
    mut stream_receiver: futures::channel::mpsc::UnboundedReceiver<
        InputDeviceRegistryRequestStream,
    >,
    input_device_types: Vec<input_device::InputDeviceType>,
    input_event_sender: futures::channel::mpsc::UnboundedSender<Vec<input_device::InputEvent>>,
    input_device_bindings: InputDeviceBindingMap,
    injected_devices_node: inspect::Node,
    feature_flags: crate::lib::input_device::InputPipelineFeatureFlags,
    metrics_logger: metrics::MetricsLogger,
) {
    while let Some(stream) = stream_receiver.next().await {
        let input_device_types_clone = input_device_types.clone();
        let input_event_sender_clone = input_event_sender.clone();
        let input_device_bindings_clone = input_device_bindings.clone();
        let feature_flags_clone = feature_flags.clone();
        let metrics_logger_clone = metrics_logger.clone();

        // Must clone inspect node since we move it to our async task, but we want to
        // continue to operate on this inspect tree in future iterations of the loop.
        let node_clone = injected_devices_node.clone_weak();

        // TODO(https://fxbug.dev/42061133): Push this task down to InputPipeline.
        // I didn't do that here, to keep the scope of this change small.
        Dispatcher::spawn_local(async move {
            match InputPipeline::handle_input_device_registry_request_stream(
                stream,
                &input_device_types_clone,
                &input_event_sender_clone,
                &input_device_bindings_clone,
                &node_clone,
                feature_flags_clone,
                metrics_logger_clone,
            )
            .await
            {
                Ok(()) => (),
                Err(e) => {
                    warn!(
                        "failure while serving InputDeviceRegistry: {}; \
                             will continue serving other clients",
                        e
                    );
                }
            }
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use fuchsia_async as fasync;

    #[fasync::run_singlethreaded(test)]
    async fn test_placeholder() {
        // TODO(https://fxbug.dev/42153238): Add tests that verify the construction of the input pipeline.
    }
}
