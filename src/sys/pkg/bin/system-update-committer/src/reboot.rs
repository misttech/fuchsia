// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{anyhow, Context};
use fidl_fuchsia_hardware_power_statecontrol::{
    AdminProxy as PowerStateControlProxy, RebootOptions, RebootReason2,
};
use fuchsia_async as fasync;
use log::error;
use zx::Status;

/// Waits for a timer to fire and then reboots the system, logging errors instead of failing.
pub(super) async fn wait_and_reboot(timer: fasync::Timer, proxy: &PowerStateControlProxy) {
    let () = timer.await;
    if let Err(e) = async move {
        proxy
            .perform_reboot(&RebootOptions {
                reasons: Some(vec![RebootReason2::RetrySystemUpdate]),
                ..Default::default()
            })
            .await
            .context("while performing reboot call")?
            .map_err(Status::from_raw)
            .context("reboot responded with")
    }
    .await
    {
        error!("error initiating reboot: {:#}", anyhow!(e));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuchsia_sync::Mutex;
    use futures::channel::oneshot;
    use futures::pin_mut;
    use futures::task::Poll;
    use mock_reboot::MockRebootService;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn test_wait_and_reboot_success() {
        let mut executor = fasync::TestExecutor::new_with_fake_time();

        // Create a mock reboot service.
        let (sender, recv) = oneshot::channel();
        let sender = Arc::new(Mutex::new(Some(sender)));
        let mock = Arc::new(MockRebootService::new(Box::new(move |options: RebootOptions| {
            sender.lock().take().unwrap().send(options).unwrap();
            Ok(())
        })));
        let proxy = mock.spawn_reboot_service();

        // Prepare futures to call reboot and receive the reboot request.
        let timer_duration = 5;
        let reboot_fut =
            wait_and_reboot(fasync::Timer::new(Duration::from_secs(timer_duration)), &proxy);
        pin_mut!(reboot_fut);
        pin_mut!(recv);

        // Set the time so that the timer is still going, so we should neither call reboot nor
        // observe the reboot service was called.
        executor.set_fake_time(fasync::MonotonicInstant::after(
            Duration::from_secs(timer_duration - 1).into(),
        ));
        assert!(!executor.wake_expired_timers());
        match executor.run_until_stalled(&mut reboot_fut) {
            Poll::Ready(res) => panic!("future unexpectedly completed with response: {res:?}"),
            Poll::Pending => {}
        };
        match executor.run_until_stalled(&mut recv) {
            Poll::Ready(res) => panic!("future unexpectedly completed with response: {res:?}"),
            Poll::Pending => {}
        };

        // Once the timer completes, we should complete the reboot call and observe we called the
        // reboot service with the given reboot reason.
        executor.set_fake_time(fasync::MonotonicInstant::after(Duration::from_secs(1).into()));
        assert!(executor.wake_expired_timers());
        match executor.run_until_stalled(&mut recv) {
            Poll::Ready(res) => panic!("future unexpectedly completed with response: {res:?}"),
            Poll::Pending => {}
        };
        match executor.run_until_stalled(&mut reboot_fut) {
            Poll::Ready(_) => {}
            Poll::Pending => panic!("future unexpectedly pending"),
        };
        match executor.run_until_stalled(&mut recv) {
            Poll::Ready(res) => assert_eq!(
                res,
                Ok(RebootOptions {
                    reasons: Some(vec![RebootReason2::RetrySystemUpdate]),
                    ..Default::default()
                })
            ),
            Poll::Pending => panic!("future unexpectedly pending"),
        };
    }
}
