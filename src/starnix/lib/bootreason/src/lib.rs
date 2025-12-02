// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Error;
use async_lock::OnceCell;
use fidl_fuchsia_feedback::{LastRebootInfoProviderMarker, RebootReason};
use fidl_fuchsia_io as fio;
use fuchsia_component::client::connect_to_protocol_sync;
use fuchsia_fs::node::OpenError;
use log::{debug, info};
use zx_status::Status;

/// Temp file for the Starnix lifecycle detection
const STARTED_ONCE: &str = "component-started-once";

/// Timeout for FIDL calls to LastRebootInfoProvider
const LRIP_FIDL_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::INFINITE;
static ANDROID_BOOTREASON: OnceCell<Result<String, Error>> = OnceCell::new();

/// Get an Android-compatible boot reason suitable to add to the cmdline or bootconfig.
pub async fn get_android_bootreason(
    dir: Option<fio::DirectoryProxy>,
) -> &'static Result<String, Error> {
    ANDROID_BOOTREASON.get_or_init(async || update_android_bootreason(dir).await).await
}

/// Update the Android bootreason.
///
/// Called only once when the Starnix kernel is initialized.
/// If called more than once, it reports false Android crash cases to Pitot.
async fn update_android_bootreason(dir: Option<fio::DirectoryProxy>) -> Result<String, Error> {
    if let Some(dir) = dir {
        match fuchsia_fs::directory::open_file(&dir, STARTED_ONCE, fio::Flags::FLAG_MUST_CREATE)
            .await
        {
            Ok(_file) => (),
            Err(OpenError::OpenError(Status::ALREADY_EXISTS)) => {
                info!("Session restart observed, set android bootreason to kernel_panic.");
                return Ok("kernel_panic".to_string());
            }
            Err(err) => {
                info!(
                    "Failed to generate the file with err {err:#?}. Continue with LastRebootInfo."
                );
            }
        }
    }

    info!("Converting LastRebootInfo to an android-friendly bootreason.");
    let reboot_info_proxy = connect_to_protocol_sync::<LastRebootInfoProviderMarker>()?;
    let deadline = zx::MonotonicInstant::after(LRIP_FIDL_TIMEOUT);
    let reboot_info = reboot_info_proxy.get(deadline)?;

    let bootreason = match reboot_info.reason {
        Some(RebootReason::Unknown) => "reboot,unknown",
        Some(RebootReason::Cold) => "reboot,cold",
        Some(RebootReason::BriefPowerLoss) => "reboot,hard_reset",
        Some(RebootReason::Brownout) => "reboot,undervoltage",
        Some(RebootReason::KernelPanic) => "kernel_panic",
        Some(RebootReason::SystemOutOfMemory) => "kernel_panic,oom",
        Some(RebootReason::HardwareWatchdogTimeout) => "watchdog",
        Some(RebootReason::SoftwareWatchdogTimeout) => "watchdog,sw",
        Some(RebootReason::RootJobTermination) => "kernel_panic",
        Some(RebootReason::UserRequest) => "reboot,userrequested",
        Some(RebootReason::DeveloperRequest) => "reboot,shell",
        Some(RebootReason::RetrySystemUpdate) => "reboot,ota",
        Some(RebootReason::HighTemperature) => "shutdown,thermal",
        Some(RebootReason::SessionFailure) => "kernel_panic",
        Some(RebootReason::SysmgrFailure) => "kernel_panic",
        Some(RebootReason::FactoryDataReset) => "reboot,factory_reset",
        Some(RebootReason::CriticalComponentFailure) => "kernel_panic",
        Some(RebootReason::ZbiSwap) => "reboot,normal",
        Some(RebootReason::SystemUpdate) => "reboot,ota",
        Some(RebootReason::NetstackMigration) => "reboot,normal",
        Some(RebootReason::AndroidUnexpectedReason) => "reboot,normal",
        Some(RebootReason::AndroidRescueParty) => "reboot,rescueparty",
        Some(RebootReason::AndroidCriticalProcessFailure) => "reboot,userspace_failed",
        Some(RebootReason::__SourceBreaking { .. }) => "reboot,normal",
        None => "reboot,unknown",
    };
    Ok(bootreason.to_string())
}

/// Get the last reboot reason code.
fn get_reboot_reason() -> Option<u16> {
    let reboot_info_proxy = connect_to_protocol_sync::<LastRebootInfoProviderMarker>().ok();
    let deadline = zx::MonotonicInstant::after(LRIP_FIDL_TIMEOUT);
    let reboot_info = reboot_info_proxy?.get(deadline);
    match reboot_info {
        Ok(info) => match info.reason {
            Some(r) => Some(r.into_primitive()),
            None => {
                info!("Failed to get the reboot reason.");
                Some(RebootReason::unknown().into_primitive())
            }
        },
        Err(e) => {
            info!("Failed to get the reboot info: {:?}", e);
            Some(RebootReason::unknown().into_primitive())
        }
    }
}

/// Get contents for the pstore/console-ramoops* file.
///
/// In Linux it contains a limited amount of some of the previous boot's kernel logs.
/// The ramoops won't be created after a normal reboot.
pub fn get_console_ramoops() -> Option<Vec<u8>> {
    debug!("Getting console-ramoops contents");
    match ANDROID_BOOTREASON.wait_blocking() {
        Ok(reason) => match reason.as_str() {
            "kernel_panic" | "watchdog" | "watchdog,sw" => {
                Some(format!("Last Reboot Reason: {}\n", get_reboot_reason()?).as_bytes().to_vec())
                // TODO: Log additional crash signature.
            }
            _ => None,
        },
        Err(e) => {
            info!("Failed to get android bootreason for console_ramoops: {:?}", e);
            None
        }
    }
}
