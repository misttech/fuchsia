// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A helper for constructing the vbmeta image.

use crate::base_package::BasePackage;
use crate::extra_hash_descriptor::ExtraHashDescriptor;
use crate::vfs::{FilesystemProvider, RealFilesystemProvider};
use anyhow::{Context, Result, anyhow};
use assembly_config_schema::BuildType;
use assembly_images_config::{VBMeta, VBMetaStyle};
use camino::{Utf8Path, Utf8PathBuf};
use std::path::Path;
use utf8_path::path_relative_from_current_dir;
use vbmeta::{
    Descriptor, HashDescriptor, KernelCmdlineDescriptor, Key, PropertyDescriptor, Salt,
    VBMeta as VBMetaImage,
};

/// The conventional name for the partition name of the hash descriptor in a
/// VBMeta assembled for Fuchsia.
pub const FUCHSIA_HASH_DESCRIPTOR_NAME: &str = "zircon";

/// The Android Virtualization Framework expects a VBMeta with a hash
/// descriptor of the kernel (i.e., the boot shim in our case) with this
/// partition name.
const PVM_HASH_DESCRIPTOR_NAME_KERNEL: &str = "boot";

/// The Android Virtualization Framework expects a hash descriptor of the
/// ramdisk (i.e, the ZBI in our case) with one of two possible partition
/// names. This name enables a debug mode in which the guest starts in an
/// environment with the following properties:
///
/// * The UART MMIO is already shared with the host, which is something that
///   naturally should not be shared in a non-debug context (as it leaks info
///   to the host). All other MMIO and memory remains in the unshared state by
///   default.
///
/// * The DICE/certificate chain encodes debug-ness, signaling to remote
///   attestation services that the guest is not secure.
///
const PVM_HASH_DESCRIPTOR_NAME_RAMDISK_DEBUG: &str = "initrd_debug";

/// The Android Virtualization Framework expects a hash descriptor of the
/// ramdisk (i.e, the ZBI in our case) with one of two possible partition
/// names. This name enables the normal, production mode in which all memory
/// and MMIO starts unshared and the provided DICE chain verifiable as secure.
const PVM_HASH_DESCRIPTOR_NAME_RAMDISK_NORMAL: &str = "initrd_normal";

/// A property known to the Android Virtualization Framework that parameterizes
/// verified booting behaviour.
const PVM_PROP_VIRT_CAP_KEY: &str = "com.android.virt.cap";

/// Despite the name, this value is a general signal to skip rollback
/// protection, which we do not have a scheme for at this time. See
/// https://android.googlesource.com/platform/packages/modules/Virtualization/+/refs/heads/main/guest/pvmfw/#vbmeta-properties
const PVM_PROP_VIRT_CAP_VALUE: &str = "trusty_security_vm";

/// The property key for the base merkle in the VBMeta kernel command line.
const BASE_MERKLE_CMDLINE_KEY: &str = "system.base_merkle";

/// Represents a constructed VBMeta in one of the two supported forms.
#[derive(Debug)]
pub enum ConstructedVBMeta {
    /// The path to a normal, standalone, Fuchsia VBMeta.
    Standalone(Utf8PathBuf),

    /// The path to a system VBMeta image.
    VBMetaSystem(Utf8PathBuf),

    /// The path to a copy of the assembled system's QEMU kernel with a VBMeta
    /// footer appended.
    QemuKernelWithFooter(Utf8PathBuf),
}

