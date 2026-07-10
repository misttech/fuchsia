// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![recursion_limit = "128"]

mod config;
mod device_server;
mod provider_server;

use crate::config::Config;
use crate::device_server::{serve_application_passthrough, serve_device_info_passthrough};
use anyhow::{Context as _, Error, Result, format_err};
use fidl::endpoints::{DiscoverableProtocolMarker, ServerEnd};
use fidl_fuchsia_hardware_tee::DeviceConnectorProxy;
use fidl_fuchsia_tee::{self as fuchsia_tee, DeviceInfoMarker};
use fuchsia_component::client::Service;
use fuchsia_component::server::ServiceFs;
use futures::prelude::*;
use futures::select;
use futures::stream::FusedStream;
use uuid::Uuid;

enum IncomingRequest {
    Application(ServerEnd<fuchsia_tee::ApplicationMarker>, fuchsia_tee::Uuid),
    DeviceInfo(ServerEnd<fuchsia_tee::DeviceInfoMarker>),
}

#[fuchsia::main(logging_tags = ["tee_manager"])]
async fn main() -> Result<(), Error> {
    let service = Service::open(fidl_fuchsia_hardware_tee::ServiceMarker)
        .context("Failed to open TEE service")?;

    let current_instances =
        service.clone().enumerate().await.context("Failed to enumerate TEE service")?;
    match current_instances.len() {
        0 => return Err(format_err!("No TEE devices found")),
        1 => {} // OK
        _ => {
            // Cannot handle more than one TEE device.
            // If this becomes supported, Manager will need to provide a method for clients to
            // enumerate and select a device to connect to.
            return Err(format_err!(
                "Found more than 1 TEE device - this is currently not supported"
            ));
        }
    }

    let mut instances = service.watch().await.context("Failed to watch TEE service")?;

    let dev_connector_proxy = match instances.next().await {
        Some(Ok(instance)) => instance
            .connect_to_device_connector()
            .context("Failed to connect to TEE DeviceConnector")?,
        Some(Err(e)) => return Err(e),
        None => return Err(format_err!("TEE service watcher closed unexpectedly")),
    };

    let mut fs = ServiceFs::new_local();
    fs.dir("svc").add_service_at(DeviceInfoMarker::PROTOCOL_NAME, |channel| {
        Some(IncomingRequest::DeviceInfo(ServerEnd::new(channel)))
    });

    match Config::from_file() {
        Ok(config) => {
            for app_uuid in config.application_uuids {
                let service_name = format!("fuchsia.tee.Application.{}", app_uuid.as_hyphenated());
                log::debug!("Serving {}", service_name);
                let fidl_uuid = uuid_to_fuchsia_tee_uuid(&app_uuid);
                fs.dir("svc").add_service_at(service_name, move |channel| {
                    Some(IncomingRequest::Application(ServerEnd::new(channel), fidl_uuid))
                });
            }
        }
        Err(e) => {
            log::error!("Failed to load config: {:?}", e);
        }
    }

    fs.take_and_serve_directory_handle()?;

    serve(dev_connector_proxy, fs.fuse(), instances).await
}

async fn serve<I, P>(
    dev_connector_proxy: DeviceConnectorProxy,
    service_stream: impl Stream<Item = IncomingRequest> + FusedStream + Unpin,
    instances: I,
) -> Result<(), Error>
where
    I: Stream<Item = Result<P, Error>> + Unpin,
{
    let mut device_fut = dev_connector_proxy.take_event_stream().into_future();
    let mut service_fut = service_stream.for_each_concurrent(None, |request| async {
        match request {
            IncomingRequest::Application(channel, uuid) => {
                log::trace!("Connecting application: {:?}", uuid);
                let result =
                    serve_application_passthrough(uuid, dev_connector_proxy.clone(), channel).await;
                if let Err(e) = result {
                    log::error!("Error serving application: {:?}", e);
                }
            }
            IncomingRequest::DeviceInfo(channel) => {
                let result =
                    serve_device_info_passthrough(dev_connector_proxy.clone(), channel).await;
                if let Err(e) = result {
                    log::error!("Error serving device info: {:?}", e);
                }
            }
        }
    });

    let mut instances = instances.fuse();
    let mut instances_fut = instances.next();

    select! {
        _ = service_fut => Ok(()),
        _ = device_fut => Err(format_err!("TEE DeviceConnector closed unexpectedly")),
        maybe_instance = instances_fut => {
            match maybe_instance {
                Some(Ok(_instance)) => Err(format_err!("Found more than 1 TEE device")),
                Some(Err(e)) => Err(e),
                None => {
                    Err(format_err!("TEE service watcher closed unexpectedly"))
                }
            }
        }
    }
}

