// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_input_report as fidl_input_report;
use fuchsia_async as fasync;
use fuchsia_component::server::ServiceFs;
use fuchsia_component_test::{ChildOptions, RealmBuilder, Ref};
use futures::StreamExt;

#[derive(Clone)]
pub(crate) struct InputReportMock {
    name: String,
}

impl InputReportMock {
    pub(crate) fn new<M: Into<String>>(name: M) -> Self {
        Self { name: name.into() }
    }
}

enum IncomingRequest {
    InputDeviceService(fidl_input_report::ServiceRequest),
}

#[async_trait::async_trait]
impl crate::traits::test_realm_component::TestRealmComponent for InputReportMock {
    fn ref_(&self) -> Ref {
        Ref::child(&self.name)
    }

    async fn add_to_builder(&self, builder: &RealmBuilder) {
        builder
            .add_local_child(
                &self.name,
                move |handles| {
                    Box::pin(async move {
                        let mut fs = ServiceFs::new();
                        fs.dir("svc")
                            .add_fidl_service_instance("default", IncomingRequest::InputDeviceService);

                        fs.serve_connection(handles.outgoing_dir)?;
                        fs.for_each_concurrent(None, |request| async {
                            match request {
                                IncomingRequest::InputDeviceService(
                                    fidl_input_report::ServiceRequest::InputDevice(mut stream),
                                ) => {
                                    while let Some(req) = stream.next().await {
                                        match req.unwrap() {
                                            fidl_input_report::InputDeviceRequest::GetDescriptor { responder } => {
                                                let mut desc = fidl_input_report::DeviceDescriptor::default();
                                                let mut cc = fidl_input_report::ConsumerControlDescriptor::default();
                                                cc.input = Some(fidl_input_report::ConsumerControlInputDescriptor {
                                                    buttons: Some(vec![
                                                        fidl_input_report::ConsumerControlButton::FactoryReset,
                                                    ]),
                                                    ..Default::default()
                                                });
                                                desc.consumer_control = Some(cc);
                                                responder.send(&desc).unwrap();
                                            }
                                            fidl_input_report::InputDeviceRequest::GetInputReportsReader { reader, control_handle: _ } => {
                                                fasync::Task::local(async move {
                                                    let mut stream = reader.into_stream();
                                                    if let Some(req) = stream.next().await {
                                                        let fidl_input_report::InputReportsReaderRequest::ReadInputReports { responder } = req.unwrap();
                                                        let mut report = fidl_input_report::InputReport::default();
                                                        report.event_time = Some(fasync::MonotonicInstant::now().into_nanos());
                                                        let mut cc = fidl_input_report::ConsumerControlInputReport::default();
                                                        cc.pressed_buttons = Some(vec![
                                                            fidl_input_report::ConsumerControlButton::FactoryReset,
                                                        ]);
                                                        report.consumer_control = Some(cc);
                                                        responder.send(Ok(vec![report])).unwrap();
                                                    }
                                                    // Wait for the next ReadInputReports and park it forever (hanging get)
                                                    if let Some(req) = stream.next().await {
                                                        let fidl_input_report::InputReportsReaderRequest::ReadInputReports { responder: _responder } = req.unwrap();
                                                        let () = futures::future::pending().await;
                                                    }
                                                }).detach();
                                            }
                                            fidl_input_report::InputDeviceRequest::GetFeatureReport { responder } => {
                                                responder.send(Ok(&fidl_input_report::FeatureReport::default())).unwrap();
                                            }
                                            req => {
                                                panic!("Unexpected InputDeviceRequest: {:?}", req);
                                            }
                                        }
                                    }
                                }
                            }
                        }).await;
                        Ok(())
                    })
                },
                ChildOptions::new(),
            )
            .await
            .unwrap();
    }
}