/// Construct the vbmeta image.
pub fn construct_vbmeta(
    outdir: impl AsRef<Utf8Path>,
    vbmeta_config: &VBMeta,
    zbi: impl AsRef<Path>,
    boot_shim: impl AsRef<Utf8Path>,
    build_type: BuildType,
    base_package: Option<&BasePackage>,
) -> Result<ConstructedVBMeta> {
    // Generate the salt.
    let salt = Salt::random()?;

    // Collect the descriptors and rollback indices. We do not really use
    // rollback indices, but it is an unwritten requirement of Android pVMs
    // that the index still be positive, even when rollback protection is
    // skipped. So we pass 1, the canonically positive number in that case, and
    // 0 otherwise.
    let fs = RealFilesystemProvider {};
    let (mut descriptors, rollback_index) = match vbmeta_config.style {
        VBMetaStyle::Fuchsia | VBMetaStyle::VBMetaSystem => {
            let descriptors = descriptors_for_fuchsia(zbi, salt.clone(), base_package, &fs)
                .context("constructing VBMeta descriptors")?;
            (descriptors, 0)
        }
        pvm_style => {
            let debug = pvm_style == VBMetaStyle::AndroidPvmDebug
                || (pvm_style == VBMetaStyle::AndroidPvmAuto && build_type != BuildType::User);
            let descriptors = descriptors_for_pvm(
                zbi,
                boot_shim.as_ref().as_std_path(),
                salt.clone(),
                debug,
                &fs,
            )
            .context("constructing VBMeta descriptors")?;
            (descriptors, 1)
        }
    };
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

    let (vbmeta, _salt) = crate::vbmeta::sign(
        descriptors,
        &vbmeta_config.key,
        &vbmeta_config.key_metadata,
        salt,
        rollback_index,
        &fs,
    )
    .context("signing vbmeta")?;

    // For a Fuchsia-style VBMeta, we simply write it out as a standalone image.
    if vbmeta_config.style == VBMetaStyle::Fuchsia
        || vbmeta_config.style == VBMetaStyle::VBMetaSystem
    {
        let vbmeta_path = outdir.as_ref().join(format!("{}.vbmeta", vbmeta_config.name));
        std::fs::write(&vbmeta_path, vbmeta.as_bytes())
            .with_context(|| format!("writing vbmeta: {}", vbmeta_path))?;
        let vbmeta_path_relative = path_relative_from_current_dir(&vbmeta_path)
            .with_context(|| format!("calculating relative path for: {}", vbmeta_path))?;

        if vbmeta_config.style == VBMetaStyle::VBMetaSystem {
            Ok(ConstructedVBMeta::VBMetaSystem(vbmeta_path_relative))
        } else {
            Ok(ConstructedVBMeta::Standalone(vbmeta_path_relative))
        }
    } else {
        // At this point we are handling an Android pVM-style VBMeta, which gets
        // appended to the boot shim. We do not want to modify the original boot
        // shim in place, so we write the version with the footer as a separate
        // file and update our accounting to point to it.
        let boot_shim_name = boot_shim
            .as_ref()
            .file_name()
            .ok_or_else(|| anyhow!("calculating base name for {}", boot_shim.as_ref()))?;
        let new_boot_shim = outdir.as_ref().join(boot_shim_name);
        vbmeta.append_as_footer(&boot_shim, &new_boot_shim)?;

        let new_boot_shim_relative = path_relative_from_current_dir(&new_boot_shim)
            .with_context(|| format!("calculating relative path for: {}", new_boot_shim))?;
        Ok(ConstructedVBMeta::QemuKernelWithFooter(new_boot_shim_relative))
    }
}

/// The descriptors for a VBMeta assembled for Fuchsia.
fn descriptors_for_fuchsia<FSP: FilesystemProvider>(
    zbi_path: impl AsRef<Path>,
    salt: Salt,
    base_package: Option<&BasePackage>,
    fs: &FSP,
) -> Result<Vec<Descriptor>> {
    // Read the image into memory, so that it can be hashed.
    let zbi = fs
        .read(&zbi_path)
        .with_context(|| format!("reading ZBI: {}", zbi_path.as_ref().display()))?;

    // Create the descriptor for the image.
    let descriptor =
        Descriptor::Hash(HashDescriptor::new(FUCHSIA_HASH_DESCRIPTOR_NAME, &zbi, salt));

    let mut descriptors = vec![descriptor];

    if let Some(bp) = base_package {
        descriptors.push(Descriptor::KernelCmdline(KernelCmdlineDescriptor::new(
            0,
            format!("{}={}", BASE_MERKLE_CMDLINE_KEY, bp.merkle),
        )));
    }

    Ok(descriptors)
}

