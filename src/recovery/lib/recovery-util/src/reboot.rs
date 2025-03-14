// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Error;
use async_trait::async_trait;
use fuchsia_component::client::connect_to_protocol;
#[cfg(test)]
use mockall::automock;
use zx::{MonotonicDuration, Status as zx_status};
use {fidl_fuchsia_hardware_power_statecontrol as powercontrol, fuchsia_async as fasync};

#[cfg_attr(test, automock)]
#[async_trait(?Send)]
pub trait RebootHandler {
    /// Request a reboot with optional delay in seconds. This is currently not cancellable and does not return an error result.
    /// The caller will be responsible for handling which thread to schedule this request on.
    async fn reboot(&self, delay_seconds: Option<u64>) -> Result<(), Error>;
}

#[derive(Default)]
pub struct RebootImpl;

impl RebootImpl {
    async fn request_reboot_with_proxy(
        &self,
        delay_seconds: Option<u64>,
        proxy: powercontrol::AdminProxy,
    ) -> Result<(), Error> {
        println!("Rebooting after {:?} seconds...", delay_seconds.unwrap_or(0));

        if let Some(delay) = delay_seconds {
            fasync::Timer::new(fasync::MonotonicInstant::after(MonotonicDuration::from_seconds(
                delay.try_into()?,
            )))
            .await;
        }

        // TODO(b/239569913): Update with a recovery-specific reboot reason.
        proxy
            .perform_reboot(&powercontrol::RebootOptions {
                reasons: Some(vec![powercontrol::RebootReason2::FactoryDataReset]),
                ..Default::default()
            })
            .await?
            .map_err(|e| zx_status::from_raw(e))?;
        Ok(())
    }
}

#[async_trait(?Send)]
impl RebootHandler for RebootImpl {
    async fn reboot(&self, delay_seconds: Option<u64>) -> Result<(), Error> {
        let proxy = connect_to_protocol::<powercontrol::AdminMarker>()?;
        self.request_reboot_with_proxy(delay_seconds, proxy).await
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use assert_matches::assert_matches;
    use fuchsia_async::TimeoutExt;
    use futures::channel::mpsc;
    use futures::{StreamExt, TryStreamExt};
    use {fidl_fuchsia_hardware_power_statecontrol as powercontrol, fuchsia_async as fasync};

    // Reboot tests - this functionality is only exercised in recovery OTA flows.
    fn create_mock_powercontrol_server(
    ) -> Result<(powercontrol::AdminProxy, mpsc::Receiver<powercontrol::RebootReason2>), Error>
    {
        let (mut sender, receiver) = mpsc::channel(1);
        let (proxy, mut request_stream) =
            fidl::endpoints::create_proxy_and_stream::<powercontrol::AdminMarker>();

        fasync::Task::local(async move {
            while let Some(request) =
                request_stream.try_next().await.expect("failed to read mock request")
            {
                match request {
                    powercontrol::AdminRequest::PerformReboot { options, responder } => {
                        let reason =
                            assert_matches!(&options.reasons.unwrap()[..], [reason] => *reason);
                        sender.start_send(reason).unwrap();
                        let result: powercontrol::AdminPerformRebootResult = { Ok(()) };
                        responder.send(result).ok();
                    }
                    _ => {
                        panic!("Mock server not configured to handle request");
                    }
                }
            }
        })
        .detach();

        Ok((proxy, receiver))
    }

    #[fuchsia::test]
    async fn test_reboot_reason_no_delay() {
        let (proxy, mut receiver) = create_mock_powercontrol_server().unwrap();

        let reboot = RebootImpl::default();
        reboot.request_reboot_with_proxy(None, proxy).await.unwrap();

        let reboot_reason =
            receiver.next().on_timeout(MonotonicDuration::from_seconds(5), || None).await.unwrap();

        assert_eq!(reboot_reason, powercontrol::RebootReason2::FactoryDataReset);
    }

    #[fuchsia::test]
    async fn test_reboot_with_delay() {
        let delay_seconds = 1;
        let (proxy, mut receiver) = create_mock_powercontrol_server().unwrap();

        let start_time = fasync::MonotonicInstant::now();
        let reboot = RebootImpl::default();
        reboot.request_reboot_with_proxy(Some(delay_seconds), proxy).await.unwrap();

        let reboot_reason =
            receiver.next().on_timeout(MonotonicDuration::from_seconds(5), || None).await.unwrap();

        let end_time = fasync::MonotonicInstant::now();

        assert!((end_time - start_time).into_seconds() >= delay_seconds.try_into().unwrap());
        assert_eq!(reboot_reason, powercontrol::RebootReason2::FactoryDataReset);
    }
}
