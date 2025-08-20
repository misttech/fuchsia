// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use starnix_core::device::DeviceOps;
use starnix_core::task::{
    CurrentTask, EventHandler, SignalHandler, SignalHandlerInner, WaitCanceler, Waiter,
};
use starnix_core::vfs::{FileObject, FileOps, InputBufferExt, NamespaceNode};
use starnix_core::{fileops_impl_noop_sync, fileops_impl_seekable};
use starnix_logging::{impossible_error, log_error, log_warn};
use starnix_sync::{FileOpsCore, Locked};
use starnix_uapi::device_type::DeviceType;
use starnix_uapi::error;
use starnix_uapi::errors::{EIO, Errno, errno};
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::vfs::FdEvents;
use std::sync::Arc;
use zx::{AsHandleRef, HandleBased, Rights, WaitResult};
use {fidl_fuchsia_hardware_google_nanohub as fnanohub, fidl_fuchsia_starnix_runner as frunner};

use fuchsia_runtime;

#[derive(Clone)]
pub struct DataChannelDevice {
    service_proxy: Arc<fnanohub::DataChannelServiceProxy>,
}

impl DataChannelDevice {
    pub fn new(service_proxy: fnanohub::DataChannelServiceProxy) -> Self {
        DataChannelDevice { service_proxy: Arc::new(service_proxy) }
    }
}

impl DeviceOps for DataChannelDevice {
    fn open(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _current_task: &CurrentTask,
        _id: DeviceType,
        _node: &NamespaceNode,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        let device_proxy = (|| {
            let device_proxy = self.service_proxy.connect_to_device_sync()?;
            Ok(device_proxy)
        })()
        .map_err(|_: fidl::Error| errno!(EIO, "Failed to get data channel device"))?;

        Ok(Box::new(DataChannelFile::new(Arc::new(device_proxy))?))
    }
}

pub struct DataChannelFile {
    client: Arc<fnanohub::DataChannelSynchronousProxy>,
    // Event used to determine when data is available to read or write.
    event: Arc<zx::Event>,
}

impl DataChannelFile {
    pub fn new(client: Arc<fnanohub::DataChannelSynchronousProxy>) -> Result<Self, Errno> {
        let event = zx::Event::create();
        let event_dup = event.duplicate_handle(Rights::SAME_RIGHTS).map_err(|e| {
            log_error!("Failed to duplicate event handle: {:?}", e);
            Errno::new(EIO)
        })?;

        let manager =
            fuchsia_component::client::connect_to_protocol_sync::<frunner::ManagerMarker>()
                .expect("failed");

        let wake_source_event = event.duplicate_handle(Rights::SAME_RIGHTS).map_err(|e| {
            log_error!("Failed to duplicate event handle for wake source: {:?}", e);
            Errno::new(EIO)
        })?;
        manager
            .add_wake_source(frunner::ManagerAddWakeSourceRequest {
                container_job: Some(
                    fuchsia_runtime::job_default()
                        .duplicate(Rights::SAME_RIGHTS)
                        .expect("Failed to dup handle"),
                ),
                name: Some("nanohub-datachannel".to_string()),
                handle: Some(wake_source_event.into_handle()),
                signals: Some((zx::Signals::from_bits_truncate(fnanohub::SIGNAL_WAKELOCK)).bits()),
                ..Default::default()
            })
            .map_err(|e| errno!(EIO, e))?;

        client
            .register(event, zx::MonotonicInstant::INFINITE)
            .map_err(|e| errno!(EIO, e))?
            .map_err(|e| errno!(EIO, e))?;

        Ok(DataChannelFile { client, event: Arc::new(event_dup) })
    }
}

impl FileOps for DataChannelFile {
    fileops_impl_seekable!();
    fileops_impl_noop_sync!();