/// The descriptors for a VBMeta assembled for an Android pVM.
fn descriptors_for_pvm<FSP: FilesystemProvider>(
    zbi_path: impl AsRef<Path>,
    boot_shim_path: impl AsRef<Path>,
    salt: Salt,
    debug: bool,
    fs: &FSP,
) -> Result<Vec<Descriptor>> {
    // The required "boot" hash descriptor.
    let boot_shim = fs
        .read(&boot_shim_path)
        .with_context(|| format!("reading boot shim: {}", boot_shim_path.as_ref().display()))?;
    let boot_shim_desc = Descriptor::Hash(HashDescriptor::new(
        PVM_HASH_DESCRIPTOR_NAME_KERNEL,
        &boot_shim,
        salt.clone(),
    ));

    // The required "initrd_*" hash descriptor.
    let zbi = fs
        .read(&zbi_path)
        .with_context(|| format!("reading ZBI: {}", zbi_path.as_ref().display()))?;
    let zbi_desc_name = if debug {
        PVM_HASH_DESCRIPTOR_NAME_RAMDISK_DEBUG
    } else {
        PVM_HASH_DESCRIPTOR_NAME_RAMDISK_NORMAL
    };
    let zbi_desc = Descriptor::Hash(HashDescriptor::new(zbi_desc_name, &zbi, salt));

    // Our signal to skip rollback protection.
    let prop_desc = Descriptor::Property(PropertyDescriptor::new(
        PVM_PROP_VIRT_CAP_KEY.to_string(),
        PVM_PROP_VIRT_CAP_VALUE.to_string(),
    ));
    Ok(vec![boot_shim_desc, zbi_desc, prop_desc])
}

fn sign<FSP: FilesystemProvider>(
    descriptors: Vec<Descriptor>,
    key: impl AsRef<Path>,
    key_metadata: &Option<impl AsRef<Path>>,
    salt: Salt,
    rollback_index: u64,
    fs: &FSP,
) -> Result<(VBMetaImage, Salt)> {
    // Read the signing key's bytes and metadata.
    let key_pem = fs
        .read_to_string(&key)
        .with_context(|| format!("reading key: {}", key.as_ref().display()))?;
    let key_metadata = match key_metadata {
        Some(metadata_path) => fs.read(metadata_path.as_ref()).with_context(|| {
            format!("reading key metadata: {}", metadata_path.as_ref().display())
        })?,
        None => Vec::new(),
    };
    // And then create the signing key from those.
    let key = Key::try_new(&key_pem, key_metadata).unwrap();

    // And do the signing operation itself.
    VBMetaImage::sign_with_rollback(descriptors, key, rollback_index)
        .map_err(Into::into)
        .map(|vbmeta| (vbmeta, salt))
}

#[cfg(test)]
mod tests {
    use super::{
        BASE_MERKLE_CMDLINE_KEY, ConstructedVBMeta, Descriptor, FUCHSIA_HASH_DESCRIPTOR_NAME,
        HashDescriptor, Key, PVM_HASH_DESCRIPTOR_NAME_KERNEL,
        PVM_HASH_DESCRIPTOR_NAME_RAMDISK_DEBUG, PVM_HASH_DESCRIPTOR_NAME_RAMDISK_NORMAL,
        PVM_PROP_VIRT_CAP_KEY, PVM_PROP_VIRT_CAP_VALUE, Salt, construct_vbmeta,
        descriptors_for_fuchsia, descriptors_for_pvm, sign,
    };

    use crate::base_package::BasePackage;
    use crate::vfs::mock::MockFilesystemProvider;

    use assembly_config_schema::BuildType;
    use assembly_images_config::{VBMeta, VBMetaStyle};
    use camino::{Utf8Path, Utf8PathBuf};
    use fuchsia_hash::Hash;
    use tempfile::tempdir;
    use utf8_path::path_relative_from_current_dir;

