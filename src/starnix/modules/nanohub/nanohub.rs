// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::datachannel_file::DataChannelDevice;
use crate::nanohub_comms_directory::{
    build_display_comms_directory, build_nanohub_comms_directory,
};
use crate::nanohub_gpio_stub::register_gpio_chip_device;
use crate::socket_tunnel_file::register_socket_tunnel_device;
use fidl_fuchsia_hardware_google_nanohub as fnanohub;
use fidl_fuchsia_hardware_serial as fserial;
use fuchsia_async::TimeoutExt as _;
use futures::TryStreamExt;
use starnix_core::device::service_watcher::ServiceWatcher;
use starnix_core::task::Kernel;
use starnix_logging::{log_error, log_info, log_warn};
use starnix_modules_serial::SerialDevice;
use starnix_uapi::auth::FsCred;

use std::sync::Arc;

const SERIAL_DIRECTORY: &str = "/dev/class/serial";
const GET_IDENTIFIER_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(5);

pub fn nanohub_device_init(kernel: &Arc<Kernel>) {
    register_gpio_chip_device(kernel, "gpiochipwake0");

    register_socket_tunnel_device(
        kernel,
        "/dev/display_comms".into(),
        "display_comms".into(),
        "display".into(),
        build_display_comms_directory,
    );

    // /dev/nanohub_comms requires a set of additional sysfs nodes, so create this route
    // with a specialized directory.
    register_socket_tunnel_device(
        kernel,
        "/dev/nanohub_comms".into(),
        "nanohub_comms".into(),
        "nanohub".into(),
        build_nanohub_comms_directory,
    );

    // Spawn future to bind and configure serial device
    kernel.kthreads.spawn_future(
        {
            let kernel = kernel.clone();
            move || async move { register_serial_device(kernel).await }
        },
        "register_serial_device",
    );

    register_datachannel_devices(kernel);
}

fn register_datachannel_devices(kernel: &Kernel) {
    ServiceWatcher::register_with_probe(
        kernel,
        fnanohub::StarnixDataChannelServiceMarker,
        probe_datachannel_device,
        |current_task, (data_channel_service_proxy, name), scope| {
            let kernel = current_task.kernel();
            let registry = &kernel.device_registry;
            let device_class = registry.objects.get_or_create_class("nanohub".into());
            match registry.register_dyn_device(
                kernel,
                name.as_bytes().into(),
                device_class,
                DataChannelDevice::new(
                    data_channel_service_proxy,
                    kernel.suspend_resume_manager.clone(),
                ),
            ) {
                Ok(device) => {
                    scope.add_scoped_device(device);
                }
                Err(e) => log_warn!("Failed to register datachannel device: {:?}", e),
            }
        },
    );
}

fn is_valid_datachannel_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\0'])
}

async fn probe_datachannel_device(
    data_channel_service_proxy: Arc<fnanohub::StarnixDataChannelServiceProxy>,
) -> Option<(Arc<fnanohub::StarnixDataChannelServiceProxy>, String)> {
    // Connect to `waitable` only for the duration of `GetIdentifier`; keeping an unbound
    // `waitable` connection open reserves a pooled endpoint in the nanohub driver.
    let device_proxy = match data_channel_service_proxy.connect_to_waitable() {
        Ok(proxy) => proxy,
        Err(e) => {
            log_error!("Failed to connect to waitable data channel: {:?}", e);
            return None;
        }
    };
    match async { Some(device_proxy.get_identifier().await) }
        .on_timeout(GET_IDENTIFIER_TIMEOUT, || None)
        .await
    {
        Some(Ok(fnanohub::UnboundWaitableDataChannelGetIdentifierResponse {
            name: Some(name),
            ..
        })) if is_valid_datachannel_name(&name) => Some((data_channel_service_proxy, name)),
        Some(Ok(id)) => {
            log_error!(
                "Data channel device returned missing or invalid name {:?}, skipping registration",
                id.name
            );
            None
        }
        Some(Err(e)) => {
            log_error!("Failed to get device info: {:?}", e);
            None
        }
        None => {
            log_error!("Timed out getting data channel device identifier");
            None
        }
    }
}

async fn register_serial_device(kernel: Arc<Kernel>) {
    // TODO Move this to expect once test support is enabled
    let dir =
        match fuchsia_fs::directory::open_in_namespace(SERIAL_DIRECTORY, fuchsia_fs::PERM_READABLE)
        {
            Ok(dir) => dir,
            Err(e) => {
                log_error!("Failed to open serial directory: {:}", e);
                return;
            }
        };

    let mut watcher = match fuchsia_fs::directory::Watcher::new(&dir).await {
        Ok(watcher) => watcher,
        Err(e) => {
            log_info!("Failed to create directory watcher for serial device: {:}", e);
            return;
        }
    };

    loop {
        match watcher.try_next().await {
            Ok(Some(watch_msg)) => {
                let filename = watch_msg
                    .filename
                    .as_path()
                    .to_str()
                    .expect("Failed to convert watch_msg to str");
                if filename == "." {
                    continue;
                }
                if watch_msg.event == fuchsia_fs::directory::WatchEvent::ADD_FILE
                    || watch_msg.event == fuchsia_fs::directory::WatchEvent::EXISTING
                {
                    let instance_path = format!("{}/{}", SERIAL_DIRECTORY, filename);
                    let (client_channel, server_channel) = zx::Channel::create();
                    if let Err(_) = fdio::service_connect(&instance_path, server_channel) {
                        continue;
                    }

                    // `fuchsia.hardware.serial` exposes a `DeviceProxy` type used for binding with
                    // a `Device` type. This should not be confused with the `DeviceProxy` generated
                    // by FIDL
                    let device_proxy = fserial::DeviceProxy_SynchronousProxy::new(client_channel);
                    let (serial_proxy, server_end) =
                        fidl::endpoints::create_sync_proxy::<fserial::DeviceMarker>();

                    // Instruct the serial driver to bind the connection to the underlying device
                    if let Err(_) = device_proxy.get_channel(server_end) {
                        continue;
                    }

                    // Fetch the device class to see if this is the correct instance
                    let device_class = match serial_proxy.get_class(zx::MonotonicInstant::INFINITE)
                    {
                        Ok(class) => class,
                        Err(_) => continue,
                    };

                    if device_class == fserial::Class::Mcu {
                        let serial_device = SerialDevice::new(
                            &kernel,
                            serial_proxy.into_channel().into(),
                            FsCred::root(),
                        )
                        .expect("Can create SerialDevice wrapper");

                        // TODO This will register with an incorrect device number. We should be
                        // dynamically registering a major device and this should be minor device 1
                        // of that major device.
                        let registry = &kernel.device_registry;
                        registry
                            .register_dyn_device(
                                &kernel,
                                "ttyHS1".into(),
                                registry.objects.tty_class(),
                                serial_device,
                            )
                            .expect("Can register serial device");
                        break;
                    }
                }
            }
            Ok(None) => {
                break;
            }
            Err(e) => {
                log_error!("Serial driver stream ended with error: {:}", e);
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_valid_datachannel_name;

    #[::fuchsia::test]
    fn validates_datachannel_identifier_names() {
        assert!(is_valid_datachannel_name("nanohub_ch0"));
        assert!(is_valid_datachannel_name("test_endpoint1"));

        for invalid_name in ["", ".", "..", "bad/name", "bad\0name"] {
            assert!(
                !is_valid_datachannel_name(invalid_name),
                "expected {invalid_name:?} to be rejected"
            );
        }
    }
}