    fn read(
        &self,
        locked: &mut Locked<FileOpsCore>,
        file: &FileObject,
        current_task: &CurrentTask,
        _offset: usize,
        data: &mut dyn starnix_core::vfs::OutputBuffer,
    ) -> Result<usize, Errno> {
        file.blocking_op(locked, current_task, FdEvents::POLLIN | FdEvents::POLLHUP, None, |_| {
            match self
                .client
                .read(
                    &fnanohub::DataChannelReadRequest {
                        blocking: Some(false),
                        ..Default::default()
                    },
                    zx::MonotonicInstant::INFINITE,
                )
                .map_err(|e| errno!(EIO, e))?
            {
                Ok(response) => {
                    // Keep the wake lease alive until the data has been processed.
                    // The lease is dropped at the end of this scope.
                    let _wake_lease = response.wake_lease;
                    if let Some(d) = response.data {
                        if d.len() > data.available() {
                            log_warn!("Data returned by datachannel too large for buffer");
                            // We will drop data in this case.
                        }
                        data.write(&d)
                    } else {
                        Ok(0)
                    }
                }
                Err(zx::sys::ZX_ERR_SHOULD_WAIT) => error!(EAGAIN),
                Err(e) => error!(EIO, e),
            }
        })
    }

    fn write(
        &self,
        locked: &mut Locked<FileOpsCore>,
        file: &FileObject,
        current_task: &CurrentTask,
        _offset: usize,
        data: &mut dyn starnix_core::vfs::InputBuffer,
    ) -> Result<usize, Errno> {
        let data_vector = data.read_to_vec_limited(fnanohub::MAX_MESSAGE_SIZE as usize)?;

        let len = data_vector.len();

        file.blocking_op(locked, current_task, FdEvents::POLLOUT | FdEvents::POLLHUP, None, |_| {
            let request = fnanohub::DataChannelWriteRequest {
                data: Some(data_vector.clone()),
                ..Default::default()
            };
            match self
                .client
                .write(&request, zx::MonotonicInstant::INFINITE)
                .map_err(|e| errno!(EIO, e))?
            {
                Ok(_) => Ok(len),
                Err(e) if e == zx::sys::ZX_ERR_NO_RESOURCES => error!(EAGAIN, e),
                Err(e) => error!(EIO, e),
            }
        })
    }

    fn query_events(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObject,
        _current_task: &CurrentTask,
    ) -> Result<FdEvents, Errno> {
        let current_events = self.event.wait_handle(
            zx::Signals::from_bits_truncate(fnanohub::SIGNAL_READABLE | fnanohub::SIGNAL_WRITABLE)
                | zx::Signals::CHANNEL_PEER_CLOSED,
            zx::MonotonicInstant::INFINITE_PAST,
        );

        match current_events {
            WaitResult::Ok(signals) => Ok(get_events_from_signals(signals)),
            WaitResult::TimedOut(_) => Ok(FdEvents::empty()),
            WaitResult::Canceled(_) => {
                error!(EAGAIN)
            }
            WaitResult::Err(e) => Err(impossible_error(e)),
        }
    }

    fn wait_async(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObject,
        _current_task: &CurrentTask,
        waiter: &Waiter,
        events: FdEvents,
        handler: EventHandler,
    ) -> Option<WaitCanceler> {
        let signal_handler = SignalHandler {
            inner: SignalHandlerInner::ZxHandle(get_events_from_signals),
            event_handler: handler,
            err_code: None,
        };

        let pw = waiter
            .wake_on_zircon_signals(
                &self.event.as_handle_ref(),
                get_signals_from_events(events),
                signal_handler,
            )
            .unwrap();
        Some(WaitCanceler::new_port(pw))
    }
}

fn get_signals_from_events(events: FdEvents) -> zx::Signals {
    let mut result = zx::Signals::empty();
    if events.contains(FdEvents::POLLIN) {
        result |= zx::Signals::from_bits_truncate(fnanohub::SIGNAL_READABLE);
    }
    if events.contains(FdEvents::POLLOUT) {
        result |= zx::Signals::from_bits_truncate(fnanohub::SIGNAL_WRITABLE);
    }
    if events.contains(FdEvents::POLLHUP) {
        result |= zx::Signals::CHANNEL_PEER_CLOSED;
    }
    result
}

fn get_events_from_signals(signals: zx::Signals) -> FdEvents {
    let mut result = FdEvents::empty();
    if signals.contains(zx::Signals::from_bits_truncate(fnanohub::SIGNAL_READABLE)) {
        result |= FdEvents::POLLIN;
    }
    if signals.contains(zx::Signals::from_bits_truncate(fnanohub::SIGNAL_WRITABLE)) {
        result |= FdEvents::POLLOUT;
    }
    if signals.contains(zx::Signals::CHANNEL_PEER_CLOSED) {
        result |= FdEvents::POLLHUP;
    }
    result
}
