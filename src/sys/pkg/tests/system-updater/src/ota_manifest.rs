// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::*;
use fidl_fuchsia_update_installer_ext::{PrepareFailureReason, State};
use pretty_assertions::assert_eq;
use test_case::test_case;

#[test_case("blobs/1"; "relative")]
#[test_case("./blobs/1"; "current dir relative")]
#[test_case("/blobs/1"; "root relative")]
#[test_case("//fuchsia.com/blobs/1"; "no scheme")]
#[test_case("https://fuchsia.com/blobs/1"; "absolute")]
#[fasync::run_singlethreaded(test)]
async fn packageless_update_with_relative_blob_base_url(blob_base_url: &str) {
    let content_blob = vec![1; 200];
    let content_blob_hash = fuchsia_merkle::root_from_slice(&content_blob);
    let zbi_content = b"zbi contents";
    let zbi_hash = fuchsia_merkle::root_from_slice(zbi_content);

    let env = TestEnv::builder()
        .ota_manifest(OtaManifest {
            blob_base_url: blob_base_url.into(),
            images: vec![manifest::Image {
                slot: manifest::Slot::AB,
                image_type: manifest::ImageType::Asset(AssetType::Zbi),
                blob: manifest::Blob {
                    uncompressed_size: zbi_content.len() as u64,
                    fuchsia_merkle_root: zbi_hash,
                },
            }],
            ..make_manifest([manifest::Blob {
                uncompressed_size: content_blob.len() as u64,
                fuchsia_merkle_root: content_blob_hash,
            }])
        })
        .blob(content_blob_hash, content_blob)
        .blob(zbi_hash, zbi_content.to_vec())
        .build()
        .await;

    env.run_packageless_update().await.unwrap();

    env.assert_interactions(initial_interactions().chain([
        ReplaceRetainedBlobs(vec![zbi_hash.into(), content_blob_hash.into()]),
        Gc,
        Paver(PaverEvent::ReadAsset {
            configuration: paver::Configuration::B,
            asset: paver::Asset::Kernel,
        }),
        Paver(PaverEvent::ReadAsset {
            configuration: paver::Configuration::A,
            asset: paver::Asset::Kernel,
        }),
        OtaDownloader(OtaDownloaderEvent::FetchBlob(zbi_hash.into())),
        Paver(PaverEvent::WriteAsset {
            configuration: paver::Configuration::B,
            asset: paver::Asset::Kernel,
            payload: zbi_content.to_vec(),
        }),
        Paver(PaverEvent::DataSinkFlush),
        ReplaceRetainedBlobs(vec![content_blob_hash.into()]),
        Gc,
        OtaDownloader(OtaDownloaderEvent::FetchBlob(content_blob_hash.into())),
        BlobfsSync,
        Paver(PaverEvent::SetConfigurationActive { configuration: paver::Configuration::B }),
        Paver(PaverEvent::BootManagerFlush),
        Reboot,
    ]));
}

#[fasync::run_singlethreaded(test)]
async fn packageless_update_fails_with_wrong_signature() {
    let manifest = make_manifest([]);
    let key_pair = ring::signature::Ed25519KeyPair::from_seed_unchecked(&[1; 32]).unwrap();
    let bad_signed_manifest =
        ::update_package::signed_manifest::generate(manifest, &key_pair, &key_pair).unwrap();

    let env = TestEnv::builder().ota_manifest_raw(bad_signed_manifest).build().await;

    let mut attempt = start_update(
        &MANIFEST_URL.parse().unwrap(),
        default_options(),
        &env.installer_proxy(),
        None,
    )
    .await
    .unwrap();

    assert_eq!(attempt.next().await.unwrap().unwrap(), State::Prepare);
    assert_eq!(
        attempt.next().await.unwrap().unwrap(),
        State::FailPrepare(PrepareFailureReason::Internal)
    );
    assert_matches!(attempt.try_next().await, Ok(None));

    env.assert_interactions(initial_interactions());
}
