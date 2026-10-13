// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::*;
use assert_matches::assert_matches;
use fidl_fuchsia_update_installer_ext::{State, StateId};
use pretty_assertions::assert_eq;

#[fuchsia::test]
async fn cancel_update() {
    let env = TestEnv::builder()
        .crash_reporter(MockCrashReporterService::new(|report| {
            panic!("unexpected crash report: {report:?}")
        }))
        .build()
        .await;

    let mut handle_update_pkg = env.resolver.url(UPDATE_PKG_URL).block_once();

    let mut attempt = env.start_update().await.unwrap();
    assert_eq!(attempt.next().await.unwrap().unwrap(), State::Prepare);
    handle_update_pkg.wait().await;

    env.installer_proxy().cancel_update(None).await.unwrap().unwrap();
    assert_eq!(attempt.next().await.unwrap().unwrap(), State::Canceled);
    assert_matches!(attempt.next().await, None);

    assert_eq!(
        env.get_ota_metrics().await,
        OtaMetrics {
            initiator:
                metrics::OtaResultAttemptsMigratedMetricDimensionInitiator::UserInitiatedCheck
                    as u32,
            phase: metrics::OtaResultAttemptsMigratedMetricDimensionPhase::Tufupdate as u32,
            status_code: metrics::OtaResultAttemptsMigratedMetricDimensionStatusCode::Canceled
                as u32,
        }
    );
}

#[fuchsia::test]
async fn cancel_update_packageless() {
    let env = TestEnv::builder()
        .ota_manifest(make_manifest([]))
        .crash_reporter(MockCrashReporterService::new(|report| {
            panic!("unexpected crash report: {report:?}")
        }))
        .build()
        .await;

    let handle_ota_manifest = env.http_loader_service.block_once();

    let mut attempt = env.start_packageless_update().await.unwrap();
    assert_eq!(attempt.next().await.unwrap().unwrap(), State::Prepare);

    let _resume_handle = handle_ota_manifest.await.unwrap();

    env.installer_proxy().cancel_update(None).await.unwrap().unwrap();
    assert_eq!(attempt.next().await.unwrap().unwrap(), State::Canceled);
    assert_matches!(attempt.next().await, None);

    assert_eq!(
        env.get_ota_metrics().await,
        OtaMetrics {
            initiator:
                metrics::OtaResultAttemptsMigratedMetricDimensionInitiator::UserInitiatedCheck
                    as u32,
            phase: metrics::OtaResultAttemptsMigratedMetricDimensionPhase::Tufupdate as u32,
            status_code: metrics::OtaResultAttemptsMigratedMetricDimensionStatusCode::Canceled
                as u32,
        }
    );
}

#[fuchsia::test]
async fn cannot_cancel_during_commit() {
    let (throttle_hook, throttler) = mphooks::throttle();
    let env = TestEnv::builder()
        .paver_service(|builder| builder.insert_hook(throttle_hook))
        .build()
        .await;

    env.resolver
        .register_package("update", "upd4t3")
        .add_file("packages.json", make_packages_json([]))
        .add_file("images.json", make_images_json_zbi())
        .add_file("epoch.json", make_current_epoch_json());

    // Emit all paver events before SetConfigurationActive so the update blocks in commit_images.
    throttler.emit_next_paver_events(&[
        PaverEvent::QueryCurrentConfiguration,
        PaverEvent::ReadAsset {
            configuration: paver::Configuration::A,
            asset: paver::Asset::VerifiedBootMetadata,
        },
        PaverEvent::ReadAsset {
            configuration: paver::Configuration::A,
            asset: paver::Asset::Kernel,
        },
        PaverEvent::QueryCurrentConfiguration,
        PaverEvent::QueryConfigurationStatus { configuration: paver::Configuration::A },
        PaverEvent::SetConfigurationUnbootable { configuration: paver::Configuration::B },
        PaverEvent::BootManagerFlush,
        PaverEvent::ReadAsset {
            configuration: paver::Configuration::B,
            asset: paver::Asset::Kernel,
        },
        PaverEvent::DataSinkFlush,
    ]);

    let mut attempt = env.start_update().await.unwrap();
    assert_eq!(attempt.next().await.unwrap().unwrap(), State::Prepare);
    assert_eq!(attempt.next().await.unwrap().unwrap().id(), StateId::Stage);
    assert_eq!(attempt.next().await.unwrap().unwrap().id(), StateId::Stage);
    assert_eq!(attempt.next().await.unwrap().unwrap().id(), StateId::Fetch);
    assert_eq!(attempt.next().await.unwrap().unwrap().id(), StateId::Commit);

    assert_eq!(
        env.installer_proxy().cancel_update(None).await.unwrap(),
        Err(finstaller::CancelError::UpdateCannotBeCanceled)
    );

    drop(throttler);

    assert_eq!(attempt.next().await.unwrap().unwrap().id(), StateId::WaitToReboot);
    assert_eq!(attempt.next().await.unwrap().unwrap().id(), StateId::Reboot);
    assert_matches!(attempt.next().await, None);
}
