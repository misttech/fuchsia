// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use magma::{
    MAGMA_STATUS_OK, magma_buffer_id_t, magma_buffer_t, magma_connection_create_buffer,
    magma_connection_create_context2, magma_connection_create_semaphore, magma_connection_release,
    magma_connection_release_buffer, magma_connection_release_context,
    magma_connection_release_semaphore, magma_connection_t, magma_device_create_connection,
    magma_device_import, magma_device_query, magma_device_release, magma_device_t, magma_handle_t,
    magma_initialize_logging, magma_priority_t, magma_query_t, magma_semaphore_id_t,
    magma_semaphore_reset, magma_semaphore_signal, magma_semaphore_t, magma_status_t,
};
use starnix_logging::log_error;
use std::panic::Location;
use std::sync::Arc;
use zx::HandleBased;

fn magma_result(status: magma_status_t) -> Result<(), magma_status_t> {
    if status == MAGMA_STATUS_OK { Ok(()) } else { Err(status) }
}

trait KgslErrorLogger {
    #[track_caller]
    fn kgsl_log_error(self) -> Self;
}

impl<T> KgslErrorLogger for Result<T, magma_status_t> {
    #[track_caller]
    fn kgsl_log_error(self) -> Self {
        match self {
            Ok(v) => Ok(v),
            Err(status) => {
                let caller = Location::caller();
                log_error!("kgsl: {}({}): {}", caller.file(), caller.line(), status);
                Err(status)
            }
        }
    }
}

pub fn initialize_logging(channel: zx::Channel) -> Result<(), ()> {
    // Safety: magma_initialize_logging takes ownership of the channel.
    let result = unsafe { magma_initialize_logging(channel.into_raw()) };
    if result == MAGMA_STATUS_OK { Ok(()) } else { Err(()) }
}

#[derive(Debug)]
pub enum QueryOutput {
    Value(u64),
    Buffer(zx::Vmo),
}

#[derive(Debug)]
pub struct Device {
    inner: Arc<DeviceInternal>,
}

#[derive(Debug)]
struct DeviceInternal {
    magma_device: magma_device_t,
}

impl Drop for DeviceInternal {
    fn drop(&mut self) {
        // Safety: magma_device_release takes ownership of the device handle.
        unsafe { magma_device_release(self.magma_device) };
    }
}

impl Device {
    pub fn from_channel(channel: zx::Channel) -> Result<Self, magma_status_t> {
        let mut magma_device: magma_device_t = 0;
        // Safety: magma_device_import takes ownership of the channel and returns a
        // device handle.
        let result = unsafe { magma_device_import(channel.into_raw(), &mut magma_device) };
        magma_result(result).kgsl_log_error()?;
        Ok(Device { inner: Arc::new(DeviceInternal { magma_device }) })
    }

    pub fn query(&self, id: magma_query_t) -> Result<QueryOutput, magma_status_t> {
        let mut result_out: u64 = 0;
        let mut result_buffer_out: magma_handle_t = 0;
        // Safety: magma_device_query borrows the device handle and maybe returns a
        // buffer handle.
        let result = unsafe {
            magma_device_query(self.inner.magma_device, id, &mut result_buffer_out, &mut result_out)
        };
        magma_result(result).kgsl_log_error()?;
        if result_buffer_out != 0 {
            // Safety: from_raw takes ownership of the buffer handle.
            return Ok(QueryOutput::Buffer(zx::Vmo::from_handle(unsafe {
                zx::NullableHandle::from_raw(result_buffer_out)
            })));
        }
        Ok(QueryOutput::Value(result_out))
    }

    pub fn create_connection(&self) -> Result<Connection, magma_status_t> {
        let mut magma_connection: magma_connection_t = 0;
        // Safety: magma_device_create_connection borrows the device handle and returns
        // a connection handle.
        let result = unsafe {
            magma_device_create_connection(self.inner.magma_device, &mut magma_connection)
        };
        magma_result(result).kgsl_log_error()?;
        Ok(Connection { inner: Arc::new(ConnectionInternal { magma_connection }) })
    }
}

#[derive(Debug)]
pub struct Connection {
    inner: Arc<ConnectionInternal>,
}

#[derive(Debug)]
struct ConnectionInternal {
    magma_connection: magma_connection_t,
}

impl Drop for ConnectionInternal {
    fn drop(&mut self) {
        // Safety: magma_connection_release takes ownership of the connection handle.
        unsafe { magma_connection_release(self.magma_connection) };
    }
}

