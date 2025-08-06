// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Context as _;
use assembled_system::{AssembledSystem, Image};
use assembly_partitions_config::PartitionsConfig;
use camino::Utf8PathBuf;
use delivery_blob::DeliveryBlobType;
use epoch::EpochFile;
use fuchsia_pkg::PackageManifest;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use update_package::manifest::{self, OtaManifestV1};

fn get_all_blobs(
    packages: &[(Option<Utf8PathBuf>, PackageManifest)],
) -> anyhow::Result<BTreeMap<fuchsia_merkle::Hash, u64>> {
    let mut blobs = BTreeMap::new();
    let mut subpackages_to_visit = Vec::new();

    for (_path, manifest) in packages {
        for blob in manifest.blobs() {
            blobs.insert(blob.merkle, blob.size);
        }
        subpackages_to_visit.extend(manifest.subpackages().iter().cloned());
    }

    let mut visited_subpackages = BTreeSet::new();
    while let Some(subpackage) = subpackages_to_visit.pop() {
        if visited_subpackages.contains(&subpackage.merkle) {
            continue;
        }
        visited_subpackages.insert(subpackage.merkle);

        let manifest = PackageManifest::try_load_from(&subpackage.manifest_path)
            .with_context(|| format!("parsing subpackage manifest {}", subpackage.manifest_path))?;
        for blob in manifest.blobs() {
            blobs.insert(blob.merkle, blob.size);
        }
        subpackages_to_visit.extend(manifest.subpackages().iter().cloned());
    }

    Ok(blobs)
}

