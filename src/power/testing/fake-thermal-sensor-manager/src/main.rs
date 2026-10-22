// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_hardware_temperature as ftemperature;
use fidl_fuchsia_hardware_trippoint as ftrippoint;
use fidl_fuchsia_thermal as fthermal;
use fuchsia_async as fasync;
use fuchsia_component::server::ServiceFs;
use futures::{StreamExt, TryStreamExt};
use log::info;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

const SENSOR_NAME: &'static str = "fake-trippoint";
const DEFAULT_SENSOR_TEMPERATURE: f32 = 25.0;

struct SensorProps {
    info: fthermal::SensorInfo,
    temperature: f32,
    override_temperature: RefCell<Option<f32>>,
    trip_point_descriptors: RefCell<Vec<ftrippoint::TripPointDescriptor>>,
    pending_wait_responders: RefCell<Vec<ftrippoint::TripPointWaitForAnyTripPointResponder>>,
}

fn default_trip_point_descriptors() -> Vec<ftrippoint::TripPointDescriptor> {
    vec![
        ftrippoint::TripPointDescriptor {
            index: 0,
            type_: ftrippoint::TripPointType::OneshotTempAbove,
            configuration: ftrippoint::TripPointValue::ClearedTripPoint(
                ftrippoint::ClearedTripPoint,
            ),
        },
        ftrippoint::TripPointDescriptor {
            index: 1,
            type_: ftrippoint::TripPointType::OneshotTempBelow,
            configuration: ftrippoint::TripPointValue::ClearedTripPoint(
                ftrippoint::ClearedTripPoint,
            ),
        },
    ]
}

fn spawn_sensor_server(
    scope: &fasync::ScopeHandle,
    server: fthermal::SensorServer_,
    props: Rc<SensorProps>,
) {
    match server {
        fthermal::SensorServer_::Temperature(temp_server_end) => {
            let mut stream = temp_server_end.into_stream();
            let props = props.clone();
            scope.spawn_local(async move {
                while let Ok(Some(req)) = stream.try_next().await {
                    match req {
                        ftemperature::DeviceRequest::GetTemperatureCelsius { responder } => {
                            let temp_c_opt = props.override_temperature.borrow().clone();
                            let temp_c = match temp_c_opt {
                                Some(temp_c) => temp_c,
                                None => props.temperature,
                            };
                            responder.send(zx::sys::ZX_OK, temp_c as f32).unwrap();
                        }
                        ftemperature::DeviceRequest::GetSensorName { responder } => {
                            match &props.info.name {
                                Some(name) => responder.send(name).unwrap(),
                                None => unreachable!("unknown sensor name"),
                            }
                        }
                    }
                }
            });
        }
        fthermal::SensorServer_::Trippoint(trippoint_server_end) => {
            let mut stream = trippoint_server_end.into_stream();
            let props = props.clone();
            scope.spawn_local(async move {
                while let Ok(Some(req)) = stream.try_next().await {
                    match req {
                        ftrippoint::TripPointRequest::GetTripPointDescriptors { responder } => {
                            let descriptors = props.trip_point_descriptors.borrow();
                            let _ = responder.send(Ok(&descriptors));
                        }
                        ftrippoint::TripPointRequest::SetTripPoints { descriptors, responder } => {
                            let status = {
                                let mut current = props.trip_point_descriptors.borrow_mut();
                                let mut status = Ok(());
                                for desc in &descriptors {
                                    let idx = desc.index as usize;
                                    if idx >= current.len() {
                                        status = Err(zx::Status::OUT_OF_RANGE.into_raw());
                                        break;
                                    }
                                    let temp_c = match (current[idx].type_, &desc.configuration) {
                                        (_, ftrippoint::TripPointValue::ClearedTripPoint(_)) => {
                                            Some(DEFAULT_SENSOR_TEMPERATURE)
                                        }
                                        (
                                            ftrippoint::TripPointType::OneshotTempAbove,
                                            ftrippoint::TripPointValue::OneshotTempAboveTripPoint(
                                                tp,
                                            ),
                                        ) if tp.critical_temperature_celsius.is_finite() => {
                                            Some(tp.critical_temperature_celsius)
                                        }
                                        (
                                            ftrippoint::TripPointType::OneshotTempBelow,
                                            ftrippoint::TripPointValue::OneshotTempBelowTripPoint(
                                                tp,
                                            ),
                                        ) if tp.critical_temperature_celsius.is_finite() => {
                                            Some(tp.critical_temperature_celsius)
                                        }
                                        _ => None,
                                    };
                                    let Some(temp_c) = temp_c else {
                                        status = Err(zx::Status::INVALID_ARGS.into_raw());
                                        break;
                                    };
                                    if desc.type_ != current[idx].type_ {
                                        status = Err(zx::Status::INVALID_ARGS.into_raw());
                                        break;
                                    }
                                    if temp_c <= -273.15 {
                                        status = Err(zx::Status::OUT_OF_RANGE.into_raw());
                                        break;
                                    }
                                }
                                if status.is_ok() {
                                    for desc in descriptors {
                                        let idx = desc.index as usize;
                                        current[idx] = desc;
                                    }
                                }
                                status
                            };
                            if status.is_ok() {
                                let has_active =
                                    props.trip_point_descriptors.borrow().iter().any(|d| {
                                        !matches!(
                                            d.configuration,
                                            ftrippoint::TripPointValue::ClearedTripPoint(_)
                                        )
                                    });
                                if !has_active {
                                    for waiter in
                                        props.pending_wait_responders.borrow_mut().drain(..)
                                    {
                                        let _ = waiter.send(Err(zx::Status::BAD_STATE.into_raw()));
                                    }
                                }
                            }
                            let _ = responder.send(status);
                        }
                        ftrippoint::TripPointRequest::WaitForAnyTripPoint { responder } => {
                            let has_active =
                                props.trip_point_descriptors.borrow().iter().any(|d| {
                                    !matches!(
                                        d.configuration,
                                        ftrippoint::TripPointValue::ClearedTripPoint(_)
                                    )
                                });
                            if !has_active {
                                let _ = responder.send(Err(zx::Status::BAD_STATE.into_raw()));
                            } else {
                                props.pending_wait_responders.borrow_mut().push(responder);
                            }
                        }
                        ftrippoint::TripPointRequest::_UnknownMethod { .. } => {}
                    }
                }
            });
        }
        _ => unreachable!(),
    }
}