impl Connection {
    pub fn create_context(&self, priority: magma_priority_t) -> Result<Context, magma_status_t> {
        let mut magma_context_id: u32 = 0;
        // Safety: magma_connection_create_context2 borrows the connection handle and
        // returns a context id.
        let result = unsafe {
            magma_connection_create_context2(
                self.inner.magma_connection,
                priority,
                &mut magma_context_id,
            )
        };
        magma_result(result).kgsl_log_error()?;
        Ok(Context {
            inner: Arc::new(ContextInternal { connection: self.inner.clone(), magma_context_id }),
        })
    }

    pub fn create_semaphore(&self) -> Result<Semaphore, magma_status_t> {
        let mut magma_semaphore: magma_semaphore_t = 0;
        let mut magma_semaphore_id: magma_semaphore_id_t = 0;
        // Safety: magma_connection_create_semaphore borrows the connection handle and
        // returns a semaphore handle.
        let result = unsafe {
            magma_connection_create_semaphore(
                self.inner.magma_connection,
                &mut magma_semaphore,
                &mut magma_semaphore_id,
            )
        };
        magma_result(result).kgsl_log_error()?;
        Ok(Semaphore {
            inner: Arc::new(SemaphoreInternal {
                connection: self.inner.clone(),
                magma_semaphore,
                magma_semaphore_id,
            }),
        })
    }

    pub fn create_buffer(&self, size: u64) -> Result<Buffer, magma_status_t> {
        let mut size_out: u64 = 0;
        let mut magma_buffer: magma_buffer_t = 0;
        let mut magma_buffer_id: magma_buffer_id_t = 0;
        // Safety: magma_connection_create_buffer borrows the connection handle and
        // returns a buffer handle.
        let result = unsafe {
            magma_connection_create_buffer(
                self.inner.magma_connection,
                size,
                &mut size_out,
                &mut magma_buffer,
                &mut magma_buffer_id,
            )
        };
        magma_result(result).kgsl_log_error()?;
        Ok(Buffer {
            inner: Arc::new(BufferInternal {
                connection: self.inner.clone(),
                magma_buffer,
                magma_buffer_id,
                size: size_out,
            }),
        })
    }
}

#[derive(Debug)]
pub struct Context {
    #[expect(dead_code)]
    inner: Arc<ContextInternal>,
}

#[derive(Debug)]
struct ContextInternal {
    connection: Arc<ConnectionInternal>,
    magma_context_id: u32,
}

impl Drop for ContextInternal {
    fn drop(&mut self) {
        // Safety: magma_connection_release_context borrows the connection handle and
        // takes ownership of the context id.
        unsafe {
            magma_connection_release_context(
                self.connection.magma_connection,
                self.magma_context_id,
            )
        };
    }
}

#[derive(Debug)]
pub struct Semaphore {
    inner: Arc<SemaphoreInternal>,
}

impl Semaphore {
    pub fn id(&self) -> magma_semaphore_id_t {
        self.inner.magma_semaphore_id
    }

    pub fn signal(&self) {
        // Safety: magma_semaphore_signal borrows the semaphore handle.
        unsafe { magma_semaphore_signal(self.inner.magma_semaphore) }
    }

    pub fn reset(&self) {
        // Safety: magma_semaphore_reset borrows the semaphore handle.
        unsafe { magma_semaphore_reset(self.inner.magma_semaphore) }
    }
}

#[derive(Debug)]
struct SemaphoreInternal {
    connection: Arc<ConnectionInternal>,
    magma_semaphore: magma_semaphore_t,
    magma_semaphore_id: magma_semaphore_id_t,
}

impl Drop for SemaphoreInternal {
    fn drop(&mut self) {
        // Safety: magma_connection_release_semaphore borrows the connection handle and
        // takes ownership of the semaphore handle.
        unsafe {
            magma_connection_release_semaphore(
                self.connection.magma_connection,
                self.magma_semaphore,
            )
        };
    }
}

#[derive(Debug)]
pub struct Buffer {
    inner: Arc<BufferInternal>,
}

impl Buffer {
    pub fn id(&self) -> magma_buffer_id_t {
        self.inner.magma_buffer_id
    }

    pub fn size(&self) -> u64 {
        self.inner.size
    }
}

#[derive(Debug)]
struct BufferInternal {
    connection: Arc<ConnectionInternal>,
    magma_buffer: magma_buffer_t,
    magma_buffer_id: magma_buffer_id_t,
    size: u64,
}

impl Drop for BufferInternal {
    fn drop(&mut self) {
        // Safety: magma_connection_release_buffer borrows the connection handle and
        // takes ownership of the buffer handle.
        unsafe {
            magma_connection_release_buffer(self.connection.magma_connection, self.magma_buffer)
        };
    }
}
