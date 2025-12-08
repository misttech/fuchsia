// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A helper for constructing the vbmeta image.

use crate::extra_hash_descriptor::ExtraHashDescriptor;
use crate::vfs::{FilesystemProvider, RealFilesystemProvider};
use crate::{AssembledSystem, Image};
use anyhow::{Context, Result};
use assembly_images_config::VBMeta;
use camino::{Utf8Path, Utf8PathBuf};
use std::path::Path;
use utf8_path::path_relative_from_current_dir;
use vbmeta::{Descriptor, HashDescriptor, Key, Salt, VBMeta as VBMetaImage};

/// The conventional name for the partition name of the hash descriptor in a
/// VBMeta assembled for Fuchsia.
pub const FUCHSIA_HASH_DESCRIPTOR_NAME: &str = "zircon";

/// Construct the vbmeta image.
pub fn construct_vbmeta(
    assembled_system: &mut AssembledSystem,
    outdir: impl AsRef<Utf8Path>,
    vbmeta_config: &VBMeta,
    zbi: impl AsRef<Path>,
) -> Result<Utf8PathBuf> {
    // Generate the salt.
    let salt = Salt::random()?;

    // Collect the descriptors.
    let fs = RealFilesystemProvider {};
    let mut descriptors = descriptors_for_fuchsia(zbi, salt.clone(), &fs)
        .context("constructing VBMeta descriptors")?;
    for hash in &vbmeta_config.additional_descriptors {
        descriptors.push(Descriptor::Hash(
            ExtraHashDescriptor {
                name: Some(hash.name.clone()),
                size: Some(hash.size),
                salt: None,
                digest: None,
                flags: Some(hash.flags),
                min_avb_version: None, //Some(d.min_avb_version),
            }
            .into(),
        ));
    }

    // Sign the image and construct a VBMeta.
    let (vbmeta, _salt) = crate::vbmeta::sign(
        descriptors,
        &vbmeta_config.key,
        &vbmeta_config.key_metadata,
        salt,
        &fs,
    )
    .context("signing vbmeta")?;

    // Write VBMeta to a file and return the path.
    let vbmeta_path = outdir.as_ref().join(format!("{}.vbmeta", vbmeta_config.name));
    std::fs::write(&vbmeta_path, vbmeta.as_bytes())
        .with_context(|| format!("writing vbmeta: {}", &vbmeta_path))?;
    let vbmeta_path_relative = path_relative_from_current_dir(&vbmeta_path)
        .with_context(|| format!("calculating relative path for: {}", &vbmeta_path))?;
    assembled_system.images.push(Image::VBMeta(vbmeta_path_relative.clone()));
    Ok(vbmeta_path_relative)
}

/// The descriptors for a VBMeta assembled for Fuchsia.
fn descriptors_for_fuchsia<FSP: FilesystemProvider>(
    zbi_path: impl AsRef<Path>,
    salt: Salt,
    fs: &FSP,
) -> Result<Vec<Descriptor>> {
    // Read the image into memory, so that it can be hashed.
    let zbi = fs
        .read(&zbi_path)
        .with_context(|| format!("reading ZBI: {}", zbi_path.as_ref().display()))?;

    // Create the descriptor for the image.
    let descriptor =
        Descriptor::Hash(HashDescriptor::new(FUCHSIA_HASH_DESCRIPTOR_NAME, &zbi, salt));
    Ok(vec![descriptor])
}

fn sign<FSP: FilesystemProvider>(
    descriptors: Vec<Descriptor>,
    key: impl AsRef<Path>,
    key_metadata: impl AsRef<Path>,
    salt: Salt,
    fs: &FSP,
) -> Result<(VBMetaImage, Salt)> {
    // Read the signing key's bytes and metadata.
    let key_pem = fs
        .read_to_string(&key)
        .with_context(|| format!("reading key: {}", key.as_ref().display()))?;
    let key_metadata = fs
        .read(&key_metadata)
        .with_context(|| format!("reading key metadata: {}", key_metadata.as_ref().display()))?;
    // And then create the signing key from those.
    let key = Key::try_new(&key_pem, key_metadata).unwrap();

    // And do the signing operation itself.
    VBMetaImage::sign(descriptors, key).map_err(Into::into).map(|vbmeta| (vbmeta, salt))
}