/// Write the ota manifest to `out_path`.
pub fn write_ota_manifest(
    version_file: impl AsRef<std::path::Path>,
    epoch: &EpochFile,
    delivery_blob_type: DeliveryBlobType,
    system_a: &Option<AssembledSystem>,
    system_r: &Option<AssembledSystem>,
    partitions: &PartitionsConfig,
    packages_a: &[(Option<Utf8PathBuf>, PackageManifest)],
    out_path: impl AsRef<std::path::Path>,
) -> anyhow::Result<()> {
    let build_version = std::fs::read_to_string(version_file)
        .context("reading version file")?
        .parse()
        .context("parsing version file")?;
    let delivery_blob_type = delivery_blob_type.into();

    let mut images = vec![];
    let mut collect_images = |system: &AssembledSystem, slot| {
        let has_signed_zbi =
            system.images.iter().any(|image| matches!(image, Image::ZBI { signed: true, .. }));
        for image in &system.images {
            let (path, image_type) = match image {
                Image::ZBI { path, signed } => {
                    if has_signed_zbi && !signed {
                        continue;
                    }
                    (path, manifest::ImageType::Asset(update_package::images::AssetType::Zbi))
                }
                Image::VBMeta(path) => {
                    (path, manifest::ImageType::Asset(update_package::images::AssetType::Vbmeta))
                }
                Image::Dtbo(path) => (path, manifest::ImageType::Firmware("dtbo".into())),
                Image::BasePackage(_)
                | Image::BlobFS { .. }
                | Image::FVM(_)
                | Image::FVMFastboot(_)
                | Image::FVMSparse(_)
                | Image::Fxfs(_)
                | Image::FxfsSparse { .. }
                | Image::QemuKernel(_) => continue,
            };
            images.push(
                manifest::Image::from_path(path, slot, image_type, delivery_blob_type)
                    .with_context(|| format!("reading image: {path}"))?,
            );
        }
        anyhow::Ok(())
    };
    if let Some(system) = system_a {
        collect_images(system, manifest::Slot::AB)?;
    }

    if let Some(system) = system_r {
        match partitions.recovery_style().context("getting recovery style")? {
            assembly_partitions_config::RecoveryStyle::AB => {
                let has_signed_zbi = system
                    .images
                    .iter()
                    .any(|image| matches!(image, Image::ZBI { signed: true, .. }));
                for image in &system.images {
                    let (path, firmware_type) = match image {
                        Image::ZBI { path, signed } => {
                            if has_signed_zbi && !signed {
                                continue;
                            }
                            (path, "recovery_zbi")
                        }
                        Image::VBMeta(path) => (path, "recovery_vbmeta"),
                        Image::BasePackage(_)
                        | Image::BlobFS { .. }
                        | Image::Dtbo(_)
                        | Image::FVM(_)
                        | Image::FVMFastboot(_)
                        | Image::FVMSparse(_)
                        | Image::Fxfs(_)
                        | Image::FxfsSparse { .. }
                        | Image::QemuKernel(_) => continue,
                    };
                    images.push(
                        manifest::Image::from_path(
                            path,
                            manifest::Slot::AB,
                            manifest::ImageType::Firmware(firmware_type.into()),
                            delivery_blob_type,
                        )
                        .with_context(|| format!("reading image: {path}"))?,
                    );
                }
            }
            assembly_partitions_config::RecoveryStyle::R => {
                collect_images(system, manifest::Slot::R)?;
            }
            assembly_partitions_config::RecoveryStyle::NoRecovery => {
                anyhow::bail!("Has recovery images but no recovery partitions");
            }
        }
    }

    for bootloader in &partitions.bootloader_partitions {
        images.push(
            manifest::Image::from_path(
                &bootloader.image,
                manifest::Slot::AB,
                manifest::ImageType::Firmware(bootloader.partition_type.clone()),
                delivery_blob_type,
            )
            .with_context(|| format!("reading image: {:?}", bootloader.image))?,
        );
    }

    let blobs = get_all_blobs(packages_a)
        .context("getting all blobs from packages")?
        .into_iter()
        .map(|(merkle, size)| manifest::Blob {
            uncompressed_size: size,
            delivery_blob_type,
            fuchsia_merkle_root: merkle,
        })
        .collect();

    let manifest = OtaManifestV1 {
        build_version,
        board: partitions.hardware_revision.clone(),
        epoch: match epoch {
            EpochFile::Version1 { epoch } => *epoch,
        },
        mode: update_package::UpdateMode::Normal,
        // Relative to the OTA manifest URL.
        blob_base_url: "blobs".into(),
        images,
        blobs,
    };
    if let Some(parent) = out_path.as_ref().parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating directory {}", parent.display()))?;
    }
    let file = File::create(out_path).context("creating ota manifest")?;
    serde_json::to_writer(std::io::BufWriter::new(file), &manifest.into_versioned())
        .context("writing ota manifest")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use assembly_partitions_config::{BootloaderPartition, Partition, Slot as PartitionSlot};
    use assembly_release_info::SystemReleaseInfo;
    use camino::Utf8Path;
    use fuchsia_pkg::{
        BlobInfo, MetaPackage, PackageManifest, PackageManifestBuilder, SubpackageInfo,
    };
    use pretty_assertions::assert_eq;
    use std::io::Write as _;
    use tempfile::NamedTempFile;

    fn make_package(name: &str, blobs: impl IntoIterator<Item = BlobInfo>) -> PackageManifest {
        let meta_package = MetaPackage::from_name_and_variant_zero(name.parse().unwrap());
        let mut builder = PackageManifestBuilder::new(meta_package);
        for blob in blobs {
            builder = builder.add_blob(blob);
        }
        builder.build()
    }

    #[test]
    fn build_ota_manifest() {
        let mut version_file = NamedTempFile::new().unwrap();
        write!(version_file, "1.2.3.4").unwrap();

        let fake_zbi = NamedTempFile::new().unwrap();
        let fake_vbmeta = NamedTempFile::new().unwrap();
        let system_a = Some(AssembledSystem {
            images: vec![
                Image::ZBI { path: "unsigned zbi".to_owned().into(), signed: false },
                Image::ZBI {
                    path: Utf8Path::from_path(fake_zbi.path()).unwrap().to_path_buf(),
                    signed: true,
                },
                Image::VBMeta(Utf8Path::from_path(fake_vbmeta.path()).unwrap().to_path_buf()),
            ],
            board_name: "board".into(),
            partitions_config: None,
            system_release_info: SystemReleaseInfo::new_for_testing(),
        });

        let partitions = PartitionsConfig {
            bootstrap_partitions: vec![],
            unlock_credentials: vec![],
            bootloader_partitions: vec![],
            partitions: vec![
                Partition::ZBI { name: "zircon_a".into(), slot: PartitionSlot::A, size: None },
                Partition::VBMeta { name: "vbmeta_a".into(), slot: PartitionSlot::A, size: None },
            ],
            hardware_revision: "board".into(),
        };

        let meta_far_merkle = "0".repeat(64).parse().unwrap();
        let meta_far = BlobInfo {
            merkle: meta_far_merkle,
            size: 100,
            path: PackageManifest::META_FAR_BLOB_PATH.into(),
            source_path: "src_path0".into(),
        };
        let blob1_merkle = "1".repeat(64).parse().unwrap();
        let blob1 = BlobInfo {
            merkle: blob1_merkle,
            size: 200,
            path: "path1".into(),
            source_path: "src_path1".into(),
        };

        let pkg1 = make_package("pkg1", [meta_far, blob1]);

        let manifest_file = NamedTempFile::new().unwrap();
        write_ota_manifest(
            version_file.path(),
            &EpochFile::Version1 { epoch: 1 },
            DeliveryBlobType::Type1,
            &system_a,
            &None,
            &partitions,
            &[(None, pkg1)],
            manifest_file.path(),
        )
        .unwrap();

        let value: serde_json::Value = serde_json::from_reader(manifest_file).unwrap();
        let manifest: OtaManifestV1 = serde_json::from_value(value["version1"].clone()).unwrap();

        assert_eq!(manifest.build_version, "1.2.3.4".parse().unwrap());
        assert_eq!(manifest.epoch, 1);
        assert_eq!(manifest.board, "board");
        assert_eq!(manifest.mode, update_package::UpdateMode::Normal);
        assert_eq!(manifest.blob_base_url, "blobs");
        assert_eq!(manifest.images.len(), 2);
        assert_eq!(
            manifest.images[0],
            update_package::manifest::Image {
                slot: update_package::manifest::Slot::AB,
                image_type: manifest::ImageType::Asset(update_package::images::AssetType::Zbi),
                sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                    .parse()
                    .unwrap(),
                size: 0,
                delivery_blob_type: 1,
                fuchsia_merkle_root:
                    "15ec7bf0b50732b49f8228e07d24365338f9e3ab994b00af08e5a3bffe55fd8b"
                        .parse()
                        .unwrap(),
            }
        );
        assert_eq!(
            manifest.images[1],
            update_package::manifest::Image {
                slot: update_package::manifest::Slot::AB,
                image_type: update_package::manifest::ImageType::Asset(
                    update_package::images::AssetType::Vbmeta
                ),
                sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                    .parse()
                    .unwrap(),
                size: 0,
                delivery_blob_type: 1,
                fuchsia_merkle_root:
                    "15ec7bf0b50732b49f8228e07d24365338f9e3ab994b00af08e5a3bffe55fd8b"
                        .parse()
                        .unwrap(),
            }
        );
        assert_eq!(manifest.blobs.len(), 2);
        assert_eq!(
            manifest.blobs[0],
            update_package::manifest::Blob {
                uncompressed_size: 100,
                delivery_blob_type: 1,
                fuchsia_merkle_root: meta_far_merkle,
            }
        );
        assert_eq!(
            manifest.blobs[1],
            update_package::manifest::Blob {
                uncompressed_size: 200,
                delivery_blob_type: 1,
                fuchsia_merkle_root: blob1_merkle,
            }
        );
    }

    #[test]
    fn build_ota_manifest_with_subpackages_and_recovery() {
        let mut version_file = NamedTempFile::new().unwrap();
        write!(version_file, "1.2.3.4").unwrap();

        // System A images
        let fake_zbi_a = NamedTempFile::new().unwrap();
        let fake_vbmeta_a = NamedTempFile::new().unwrap();
        let fake_dtbo = NamedTempFile::new().unwrap();
        let system_a = Some(AssembledSystem {
            images: vec![
                Image::ZBI { path: "unsigned zbi".to_owned().into(), signed: false },
                Image::ZBI {
                    path: Utf8Path::from_path(fake_zbi_a.path()).unwrap().to_path_buf(),
                    signed: true,
                },
                Image::VBMeta(Utf8Path::from_path(fake_vbmeta_a.path()).unwrap().to_path_buf()),
                Image::Dtbo(Utf8Path::from_path(fake_dtbo.path()).unwrap().to_path_buf()),
            ],
            board_name: "board".into(),
            partitions_config: None,
            system_release_info: SystemReleaseInfo::new_for_testing(),
        });

        // System R images
        let fake_zbi_r = NamedTempFile::new().unwrap();
        let fake_vbmeta_r = NamedTempFile::new().unwrap();
        let system_r = Some(AssembledSystem {
            images: vec![
                Image::ZBI {
                    path: Utf8Path::from_path(fake_zbi_r.path()).unwrap().to_path_buf(),
                    signed: true,
                },
                Image::ZBI { path: "unsigned zbi".to_owned().into(), signed: false },
                Image::VBMeta(Utf8Path::from_path(fake_vbmeta_r.path()).unwrap().to_path_buf()),
            ],
            board_name: "board".into(),
            partitions_config: None,
            system_release_info: SystemReleaseInfo::new_for_testing(),
        });

        let fake_bootloader = NamedTempFile::new().unwrap();
        let partitions = PartitionsConfig {
            bootstrap_partitions: vec![],
            unlock_credentials: vec![],
            bootloader_partitions: vec![BootloaderPartition {
                partition_type: "bl_type".into(),
                name: Some("bootloader".into()),
                image: Utf8Path::from_path(fake_bootloader.path()).unwrap().to_path_buf(),
            }],
            partitions: vec![
                Partition::ZBI { name: "zircon_a".into(), slot: PartitionSlot::A, size: None },
                Partition::VBMeta { name: "vbmeta_a".into(), slot: PartitionSlot::A, size: None },
                Partition::Dtbo { name: "dtbo_a".into(), slot: PartitionSlot::A, size: None },
                Partition::ZBI { name: "zircon_r".into(), slot: PartitionSlot::R, size: None },
                Partition::VBMeta { name: "vbmeta_r".into(), slot: PartitionSlot::R, size: None },
            ],
            hardware_revision: "board".into(),
        };

        // Packages with subpackages
        let meta_far_merkle = "0".repeat(64).parse().unwrap();
        let meta_far = BlobInfo {
            merkle: meta_far_merkle,
            size: 100,
            path: PackageManifest::META_FAR_BLOB_PATH.into(),
            source_path: "src_path0".into(),
        };
        let blob1_merkle = "1".repeat(64).parse().unwrap();
        let blob1 = BlobInfo {
            merkle: blob1_merkle,
            size: 200,
            path: "path1".into(),
            source_path: "src_path1".into(),
        };

        let blob2_merkle = "2".repeat(64).parse().unwrap();
        let blob2 = BlobInfo {
            merkle: blob2_merkle,
            size: 300,
            path: "path2".into(),
            source_path: "src_path2".into(),
        };

        let mut subpackage_manifest_file = NamedTempFile::new().unwrap();
        let subpackage_manifest_path = subpackage_manifest_file.path().to_str().unwrap().to_owned();
        let subpackage_merkle = "3".repeat(64).parse().unwrap();
        let subpackage_metafar = BlobInfo {
            merkle: subpackage_merkle,
            size: 400,
            path: PackageManifest::META_FAR_BLOB_PATH.into(),
            source_path: subpackage_manifest_path.clone(),
        };
        let subpackage_manifest = make_package("subpkg", [subpackage_metafar, blob2]);
        serde_json::to_writer(&mut subpackage_manifest_file, &subpackage_manifest).unwrap();

        let meta_package = MetaPackage::from_name_and_variant_zero("pkg1".parse().unwrap());
        let subpackage_info = SubpackageInfo {
            name: "subpkg".to_string(),
            merkle: subpackage_merkle,
            manifest_path: subpackage_manifest_path,
        };
        let pkg1 = PackageManifestBuilder::new(meta_package)
            .add_blob(meta_far)
            .add_blob(blob1)
            .add_subpackage(subpackage_info)
            .build();

        let manifest_file = NamedTempFile::new().unwrap();
        write_ota_manifest(
            version_file.path(),
            &EpochFile::Version1 { epoch: 1 },
            DeliveryBlobType::Type1,
            &system_a,
            &system_r,
            &partitions,
            &[(None, pkg1)],
            manifest_file.path(),
        )
        .unwrap();

        let value: serde_json::Value = serde_json::from_reader(manifest_file).unwrap();
        let manifest: OtaManifestV1 = serde_json::from_value(value["version1"].clone()).unwrap();

        assert_eq!(manifest.build_version, "1.2.3.4".parse().unwrap());
        assert_eq!(manifest.epoch, 1);
        assert_eq!(manifest.board, "board");
        assert_eq!(manifest.images.len(), 6); // 3 from A, 2 from R, 1 bootloader
        assert_eq!(manifest.images[0].slot, update_package::manifest::Slot::AB);
        assert_eq!(
            manifest.images[0].image_type,
            manifest::ImageType::Asset(update_package::images::AssetType::Zbi),
        );
        assert_eq!(manifest.images[1].slot, update_package::manifest::Slot::AB);
        assert_eq!(
            manifest.images[1].image_type,
            update_package::manifest::ImageType::Asset(update_package::images::AssetType::Vbmeta),
        );
        assert_eq!(manifest.images[2].slot, update_package::manifest::Slot::AB);
        assert_eq!(manifest.images[2].image_type, manifest::ImageType::Firmware("dtbo".into()));
        assert_eq!(manifest.images[3].slot, update_package::manifest::Slot::R);
        assert_eq!(
            manifest.images[3].image_type,
            manifest::ImageType::Asset(update_package::images::AssetType::Zbi),
        );
        assert_eq!(manifest.images[4].slot, update_package::manifest::Slot::R);
        assert_eq!(
            manifest.images[4].image_type,
            update_package::manifest::ImageType::Asset(update_package::images::AssetType::Vbmeta),
        );
        assert_eq!(manifest.images[5].slot, update_package::manifest::Slot::AB);
        assert_eq!(manifest.images[5].image_type, manifest::ImageType::Firmware("bl_type".into()));

        assert_eq!(manifest.blobs.len(), 4);
        assert_eq!(
            manifest.blobs[0],
            update_package::manifest::Blob {
                uncompressed_size: 100,
                delivery_blob_type: 1,
                fuchsia_merkle_root: meta_far_merkle,
            }
        );
        assert_eq!(
            manifest.blobs[1],
            update_package::manifest::Blob {
                uncompressed_size: 200,
                delivery_blob_type: 1,
                fuchsia_merkle_root: blob1_merkle,
            }
        );
        assert_eq!(
            manifest.blobs[2],
            update_package::manifest::Blob {
                uncompressed_size: 300,
                delivery_blob_type: 1,
                fuchsia_merkle_root: blob2_merkle,
            }
        );
        assert_eq!(
            manifest.blobs[3],
            update_package::manifest::Blob {
                uncompressed_size: 400,
                delivery_blob_type: 1,
                fuchsia_merkle_root: subpackage_merkle,
            }
        );
    }

    #[test]
    fn build_ota_manifest_with_ab_recovery() {
        let mut version_file = NamedTempFile::new().unwrap();
        write!(version_file, "1.2.3.4").unwrap();

        // System A images
        let fake_zbi_a = NamedTempFile::new().unwrap();
        let fake_vbmeta_a = NamedTempFile::new().unwrap();
        let system_a = Some(AssembledSystem {
            images: vec![
                Image::ZBI {
                    path: Utf8Path::from_path(fake_zbi_a.path()).unwrap().to_path_buf(),
                    signed: false,
                },
                Image::VBMeta(Utf8Path::from_path(fake_vbmeta_a.path()).unwrap().to_path_buf()),
            ],
            board_name: "board".into(),
            partitions_config: None,
            system_release_info: SystemReleaseInfo::new_for_testing(),
        });

        // System R images
        let fake_zbi_r = NamedTempFile::new().unwrap();
        let fake_vbmeta_r = NamedTempFile::new().unwrap();
        let system_r = Some(AssembledSystem {
            images: vec![
                Image::ZBI { path: "unsigned zbi".to_owned().into(), signed: false },
                Image::ZBI {
                    path: Utf8Path::from_path(fake_zbi_r.path()).unwrap().to_path_buf(),
                    signed: true,
                },
                Image::VBMeta(Utf8Path::from_path(fake_vbmeta_r.path()).unwrap().to_path_buf()),
            ],
            board_name: "board".into(),
            partitions_config: None,
            system_release_info: SystemReleaseInfo::new_for_testing(),
        });

        let fake_bootloader = NamedTempFile::new().unwrap();
        let partitions = PartitionsConfig {
            bootstrap_partitions: vec![],
            unlock_credentials: vec![],
            bootloader_partitions: vec![BootloaderPartition {
                partition_type: "bl_type".into(),
                name: Some("bootloader".into()),
                image: Utf8Path::from_path(fake_bootloader.path()).unwrap().to_path_buf(),
            }],
            partitions: vec![
                Partition::ZBI { name: "zircon_a".into(), slot: PartitionSlot::A, size: None },
                Partition::VBMeta { name: "vbmeta_a".into(), slot: PartitionSlot::A, size: None },
                Partition::RecoveryZBI {
                    name: "recovery_zbi_a".into(),
                    slot: PartitionSlot::A,
                    size: None,
                },
                Partition::RecoveryVBMeta {
                    name: "recovery_vbmeta_a".into(),
                    slot: PartitionSlot::A,
                    size: None,
                },
            ],
            hardware_revision: "board".into(),
        };

        let manifest_file = NamedTempFile::new().unwrap();
        write_ota_manifest(
            version_file.path(),
            &EpochFile::Version1 { epoch: 1 },
            DeliveryBlobType::Type1,
            &system_a,
            &system_r,
            &partitions,
            &[],
            manifest_file.path(),
        )
        .unwrap();

        let value: serde_json::Value = serde_json::from_reader(manifest_file).unwrap();
        let manifest: OtaManifestV1 = serde_json::from_value(value["version1"].clone()).unwrap();

        assert_eq!(manifest.images.len(), 5);
        assert_eq!(manifest.blobs.len(), 0);
        assert_eq!(manifest.images[0].slot, update_package::manifest::Slot::AB);
        assert_eq!(
            manifest.images[0].image_type,
            manifest::ImageType::Asset(update_package::images::AssetType::Zbi),
        );
        assert_eq!(manifest.images[1].slot, update_package::manifest::Slot::AB);
        assert_eq!(
            manifest.images[1].image_type,
            update_package::manifest::ImageType::Asset(update_package::images::AssetType::Vbmeta),
        );
        assert_eq!(manifest.images[2].slot, update_package::manifest::Slot::AB);
        assert_eq!(
            manifest.images[2].image_type,
            manifest::ImageType::Firmware("recovery_zbi".into()),
        );
        assert_eq!(manifest.images[3].slot, update_package::manifest::Slot::AB);
        assert_eq!(
            manifest.images[3].image_type,
            manifest::ImageType::Firmware("recovery_vbmeta".into()),
        );
        assert_eq!(manifest.images[4].slot, update_package::manifest::Slot::AB);
        assert_eq!(manifest.images[4].image_type, manifest::ImageType::Firmware("bl_type".into()));
    }
}