fn handle_sensor_manager_stream(
    scope: fasync::ScopeHandle,
    mut stream: fthermal::SensorManagerRequestStream,
    sensors: Rc<HashMap<String, Rc<SensorProps>>>,
) {
    let scope_clone = scope.clone();
    scope.spawn_local(async move {
        while let Ok(Some(req)) = stream.try_next().await {
            match req {
                fthermal::SensorManagerRequest::ListSensors { responder } => {
                    let infos = sensors.values().map(|s| s.info.clone()).collect::<Vec<_>>();
                    responder.send(&infos).unwrap();
                }
                fthermal::SensorManagerRequest::SetTemperatureOverride {
                    name,
                    override_temperature,
                    responder,
                } => {
                    let sensor = sensors.get(&name).expect("unknown sensor name");
                    sensor.override_temperature.borrow_mut().replace(override_temperature);
                    responder.send(Ok(())).unwrap();
                }
                fthermal::SensorManagerRequest::ClearTemperatureOverride { name, responder } => {
                    let sensor = sensors.get(&name).expect("unknown sensor name");
                    sensor.override_temperature.borrow_mut().take();
                    responder.send(Ok(())).unwrap();
                }
                fthermal::SensorManagerRequest::Connect { payload, responder } => {
                    let name = payload.name.expect("name is required");
                    let server = payload.server_end.expect("server is required");

                    let sensor = sensors.get(&name).expect("unknown sensor name");
                    spawn_sensor_server(&scope_clone, server, sensor.clone());
                    responder.send(Ok(())).unwrap();
                }
                _ => unreachable!(),
            }
        }
    });
}

#[fuchsia::main]
async fn main() {
    info!("Started fake-thermal-sensor-manager");
    let scope = fasync::Scope::new();
    let scope_handle = scope.to_handle();

    let sensors = Rc::new({
        let mut sensors = HashMap::new();
        sensors.insert(
            SENSOR_NAME.to_string(),
            Rc::new(SensorProps {
                info: fthermal::SensorInfo {
                    name: Some(SENSOR_NAME.to_string()),
                    ..Default::default()
                },
                temperature: DEFAULT_SENSOR_TEMPERATURE.into(),
                override_temperature: RefCell::new(None),
                trip_point_descriptors: RefCell::new(default_trip_point_descriptors()),
                pending_wait_responders: RefCell::new(Vec::new()),
            }),
        );
        sensors
    });

    let mut fs = ServiceFs::new_local();
    fs.dir("svc").add_fidl_service(move |stream| {
        handle_sensor_manager_stream(scope_handle.clone(), stream, sensors.clone())
    });

    fs.take_and_serve_directory_handle().unwrap();
    fs.collect::<()>().await;
    scope.join().await;
}
