// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_driver_framework as fdf;
use fidl_next::{Request, Responder};
use fidl_next_fuchsia_hardware_platform_device::{self as fdevice, DeviceServerHandler};

use fuchsia_async as fasync;
use fuchsia_component::server::ServiceFs;
use fuchsia_sync::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use zx::Status;

struct FakePDevState {
    metadata: HashMap<String, Vec<u8>>,
}

#[derive(Clone)]
pub struct FakePDev {
    state: Arc<Mutex<FakePDevState>>,
}

impl Default for FakePDev {
    fn default() -> Self {
        Self::new()
    }
}

impl FakePDev {
    pub fn new() -> Self {
        Self { state: Arc::new(Mutex::new(FakePDevState { metadata: HashMap::new() })) }
    }

    pub fn add_metadata(&self, id: &str, data: Vec<u8>) {
        self.state.lock().metadata.insert(id.to_string(), data);
    }

    pub fn serve(
        &self,
        service_fs: &mut ServiceFs<fuchsia_component::server::ServiceObj<'static, ()>>,
        scope: fasync::ScopeHandle,
        instance_name: &str,
    ) -> fdf::Offer {
        let state_clone = self.state.clone();

        fdf_component::ServiceOffer::<fdevice::Service>::new_next()
            .add_default_named_next(
                service_fs,
                instance_name,
                FakePDevService { state: state_clone, scope },
            )
            .build_zircon_offer_next()
    }
}

struct FakePDevService {
    state: Arc<Mutex<FakePDevState>>,
    scope: fasync::ScopeHandle,
}

impl fdevice::ServiceHandler for FakePDevService {
    fn device(&self, server_end: fidl_next::ServerEnd<fdevice::Device>) {
        server_end.spawn_on(FakePDevServer { state: self.state.clone() }, &self.scope);
    }
}

struct FakePDevServer {
    state: Arc<Mutex<FakePDevState>>,
}

impl DeviceServerHandler for FakePDevServer {
    async fn get_mmio_by_id(
        &mut self,
        _request: Request<fdevice::device::GetMmioById>,
        responder: Responder<fdevice::device::GetMmioById>,
    ) {
        let _ = responder.respond_err(Status::NOT_FOUND.into_raw()).await;
    }

    async fn get_mmio_by_name(
        &mut self,
        _request: Request<fdevice::device::GetMmioByName>,
        responder: Responder<fdevice::device::GetMmioByName>,
    ) {
        let _ = responder.respond_err(Status::NOT_FOUND.into_raw()).await;
    }

    async fn get_interrupt_by_id(
        &mut self,
        _request: Request<fdevice::device::GetInterruptById>,
        responder: Responder<fdevice::device::GetInterruptById>,
    ) {
        let _ = responder.respond_err(Status::NOT_FOUND.into_raw()).await;
    }

    async fn get_interrupt_by_name(
        &mut self,
        _request: Request<fdevice::device::GetInterruptByName>,
        responder: Responder<fdevice::device::GetInterruptByName>,
    ) {
        let _ = responder.respond_err(Status::NOT_FOUND.into_raw()).await;
    }

    async fn get_bti_by_id(
        &mut self,
        _request: Request<fdevice::device::GetBtiById>,
        responder: Responder<fdevice::device::GetBtiById>,
    ) {
        let _ = responder.respond_err(Status::NOT_FOUND.into_raw()).await;
    }

    async fn get_bti_by_name(
        &mut self,
        _request: Request<fdevice::device::GetBtiByName>,
        responder: Responder<fdevice::device::GetBtiByName>,
    ) {
        let _ = responder.respond_err(Status::NOT_FOUND.into_raw()).await;
    }

    async fn get_smc_by_id(
        &mut self,
        _request: Request<fdevice::device::GetSmcById>,
        responder: Responder<fdevice::device::GetSmcById>,
    ) {
        let _ = responder.respond_err(Status::NOT_FOUND.into_raw()).await;
    }

    async fn get_smc_by_name(
        &mut self,
        _request: Request<fdevice::device::GetSmcByName>,
        responder: Responder<fdevice::device::GetSmcByName>,
    ) {
        let _ = responder.respond_err(Status::NOT_FOUND.into_raw()).await;
    }

    async fn get_power_configuration(
        &mut self,
        responder: Responder<fdevice::device::GetPowerConfiguration>,
    ) {
        let power_elements: Vec<
            fidl_next_fuchsia_hardware_power::natural::PowerElementConfiguration,
        > = vec![];
        let _ = responder.respond(power_elements).await;
    }

    async fn get_node_device_info(
        &mut self,
        responder: Responder<fdevice::device::GetNodeDeviceInfo>,
    ) {
        let _ = responder.respond_err(Status::NOT_FOUND.into_raw()).await;
    }

    async fn get_board_info(&mut self, responder: Responder<fdevice::device::GetBoardInfo>) {
        let _ = responder.respond_err(Status::NOT_FOUND.into_raw()).await;
    }

    async fn get_metadata(
        &mut self,
        request: Request<fdevice::device::GetMetadata>,
        responder: Responder<fdevice::device::GetMetadata>,
    ) {
        let metadata = self.state.lock().metadata.get(request.payload().id.as_str()).cloned();
        if let Some(data) = metadata {
            let _ = responder.respond(data).await;
        } else {
            let _ = responder.respond_err(Status::NOT_FOUND.into_raw()).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl_next::fuchsia::create_channel;

    async fn run_test<F, Fut>(test_func: F)
    where
        F: FnOnce(fidl_next::Client<fdevice::Device>, FakePDev) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let fake_pdev = FakePDev::new();

        let (client_end, server_end) = create_channel::<fdevice::Device>();
        let server = FakePDevServer { state: fake_pdev.state.clone() };
        let scope = fasync::Scope::new_with_name("test");
        server_end.spawn_on(server, &scope);

        let client = client_end.spawn();
        test_func(client, fake_pdev).await;
    }

    #[fuchsia::test]
    async fn test_get_metadata() {
        run_test(|client, fake_pdev| async move {
            fake_pdev.add_metadata("test_id", vec![1, 2, 3, 4]);
            let res = client.get_metadata("test_id").await.unwrap();
            assert!(res.is_ok());
            assert_eq!(res.unwrap().metadata, vec![1, 2, 3, 4]);

            let res_err = client.get_metadata("unknown_id").await.unwrap();
            assert!(res_err.is_err());
        })
        .await;
    }
}
