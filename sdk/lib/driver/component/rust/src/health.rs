// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Support for running a driver's [`crate::Driver::health_check`] on behalf of the lifecycle
//! server: the per-level deadlines and the root dispatcher liveness check that precedes every
//! check.

use fdf::{AsAsyncDispatcherRef, DriverDispatcherRef};
use fidl_next_fuchsia_driver_framework::{
    HealthCheckError, HealthCheckRequest, HealthCheckResponse,
};
use futures::channel::oneshot;

use crate::Driver;

/// The result of a single health check.
pub type HealthCheckResult = Result<HealthCheckResponse, HealthCheckError>;

// Per-level health check deadlines. These mirror the deadlines driver-host enforces for each
// `HealthCheckRequest`; the server adds `HEALTH_CHECK_TIMEOUT_MARGIN` so the host's own TIMEOUT is
// reported first, and the margin only bounds how long a hung check (or wedged root dispatcher) can
// keep its future alive on the driver side.
const PING_HEALTH_CHECK_DEADLINE: zx::MonotonicDuration = zx::MonotonicDuration::from_millis(250);
const FAST_HEALTH_CHECK_DEADLINE: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(2);
const DEEP_HEALTH_CHECK_DEADLINE: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(120);
const HEALTH_CHECK_TIMEOUT_MARGIN: zx::MonotonicDuration = zx::MonotonicDuration::from_millis(100);

/// Returns how long the server lets a check at `level` run before answering `TIMEOUT` itself,
/// or `None` for unknown flexible enum levels.
fn health_check_timeout(level: HealthCheckRequest) -> Option<zx::MonotonicDuration> {
    let deadline = match level {
        HealthCheckRequest::Ping => PING_HEALTH_CHECK_DEADLINE,
        HealthCheckRequest::Fast => FAST_HEALTH_CHECK_DEADLINE,
        HealthCheckRequest::Deep => DEEP_HEALTH_CHECK_DEADLINE,
        _ => return None,
    };
    Some(deadline + HEALTH_CHECK_TIMEOUT_MARGIN)
}

/// Confirms that `root_dispatcher` is still processing tasks.
///
/// The lifecycle server runs on a dedicated fuchsia-async thread, not the root dispatcher, so
/// unlike the C++ server (whose hook runs on the root dispatcher and therefore proves its liveness
/// implicitly) it must ping the dispatcher explicitly. If the dispatcher is wedged this never
/// completes, and the caller's deadline turns that into a `TIMEOUT`.
async fn ping_root_dispatcher(
    root_dispatcher: DriverDispatcherRef<'static>,
) -> Result<(), HealthCheckError> {
    let (tx, rx) = oneshot::channel();
    root_dispatcher
        .post_task_sync(move |status| {
            if status.is_ok() {
                let _ = tx.send(());
            }
        })
        .map_err(|_| HealthCheckError::InvalidState)?;
    // The sender is dropped without sending if the dispatcher is shutting down.
    rx.await.map_err(|_| HealthCheckError::InvalidState)
}

/// Runs one health check to completion: confirms `root_dispatcher` is live, then invokes
/// [`Driver::health_check`], bounded by the per-level deadline.
pub(crate) async fn run_health_check<D: Driver>(
    driver: &D,
    root_dispatcher: DriverDispatcherRef<'static>,
    request: HealthCheckRequest,
) -> HealthCheckResult {
    let Some(timeout) = health_check_timeout(request) else {
        return Err(HealthCheckError::NotSupported);
    };
    let check = async move {
        ping_root_dispatcher(root_dispatcher).await?;
        driver.health_check(request).await
    };
    fuchsia_async::TimeoutExt::on_timeout(check, timeout, || Err(HealthCheckError::Timeout)).await
}
