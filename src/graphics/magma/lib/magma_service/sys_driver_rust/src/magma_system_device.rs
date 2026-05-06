// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::traits;
use crate::traits::LogError;

use crate::magma_common_defs::{
    MAGMA_QUERY_MAXIMUM_INFLIGHT_PARAMS, MAX_INFLIGHT_MEMORY_MB, MAX_INFLIGHT_MESSAGES,
};
use crate::magma_system_connection::{ConnectionMessage, Owner as ConnectionOwner};
use fidl::endpoints::ServerEnd;
use futures::channel::mpsc::UnboundedSender;
use std::collections::HashMap;

use crate::magma_system_connection::MagmaStatus;
use fuchsia_async;
use std::sync::Mutex;
use zx;

pub struct MagmaSystemDevice {
    driver: Box<dyn traits::Driver>,
    msd_device: Box<dyn traits::Device>,
    perf_count_access_token_id: u64,
    connections: Mutex<HashMap<u64, UnboundedSender<ConnectionMessage>>>,
}

impl Drop for MagmaSystemDevice {
    fn drop(&mut self) {
        log::debug!("Dropping MagmaSystemDevice");
    }
}

impl ConnectionOwner for MagmaSystemDevice {
    fn driver(&self) -> &dyn traits::Driver {
        &*self.driver
    }

    fn perf_count_access_token_id(&self) -> u64 {
        self.perf_count_access_token_id
    }
}

impl MagmaSystemDevice {
    pub fn new(
        driver: Box<dyn traits::Driver>,
        msd_device: Box<dyn traits::Device>,
        perf_count_access_token_id: u64,
    ) -> Self {
        MagmaSystemDevice {
            driver,
            msd_device,
            perf_count_access_token_id,
            connections: Mutex::new(HashMap::new()),
        }
    }

    pub fn query(&self, id: u64) -> Result<(Option<zx::Vmo>, u64), MagmaStatus> {
        match id {
            MAGMA_QUERY_MAXIMUM_INFLIGHT_PARAMS => {
                let result = (MAX_INFLIGHT_MESSAGES << 32) | MAX_INFLIGHT_MEMORY_MB;
                Ok((None, result))
            }
            _ => self.msd_device.query(id),
        }
    }

    /// This requests that all of the connections are stopped.
    /// This does not wait for the connection threads to complete.
    pub fn stop_all_connections(&self) {
        let connections = self.connections.lock().unwrap();
        for (_, connection) in connections.iter() {
            let _ = connection.unbounded_send(ConnectionMessage::ContextKilled);
        }
    }

    pub fn start_connection_thread(
        self: std::sync::Arc<Self>,
        client_id: u64,
        connection: Box<dyn crate::traits::Connection>,
        primary_channel: ServerEnd<fidl_fuchsia_gpu_magma::PrimaryMarker>,
        notification_channel: ServerEnd<fidl_fuchsia_gpu_magma::NotificationMarker>,
    ) {
        let (message_sender, message_receiver) = futures::channel::mpsc::unbounded::<
            crate::magma_system_connection::ConnectionMessage,
        >();

        self.connections.lock().unwrap().insert(client_id, message_sender.clone());
        std::thread::Builder::new()
            .name(format!("ConnectionThread {}", client_id))
            .spawn(move || {
                let mut executor = fuchsia_async::LocalExecutor::default();
                executor.run_singlethreaded(async move {
                    let _ = fuchsia_scheduler::set_role_for_this_thread(
                        "fuchsia.graphics.magma.connection",
                    )
                    .log_err("Failed to set thread role");

                    let system_conn = crate::magma_system_connection::MagmaSystemConnection::new(
                        self.clone(),
                        connection,
                        notification_channel,
                        message_sender,
                    );

                    let mut server =
                        crate::primary_fidl_server::PrimaryFidlServer::new(system_conn);
                    let _ = server
                        .run(primary_channel.into_stream(), message_receiver)
                        .await
                        .log_err("Failed to run PrimaryFidlServer");

                    let mut map = self.connections.lock().unwrap();
                    map.remove(&client_id);
                });
            })
            .unwrap();
    }
}

impl crate::magma_system_connection::Owner for std::sync::Arc<MagmaSystemDevice> {
    fn driver(&self) -> &dyn crate::traits::Driver {
        (**self).driver()
    }
    fn perf_count_access_token_id(&self) -> u64 {
        (**self).perf_count_access_token_id()
    }
}

impl crate::traits::Device for MagmaSystemDevice {
    fn set_memory_pressure_level(&self, level: u32) {
        self.msd_device.set_memory_pressure_level(level)
    }

    fn query(&self, id: u64) -> Result<(Option<zx::Vmo>, u64), MagmaStatus> {
        self.query(id)
    }

    fn get_icd_list(&self) -> Result<Vec<crate::traits::MsdIcdInfo>, MagmaStatus> {
        self.msd_device.get_icd_list()
    }

    fn set_power_state(&self, power_state: i64, callback: Box<dyn FnOnce(i32) + Send>) {
        self.msd_device.set_power_state(power_state, callback)
    }

    fn dump_status(&self, dump_flags: u32) {
        self.msd_device.dump_status(dump_flags)
    }

    fn open(
        &self,
        client_id: u64,
        client_type: crate::traits::MagmaClientType,
    ) -> Option<Box<dyn crate::traits::Connection>> {
        self.msd_device.open(client_id, client_type)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{MockDevice, MockDriver};
    use crate::traits::Device;

    #[fuchsia::test]
    fn maximum_inflight_messages() {
        let driver = MockDriver;
        let msd_device = MockDevice;
        let device = MagmaSystemDevice::new(Box::new(driver), Box::new(msd_device), 0);

        const MAGMA_QUERY_MAXIMUM_INFLIGHT_PARAMS: u64 = 5;
        let result = device.query(MAGMA_QUERY_MAXIMUM_INFLIGHT_PARAMS).unwrap();
        let value = result.1;

        let messages = (value >> 32) as u32;
        let memory = value as u32;

        assert_eq!(messages, 1000);
        assert_eq!(memory, 100);
    }

    #[fuchsia::test]
    fn get_icd_list() {
        let driver = MockDriver;
        let msd_device = MockDevice;
        let device = MagmaSystemDevice::new(Box::new(driver), Box::new(msd_device), 0);

        let result = device.get_icd_list().unwrap();
        assert!(result.is_empty());
    }
}
