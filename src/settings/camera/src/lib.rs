// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use anyhow::{Error, format_err};
use fidl::endpoints::create_proxy;
use fidl_fuchsia_camera3::{
    DeviceMarker, DeviceProxy as Camera3DeviceProxy, DeviceWatcherMarker,
    DeviceWatcherProxy as Camera3DeviceWatcherProxy, WatchDevicesEvent,
};
use fuchsia_async::{self as fasync, DurationExt};
use futures::FutureExt;
use futures::future::Fuse;
use settings_common::inspect::event::ExternalEventPublisher;
use settings_common::service_context::{ExternalServiceProxy, ServiceContext};
use settings_common::{call, call_async};
use zx::MonotonicDuration;

/// The amount of time in milliseconds to wait for a camera device to be detected.
pub const CAMERA_WATCHER_TIMEOUT: i64 = 30_000;

/// A proxy to `fuchsia.camera3.DeviceWatcher`.
pub type CameraWatcherProxy =
    ExternalServiceProxy<Camera3DeviceWatcherProxy, ExternalEventPublisher>;

/// Events reported by the camera watcher agent to clients of the camera software mute state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CameraWatcherEvent {
    /// The aggregate software mute state of the watched cameras. The cameras are muted only if
    /// all connected cameras are muted.
    SoftwareMuted(bool),
    /// A camera was reported as added after initial enumeration. Its software mute state starts
    /// unmuted in the camera driver, so the stored state may need to be applied to it. The
    /// camera's mute state is reported afterwards.
    DeviceAdded,
}

/// Retrieves the ids of all camera devices given the camera device watcher proxy.
async fn get_camera_ids(camera_watcher_proxy: &CameraWatcherProxy) -> Result<Vec<u64>, Error> {
    // Get a list of id structs containing existing, new, and removed ids.

    // Sets a timer and watches for changes from the camera api. If the first response is empty,
    // continue to watch for an update to the devices. If we receive a nonempty response,
    // we extract the ids and return. If the timeout is reached, then it is assumed to be an error.
    let timer =
        fasync::Timer::new(MonotonicDuration::from_millis(CAMERA_WATCHER_TIMEOUT).after_now())
            .fuse();
    let camera_ids = call_async!(camera_watcher_proxy => watch_devices()).fuse();

    // Used to add the second watch call if the first comes back with empty devices.
    let unfulfilled_future = Fuse::terminated();

    futures::pin_mut!(timer, camera_ids, unfulfilled_future);
    loop {
        futures::select! {
            ids_result = camera_ids => {
                let ids = ids_result?;
                match extract_cam_ids(ids) {
                    Ok(cam_ids) => return Ok(cam_ids),
                    Err(_) => {
                        // The camera list might not have active cameras yet; make another watch
                        // call and keep waiting.
                        let next_camera_ids =
                            call_async!(camera_watcher_proxy => watch_devices()).fuse();
                        unfulfilled_future.set(next_camera_ids);
                    }
                }
            }
            ids_result_second = unfulfilled_future => {
                let ids = ids_result_second?;
                return extract_cam_ids(ids);
            }
            _ = timer => {
                return Err(format_err!("Could not find a camera"));
            }
        }
    }
}

/// Extracts all present camera ids from the list of device watcher events.
///
/// Multiple cameras are not yet supported individually, and individual camera mute/unmute is not
/// exposed, so all active camera ids are returned to be muted and watched together.
fn extract_cam_ids(events: Vec<WatchDevicesEvent>) -> Result<Vec<u64>, Error> {
    let ids: Vec<u64> = events
        .into_iter()
        .filter_map(|event| match event {
            WatchDevicesEvent::Existing(id) | WatchDevicesEvent::Added(id) => Some(id),
            WatchDevicesEvent::Removed(_) => None,
        })
        .collect();
    if ids.is_empty() { Err(format_err!("Could not find a camera")) } else { Ok(ids) }
}

/// Establishes connections to all `fuchsia.camera3.Device` instances by watching
/// the camera ids and using them to connect to the devices.
///
/// Multiple cameras are not yet supported individually, and individual camera mute/unmute is not
/// exposed, so all reported cameras are connected.
pub async fn connect_to_cameras(
    service_context: &ServiceContext,
    external_publisher: ExternalEventPublisher,
) -> Result<Vec<Camera3DeviceProxy>, Error> {
    let (_camera_watcher_proxy, cameras) =
        connect_to_cameras_with_watcher(service_context, external_publisher).await?;
    Ok(cameras.into_iter().map(|(_, camera_proxy)| camera_proxy).collect())
}

/// Like `connect_to_cameras`, but also returns the camera device watcher connection, which can
/// be used to watch for cameras that are added or removed later, and the id of each camera.
pub async fn connect_to_cameras_with_watcher(
    service_context: &ServiceContext,
    external_publisher: ExternalEventPublisher,
) -> Result<(CameraWatcherProxy, Vec<(u64, Camera3DeviceProxy)>), Error> {
    // Connect to the camera device watcher to get camera ids. This will
    // be used to connect to the cameras.
    let camera_watcher_proxy = service_context
        .connect_with_publisher::<DeviceWatcherMarker, _>(external_publisher)
        .await?;
    let camera_ids = get_camera_ids(&camera_watcher_proxy).await?;

    // Connect to each camera device with the found ids.
    let mut cameras = Vec::with_capacity(camera_ids.len());
    for camera_id in camera_ids {
        cameras.push((camera_id, connect_to_camera(&camera_watcher_proxy, camera_id)?));
    }
    Ok((camera_watcher_proxy, cameras))
}

/// Connects to the camera device with the given id.
pub fn connect_to_camera(
    camera_watcher_proxy: &CameraWatcherProxy,
    camera_id: u64,
) -> Result<Camera3DeviceProxy, Error> {
    let (camera_proxy, device_server) = create_proxy::<DeviceMarker>();
    call!(camera_watcher_proxy => connect_to_device(camera_id, device_server)).map_err(|e| {
        format_err!("Could not connect to fuchsia.camera3.DeviceWatcher device {camera_id}: {e:?}")
    })?;
    Ok(camera_proxy)
}

/// Waits for the next change to the set of camera devices reported by `camera_watcher_proxy`.
pub async fn watch_camera_changes(
    camera_watcher_proxy: &CameraWatcherProxy,
) -> Result<Vec<WatchDevicesEvent>, Error> {
    call_async!(camera_watcher_proxy => watch_devices())
        .await
        .map_err(|e| format_err!("Could not watch camera devices: {e:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    fn test_extract_cam_ids_single_camera() {
        assert_eq!(extract_cam_ids(vec![WatchDevicesEvent::Added(1)]).unwrap(), vec![1]);
        assert_eq!(extract_cam_ids(vec![WatchDevicesEvent::Existing(42)]).unwrap(), vec![42]);
    }

    #[fuchsia::test]
    fn test_extract_cam_ids_multiple_cameras() {
        let events = vec![
            WatchDevicesEvent::Existing(1),
            WatchDevicesEvent::Added(2),
            WatchDevicesEvent::Removed(3),
            WatchDevicesEvent::Added(4),
        ];
        assert_eq!(extract_cam_ids(events).unwrap(), vec![1, 2, 4]);
    }

    #[fuchsia::test]
    fn test_extract_cam_ids_no_active_cameras_returns_err() {
        assert!(extract_cam_ids(vec![]).is_err());
        assert!(extract_cam_ids(vec![WatchDevicesEvent::Removed(1)]).is_err());
    }
}
