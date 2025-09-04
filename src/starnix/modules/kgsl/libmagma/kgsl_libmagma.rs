// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use magma::{
    MAGMA_STATUS_OK, magma_connection_create_context2, magma_connection_release,
    magma_connection_release_context, magma_connection_t, magma_device_create_connection,
    magma_device_import, magma_device_query, magma_device_release, magma_device_t, magma_handle_t,
    magma_initialize_logging, magma_priority_t, magma_query_t, magma_status_t,
};
use starnix_logging::log_error;
use std::sync::Arc;
use zx::HandleBased;

fn magma_result(status: magma_status_t) -> Result<(), magma_status_t> {
    if status == MAGMA_STATUS_OK { Ok(()) } else { Err(status) }
}

#[track_caller]
fn kgsl_log_error(status: &magma_status_t) {
    log_error!("kgsl: {}({}): {}", file!(), line!(), status);
}

pub fn initialize_logging(channel: zx::Channel) -> Result<(), ()> {
    let result = unsafe { magma_initialize_logging(channel.into_raw()) };
    if result == MAGMA_STATUS_OK { Ok(()) } else { Err(()) }
}

pub struct Device {
    inner: Arc<DeviceInternal>,
}
struct DeviceInternal {
    magma_device: magma_device_t,
}

impl Drop for DeviceInternal {
    fn drop(&mut self) {
        unsafe { magma_device_release(self.magma_device) };
    }
}

impl Device {
    pub fn from_channel(channel: zx::Channel) -> Result<Self, magma_status_t> {
        let mut magma_device: magma_device_t = 0;
        let result = unsafe { magma_device_import(channel.into_raw(), &mut magma_device) };
        magma_result(result).inspect_err(kgsl_log_error)?;
        Ok(Device { inner: Arc::new(DeviceInternal { magma_device }) })
    }

    pub fn query_value(&self, id: magma_query_t) -> Result<u64, magma_status_t> {
        let mut result_out: u64 = 0;
        let mut result_buffer_out: magma_handle_t = 0;
        let result = unsafe {
            magma_device_query(self.inner.magma_device, id, &mut result_buffer_out, &mut result_out)
        };
        assert!(result_buffer_out == 0);
        magma_result(result).inspect_err(kgsl_log_error)?;
        Ok(result_out)
    }

    pub fn create_connection(&self) -> Result<Connection, magma_status_t> {
        let mut magma_connection: magma_connection_t = 0;
        let result = unsafe {
            magma_device_create_connection(self.inner.magma_device, &mut magma_connection)
        };
        magma_result(result).inspect_err(kgsl_log_error)?;
        Ok(Connection { inner: Arc::new(ConnectionInternal { magma_connection }) })
    }
}

pub struct Connection {
    inner: Arc<ConnectionInternal>,
}

struct ConnectionInternal {
    magma_connection: magma_connection_t,
}

impl Drop for ConnectionInternal {
    fn drop(&mut self) {
        unsafe { magma_connection_release(self.magma_connection) };
    }
}

impl Connection {
    pub fn create_context(&self, priority: magma_priority_t) -> Result<Context, magma_status_t> {
        let mut magma_context_id: u32 = 0;
        let result = unsafe {
            magma_connection_create_context2(
                self.inner.magma_connection,
                priority,
                &mut magma_context_id,
            )
        };
        magma_result(result).inspect_err(kgsl_log_error)?;
        Ok(Context {
            _inner: Arc::new(ContextInternal { connection: self.inner.clone(), magma_context_id }),
        })
    }
}

pub struct Context {
    _inner: Arc<ContextInternal>,
}

struct ContextInternal {
    connection: Arc<ConnectionInternal>,
    magma_context_id: u32,
}

impl Drop for ContextInternal {
    fn drop(&mut self) {
        unsafe {
            magma_connection_release_context(
                self.connection.magma_connection,
                self.magma_context_id,
            )
        };
    }
}