    #[test]
    fn construct_fuchsia_style() {
        let tmp = tempdir().unwrap();
        let dir = Utf8Path::from_path(tmp.path()).unwrap();

        let key_path = dir.join("key");
        let metadata_path = dir.join("key_metadata");
        std::fs::write(&key_path, test_keys::ATX_TEST_KEY).unwrap();
        std::fs::write(&metadata_path, test_keys::TEST_RSA_4096_PEM).unwrap();

        let vbmeta_config = VBMeta {
            style: VBMetaStyle::Fuchsia,
            name: "fuchsia".into(),
            key: key_path,
            key_metadata: Some(metadata_path),
            additional_descriptors: vec![],
        };

        // Create a fake zbi.
        let zbi_path = dir.join("fuchsia.zbi");
        std::fs::write(&zbi_path, "fake zbi").unwrap();

        let vbmeta = construct_vbmeta(
            dir,
            &vbmeta_config,
            zbi_path,
            Utf8PathBuf::new(),
            BuildType::Eng,
            None,
        )
        .unwrap();

        let ConstructedVBMeta::Standalone(vbmeta_path) = vbmeta else {
            panic!("Expected standalone VBMeta image; got {vbmeta:#?}");
        };
        assert_eq!(
            vbmeta_path,
            path_relative_from_current_dir(dir.join("fuchsia.vbmeta")).unwrap()
        );
    }

    #[test]
    fn construct_fuchsia_style_with_base_merkle() {
        let tmp = tempdir().unwrap();
        let dir = Utf8Path::from_path(tmp.path()).unwrap();

        let key_path = dir.join("key");
        let metadata_path = dir.join("key_metadata");
        std::fs::write(&key_path, test_keys::ATX_TEST_KEY).unwrap();
        std::fs::write(&metadata_path, test_keys::TEST_RSA_4096_PEM).unwrap();

        let vbmeta_config = VBMeta {
            style: VBMetaStyle::Fuchsia,
            name: "fuchsia".into(),
            key: key_path,
            key_metadata: Some(metadata_path),
            additional_descriptors: vec![],
        };

        // Create a fake zbi.
        let zbi_path = dir.join("fuchsia.zbi");
        std::fs::write(&zbi_path, "fake zbi").unwrap();

        // Create a fake base merkle.
        let base_merkle = Hash::from([0xAA; 32]);
        let base_package = BasePackage {
            merkle: base_merkle,
            manifest_path: Utf8PathBuf::from("path/to/manifest"),
        };

        let vbmeta = construct_vbmeta(
            dir,
            &vbmeta_config,
            zbi_path,
            Utf8PathBuf::new(),
            BuildType::Eng,
            Some(&base_package),
        )
        .unwrap();

        let ConstructedVBMeta::Standalone(vbmeta_path) = vbmeta else {
            panic!("Expected standalone VBMeta image; got {vbmeta:#?}");
        };
        assert_eq!(
            vbmeta_path,
            path_relative_from_current_dir(dir.join("fuchsia.vbmeta")).unwrap()
        );
    }

    #[test]
    fn construct_pvm_style() {
        let tmp = tempdir().unwrap();
        let dir = Utf8Path::from_path(tmp.path()).unwrap();

        let key_path = dir.join("key");
        std::fs::write(&key_path, test_keys::ATX_TEST_KEY).unwrap();

        let zbi_path = dir.join("fuchsia.zbi");
        std::fs::write(&zbi_path, "fake zbi").unwrap();

        let boot_shim_path = dir.join("boot-shim.bin");
        std::fs::write(&boot_shim_path, "fake boot shim").unwrap();

        const PVM_STYLES: [VBMetaStyle; 3] = [
            VBMetaStyle::AndroidPvmAuto,
            VBMetaStyle::AndroidPvmDebug,
            VBMetaStyle::AndroidPvmNormal,
        ];
        const BUILD_TYPES: [BuildType; 3] = [BuildType::Eng, BuildType::User, BuildType::UserDebug];

        for style in PVM_STYLES {
            for build_type in BUILD_TYPES {
                let vbmeta_config = VBMeta {
                    style,
                    name: String::new(),
                    key: key_path.clone(),
                    key_metadata: None,
                    additional_descriptors: vec![],
                };

                let vbmeta = construct_vbmeta(
                    dir,
                    &vbmeta_config,
                    &zbi_path,
                    &boot_shim_path,
                    build_type,
                    None,
                )
                .unwrap();

                let ConstructedVBMeta::QemuKernelWithFooter(new_boot_shim_path) = vbmeta else {
                    panic!("Expected standalone VBMeta image; got {vbmeta:#?}");
                };

                assert_eq!(
                    new_boot_shim_path,
                    path_relative_from_current_dir(dir.join("boot-shim.bin")).unwrap()
                );
            }
        }
    }

