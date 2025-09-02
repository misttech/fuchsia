// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Context;
use fidl_fuchsia_sys2 as fsys;
use fuchsia_inspect::Property;
use starnix_core::device::DeviceOps;
use starnix_core::task::{CurrentTask, Kernel};
use starnix_core::vfs::buffers::{InputBuffer, OutputBuffer};
use starnix_core::vfs::{
    CloseFreeSafe, FileObject, FileOps, NamespaceNode, fileops_impl_nonseekable,
    fileops_impl_noop_sync,
};
use starnix_logging::{log_error, log_info};
use starnix_sync::{FileOpsCore, LockEqualOrBefore, Locked, Mutex, Unlocked};
use starnix_uapi::device_type::DeviceType;
use starnix_uapi::error;
use starnix_uapi::errors::Errno;
use starnix_uapi::open_flags::OpenFlags;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use zerocopy::IntoBytes;

/// Initializes the boot notifier device.
pub fn booted_device_init(locked: &mut Locked<Unlocked>, system_task: &CurrentTask) {
    let kernel = system_task.kernel();

    let (booted_sender, booted_receiver) = channel::<bool>();
    let node = fuchsia_inspect::component::inspector().root().create_child("boot");
    let device = BootedDevice::new(booted_sender, node);
    device.clone().register(locked, &kernel.kthreads.system_task());
    device.start_relay(&kernel, booted_receiver);

    let registry = &kernel.device_registry;
    registry
        .register_misc_device(locked, system_task, "booted".into(), device)
        .expect("can register boot_notifier");
}

#[derive(Clone)]
struct BootedDevice {
    inner: Arc<Inner>,
}

const INSPECT_KEY: &str = "boot_timestamp";

impl BootedDevice {
    pub fn new(booted_sender: Sender<bool>, inspect_node: fuchsia_inspect::Node) -> Self {
        let boot_timestamp = inspect_node.create_uint(INSPECT_KEY, 0);
        Self {
            inner: Arc::new(Inner {
                file: File::new(booted_sender),
                _inspect_node: inspect_node,
                boot_timestamp,
            }),
        }
    }

    pub fn register<L>(self, locked: &mut Locked<L>, system_task: &CurrentTask)
    where
        L: LockEqualOrBefore<FileOpsCore>,
    {
        let kernel = system_task.kernel();
        let registry = &kernel.device_registry;
        registry
            .register_dyn_device(
                locked,
                system_task,
                "booted".into(),
                registry.objects.starnix_class(),
                self,
            )
            .expect("can register booted device");
    }

    pub fn start_relay(&self, kernel: &Kernel, booted_receiver: Receiver<bool>) {
        let this = self.inner.clone();
        kernel.kthreads.spawn(move |_lock_context, _current_task| {
            let mut prev_booted = false;
            while let Ok(booted) = booted_receiver.recv() {
                if booted && !prev_booted {
                    match this.notify_component_manager() {
                        Ok(()) => log_info!("Notified component_manager of system boot"),
                        Err(e) => {
                            log_error!("Failed to notify component_manager of system boot: {e}")
                        }
                    }
                }
                prev_booted = booted;
            }
            log_error!("booted relay was terminated unexpectedly.");
        });
    }
}

struct Inner {
    file: Arc<File>,
    _inspect_node: fuchsia_inspect::Node,
    boot_timestamp: fuchsia_inspect::UintProperty,
}

impl Inner {
    fn notify_component_manager(&self) -> Result<(), anyhow::Error> {
        let client =
            fuchsia_component::client::connect_to_protocol_sync::<fsys::BootControllerMarker>()
                .context("connecting to BootController")?;
        client.notify(zx::MonotonicInstant::INFINITE).context("calling BootController/Notify")?;
        let ts = zx::BootInstant::get().into_nanos() as u64;
        let _ = self.boot_timestamp.set(ts);
        Ok(())
    }
}

struct File {
    booted: Mutex<bool>,
    sender: Sender<bool>,
}

impl File {
    fn new(sender: Sender<bool>) -> Arc<Self> {
        Arc::new(Self { booted: Mutex::new(false), sender })
    }
}

/// `TouchPowerPolicyFile` doesn't implement the `close` method.
impl CloseFreeSafe for File {}
impl FileOps for File {
    fileops_impl_nonseekable!();
    fileops_impl_noop_sync!();

    fn read(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObject,
        _current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        debug_assert!(offset == 0);
        let booted = self.booted.lock().to_owned();
        data.write_all(booted.as_bytes())
    }

    fn write(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObject,
        _current_task: &CurrentTask,
        _offset: usize,
        data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        let content = data.read_all()?;
        let booted = match &*content {
            b"0" | b"0\n" => false,
            b"1" | b"1\n" => true,
            _ => {
                log_error!("Invalid booted value - must be 0 or 1");
                return error!(EINVAL);
            }
        };
        *self.booted.lock() = booted;
        if let Err(e) = self.sender.send(booted) {
            log_error!("unable to send recent booted state to device relay: {:?}", e);
        }
        Ok(content.len())
    }
}

impl DeviceOps for BootedDevice {
    fn open(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _current_task: &CurrentTask,
        _device_type: DeviceType,
        _node: &NamespaceNode,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        let file = self.inner.file.clone();
        Ok(Box::new(file))
    }
}