#[cfg(test)]
mod tests {
    use super::{FUCHSIA_HASH_DESCRIPTOR_NAME, construct_vbmeta, descriptors_for_fuchsia, sign};

    use crate::AssembledSystem;
    use crate::vbmeta::{Descriptor, HashDescriptor, Key, Salt};
    use crate::vfs::mock::MockFilesystemProvider;

    use assembly_images_config::VBMeta;
    use assembly_release_info::SystemReleaseInfo;
    use camino::Utf8Path;
    use tempfile::tempdir;
    use utf8_path::path_relative_from_current_dir;

    #[test]
    fn construct() {
        let tmp = tempdir().unwrap();
        let dir = Utf8Path::from_path(tmp.path()).unwrap();

        let key_path = dir.join("key");
        let metadata_path = dir.join("key_metadata");
        std::fs::write(&key_path, test_keys::ATX_TEST_KEY).unwrap();
        std::fs::write(&metadata_path, test_keys::TEST_RSA_4096_PEM).unwrap();

        let vbmeta_config = VBMeta {
            name: "fuchsia".into(),
            key: key_path,
            key_metadata: metadata_path,
            additional_descriptors: vec![],
        };

        // Create a fake zbi.
        let zbi_path = dir.join("fuchsia.zbi");
        std::fs::write(&zbi_path, "fake zbi").unwrap();

        let mut assembled_system = AssembledSystem {
            images: Default::default(),
            board_name: "my_board".into(),
            partitions_config: None,
            system_release_info: SystemReleaseInfo::new_for_testing(),
        };
        let vbmeta_path =
            construct_vbmeta(&mut assembled_system, dir, &vbmeta_config, zbi_path).unwrap();
        assert_eq!(
            vbmeta_path,
            path_relative_from_current_dir(dir.join("fuchsia.vbmeta")).unwrap()
        );
    }

    #[test]
    fn fuchsia_descriptors() {
        let mut vfs = MockFilesystemProvider::new();
        vfs.add("zbi", &[0x00u8; 128]);
        vfs.add("salt", hex::encode([0xAAu8; 32]).as_bytes());

        let salt = Salt::try_from(&[0xAAu8; 32][..]).unwrap();

        let descriptors = descriptors_for_fuchsia("zbi", salt.clone(), &vfs).unwrap();

        // Validate that there's only the one descriptor.
        assert_eq!(descriptors.len(), 1);

        let Descriptor::Hash(hash) = &descriptors[0] else {
            panic!("descriptor is not a hash descriptor!?: {:#?}", descriptors[0]);
        };

        // Validate that the salt was the one from the args.
        assert_eq!(salt, hash.salt().unwrap());

        // The partition name should be the conventional Fuchsia one.
        assert_eq!(FUCHSIA_HASH_DESCRIPTOR_NAME, hash.image_name());

        // Validate that the digest is the expected one, based on the image that
        // was provided in the arguments.
        let expected_digest =
            hex::decode("caeaacb8208cfd8d214de6baef8d535f6fce499524c60aa5dcd2fce7043a9700")
                .unwrap();
        assert_eq!(Some(expected_digest.as_ref()), hash.digest());
    }

    #[test]
    fn sign_vbmeta() {
        let key_expected =
            Key::try_new(test_keys::TEST_RSA_4096_PEM, "TEST_METADATA".as_bytes()).unwrap();

        let mut vfs = MockFilesystemProvider::new();
        vfs.add("key", test_keys::TEST_RSA_4096_PEM.as_bytes());
        vfs.add("key_metadata", &b"TEST_METADATA"[..]);

        let salt = Salt::try_from(&[0xAAu8; 32][..]).unwrap();
        let descriptor =
            Descriptor::Hash(HashDescriptor::new("image_name", &[0xBB; 32], salt.clone()));

        let (vbmeta, salt) =
            sign(vec![descriptor.clone()], "key", "key_metadata", salt, &vfs).unwrap();

        // Validate that the key in the arguments was the key that was passed to
        // the vbmeta library for the signing operation.
        assert_eq!(vbmeta.key().public_key().as_ref() as &[u8], key_expected.public_key().as_ref());

        // Validate that there's only the one descriptor.
        let descriptors = vbmeta.descriptors();
        assert_eq!(descriptors.len(), 1);
        assert_eq!(descriptor, descriptors[0]);

        assert_eq!(salt.bytes, [0xAAu8; 32]); // the salt from the args.
    }
}