    #[test]
    fn fuchsia_descriptors() {
        let mut vfs = MockFilesystemProvider::new();
        vfs.add("zbi", &[0x00u8; 128]);
        vfs.add("salt", hex::encode([0xAAu8; 32]).as_bytes());

        let salt = Salt::try_from(&[0xAAu8; 32][..]).unwrap();

        let descriptors = descriptors_for_fuchsia("zbi", salt.clone(), None, &vfs).unwrap();

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
    fn fuchsia_descriptors_with_base_merkle() {
        let mut vfs = MockFilesystemProvider::new();
        vfs.add("zbi", &[0x00u8; 128]);
        vfs.add("salt", hex::encode([0xAAu8; 32]).as_bytes());

        let salt = Salt::try_from(&[0xAAu8; 32][..]).unwrap();

        let base_merkle = Hash::from([0xBB; 32]);
        let base_package = BasePackage {
            merkle: base_merkle,
            manifest_path: Utf8PathBuf::from("path/to/manifest"),
        };

        let descriptors =
            descriptors_for_fuchsia("zbi", salt.clone(), Some(&base_package), &vfs).unwrap();

        // Validate that there are two descriptors.
        assert_eq!(descriptors.len(), 2);

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

        let Descriptor::KernelCmdline(cmdline) = &descriptors[1] else {
            panic!("descriptor is not a kernel cmdline descriptor!?: {:#?}", descriptors[1]);
        };

        assert_eq!(cmdline.flags, 0);
        assert_eq!(cmdline.kernel_cmdline, format!("{}={}", BASE_MERKLE_CMDLINE_KEY, base_merkle));
    }

    #[test]
    fn pvm_descriptors() {
        let mut vfs = MockFilesystemProvider::new();
        vfs.add("zbi", &[0x00u8; 128]);
        vfs.add("boot-shim", &[0x00u8; 128]);
        vfs.add("salt", hex::encode([0xAAu8; 32]).as_bytes());

        let salt = Salt::try_from(&[0xAAu8; 32][..]).unwrap();

        for debug in [false, true] {
            let descriptors =
                descriptors_for_pvm("zbi", "boot-shim", salt.clone(), debug, &vfs).unwrap();

            // Validate that there's only the one descriptor.
            assert_eq!(descriptors.len(), 3);

            let mut kernel_hash_seen = false;
            let mut ramdisk_hash_seen = false;
            let mut property_seen = false;
            for desc in &descriptors {
                match desc {
                    Descriptor::Hash(hash) => {
                        if hash.image_name() == PVM_HASH_DESCRIPTOR_NAME_KERNEL {
                            assert!(!kernel_hash_seen);
                            kernel_hash_seen = true;
                        } else {
                            if debug {
                                assert_eq!(
                                    hash.image_name(),
                                    PVM_HASH_DESCRIPTOR_NAME_RAMDISK_DEBUG
                                );
                            } else {
                                assert_eq!(
                                    hash.image_name(),
                                    PVM_HASH_DESCRIPTOR_NAME_RAMDISK_NORMAL
                                );
                            }
                            assert!(!ramdisk_hash_seen);
                            ramdisk_hash_seen = true;
                        }
                        assert_eq!(salt, hash.salt().unwrap());

                        let expected_digest = hex::decode(
                            "caeaacb8208cfd8d214de6baef8d535f6fce499524c60aa5dcd2fce7043a9700",
                        )
                        .unwrap();
                        assert_eq!(Some(expected_digest.as_ref()), hash.digest());
                    }
                    Descriptor::Property(prop) => {
                        assert_eq!(&prop.key, PVM_PROP_VIRT_CAP_KEY);
                        assert_eq!(&prop.value, PVM_PROP_VIRT_CAP_VALUE);
                        assert!(!property_seen);
                        property_seen = true;
                    }
                    Descriptor::KernelCmdline(_) => {
                        panic!("Unexpected KernelCmdline descriptor");
                    }
                    Descriptor::ChainPartition(_) => {
                        panic!("Unexpected ChainPartition descriptor");
                    }
                }
            }
            assert!(kernel_hash_seen);
            assert!(ramdisk_hash_seen);
            assert!(property_seen);
        }
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
            sign(vec![descriptor.clone()], "key", &Some("key_metadata"), salt, 0, &vfs).unwrap();

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