/// Converts a `uuid::Uuid` to a `fidl_fuchsia_tee::Uuid`.
fn uuid_to_fuchsia_tee_uuid(uuid: &Uuid) -> fuchsia_tee::Uuid {
    let (time_low, time_mid, time_hi_and_version, clock_seq_and_node) = uuid.as_fields();

    fuchsia_tee::Uuid {
        time_low,
        time_mid,
        time_hi_and_version,
        clock_seq_and_node: *clock_seq_and_node,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl::endpoints;
    use fidl_fuchsia_hardware_tee::{
        DeviceConnectorMarker, DeviceConnectorProxy, DeviceConnectorRequest,
    };
    use fidl_fuchsia_io as fio;
    use fidl_fuchsia_tee::ApplicationMarker;
    use fidl_fuchsia_tee_manager::ProviderProxy;
    use fuchsia_async as fasync;
    use futures::channel::mpsc;
    use zx_status::Status;

    fn spawn_device_connector<F>(
        request_handler: impl Fn(DeviceConnectorRequest) -> F + 'static,
    ) -> DeviceConnectorProxy
    where
        F: Future<Output = ()> + 'static,
    {
        let (proxy, mut stream) = endpoints::create_proxy_and_stream::<DeviceConnectorMarker>();

        fasync::Task::local(async move {
            while let Some(Ok(request)) = stream.next().await {
                request_handler(request).await;
            }
        })
        .detach();

        proxy
    }

    fn get_storage(provider_proxy: &ProviderProxy) -> fio::DirectoryProxy {
        let (proxy, server_end) = endpoints::create_proxy::<fio::DirectoryMarker>();
        assert!(provider_proxy.request_persistent_storage(server_end).is_ok());
        proxy
    }

    fn is_closed_with_status(error: fidl::Error, status: Status) -> bool {
        match error {
            fidl::Error::ClientChannelClosed { status: s, .. } => s == status,
            _ => false,
        }
    }

    async fn assert_is_valid_storage(storage_proxy: &fio::DirectoryProxy) {
        assert_eq!(
            storage_proxy.query().await.unwrap(),
            fio::DirectoryMarker::PROTOCOL_NAME.as_bytes()
        );
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_serve_device_info() {
        let dev_connector = spawn_device_connector(|request| async move {
            match request {
                DeviceConnectorRequest::ConnectToDeviceInfo {
                    device_info_request,
                    control_handle: _,
                } => {
                    assert!(!device_info_request.channel().is_invalid());
                    device_info_request
                        .close_with_epitaph(Status::OK)
                        .expect("Unable to close device_info_request");
                }
                _ => unreachable!("Unexpected request"),
            }
        });

        let (mut sender, receiver) = mpsc::channel::<IncomingRequest>(1);

        fasync::Task::local(async move {
            let result = serve(
                dev_connector,
                receiver.fuse(),
                futures::stream::pending::<Result<(), anyhow::Error>>(),
            )
            .await;
            assert!(result.is_ok(), "{}", result.unwrap_err());
        })
        .detach();

        let (device_info_proxy, device_info_server) = endpoints::create_proxy::<DeviceInfoMarker>();

        sender
            .send(IncomingRequest::DeviceInfo(device_info_server))
            .await
            .expect("Unable to send DeviceInfo Request");

        let (result, _) = device_info_proxy.take_event_stream().into_future().await;
        assert!(is_closed_with_status(result.unwrap().unwrap_err(), Status::OK));
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_serve_application() {
        let app_uuid = uuid_to_fuchsia_tee_uuid(
            &Uuid::parse_str("8aaaf200-2450-11e4-abe2-0002a5d5c51b").unwrap(),
        );

        let dev_connector = spawn_device_connector(move |request| async move {
            match request {
                DeviceConnectorRequest::ConnectToApplication {
                    application_uuid,
                    service_provider,
                    application_request,
                    control_handle: _,
                } => {
                    assert_eq!(application_uuid, app_uuid);
                    assert!(service_provider.is_some());
                    assert!(!application_request.channel().is_invalid());

                    let provider_proxy = service_provider.unwrap().into_proxy();

                    assert_is_valid_storage(&get_storage(&provider_proxy)).await;

                    application_request
                        .close_with_epitaph(Status::OK)
                        .expect("Unable to close tee_request");
                }
                _ => unreachable!("Unexpected request"),
            }
        });

        let (mut sender, receiver) = mpsc::channel::<IncomingRequest>(1);

        fasync::Task::local(async move {
            let result = serve(
                dev_connector,
                receiver.fuse(),
                futures::stream::pending::<Result<(), anyhow::Error>>(),
            )
            .await;
            assert!(result.is_ok(), "{}", result.unwrap_err());
        })
        .detach();

        let (app_proxy, app_server) = endpoints::create_proxy::<ApplicationMarker>();

        sender
            .send(IncomingRequest::Application(app_server, app_uuid))
            .await
            .expect("Unable to send Application Request");

        let (result, _) = app_proxy.take_event_stream().into_future().await;
        assert!(is_closed_with_status(result.unwrap().unwrap_err(), Status::OK));
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_serve_error() {
        let (dev_connector_proxy, server_end) = endpoints::create_proxy::<DeviceConnectorMarker>();

        server_end
            .close_with_epitaph(Status::PEER_CLOSED)
            .expect("Could not close DeviceConnector ServerEnd");

        let (mut sender, receiver) = mpsc::channel::<IncomingRequest>(1);
        let (client_proxy, client_server_end) = endpoints::create_proxy::<DeviceInfoMarker>();

        sender.send(IncomingRequest::DeviceInfo(client_server_end)).await.unwrap();

        fasync::Task::local(async move {
            let result = serve(
                dev_connector_proxy,
                receiver.fuse(),
                futures::stream::pending::<Result<(), anyhow::Error>>(),
            )
            .await;
            assert!(result.is_err());
        })
        .detach();

        let result = client_proxy.get_os_info().await;
        assert!(result.is_err());
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_tee_device_closed() {
        let (dev_connector_proxy, dev_connector_server) =
            fidl::endpoints::create_proxy::<DeviceConnectorMarker>();
        let (_sender, receiver) = mpsc::channel::<IncomingRequest>(1);

        dev_connector_server
            .close_with_epitaph(Status::PEER_CLOSED)
            .expect("Could not close DeviceConnector ServerEnd");
        let result = serve(
            dev_connector_proxy,
            receiver.fuse(),
            futures::stream::pending::<Result<(), anyhow::Error>>(),
        )
        .await;
        assert!(result.is_err());
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_multiple_tee_devices() {
        let (dev_connector_proxy, _dev_connector_server) =
            fidl::endpoints::create_proxy::<DeviceConnectorMarker>();
        let (_sender, receiver) = mpsc::channel::<IncomingRequest>(1);

        let instances_stream = futures::stream::iter(vec![Ok::<(), anyhow::Error>(())]);

        let result = serve(dev_connector_proxy, receiver.fuse(), instances_stream).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().to_string(), "Found more than 1 TEE device");
    }
}
