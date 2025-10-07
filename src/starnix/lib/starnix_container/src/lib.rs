// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::collections::HashMap;
use std::fs::File;
use std::io::Cursor;

use anyhow::{Context, Result, bail};
use camino::{Utf8Path, Utf8PathBuf};
use ext4_extract::ext4_extract;
use fuchsia_pkg::{PackageBuilder, PackageManifest};
use fuchsia_url::RelativePackageUrl;

mod hal_manifest;
mod remote_bundle;

use crate::remote_bundle::Writer;
use depfile::Depfile;

#[derive(Debug, Clone)]
pub struct StarnixContainerGenerator {
    pub name: String,                 //name of the starnix container
    pub outdir: Utf8PathBuf,          //directory to place outputs into
    pub base: Utf8PathBuf, //path to package archive containing additional resources to include
    pub hals: Vec<Utf8PathBuf>, //path to hal package archive
    pub skip_subpackages: bool, //whether to skip including HALs as subpackages.
    pub system: Utf8PathBuf, //path to an Android system image
    pub vendor: Option<Utf8PathBuf>, //path to an Android vendor partition image
    pub fstab: Option<Utf8PathBuf>, //path to fstab, will go in /odm which overrides the one in /vendor
    pub init: Vec<Utf8PathBuf>, //path to extra init scripts, will go in /odm/etc/init. Can be passed more than once.
    pub depfile: Option<Utf8PathBuf>, //path to a depfile to write
}

impl StarnixContainerGenerator {
    // Build the StarnixContainer
    fn add_ext4_image(
        self,
        name: impl AsRef<Utf8Path>,
        outdir: impl AsRef<Utf8Path>,
        image_path: impl AsRef<Utf8Path>,
        builder: &mut PackageBuilder,
    ) -> Result<HashMap<String, String>> {
        // Put all the system image files into the container.
        let name = name.as_ref();
        let outdir = outdir.as_ref();
        let image_path = image_path.as_ref();

        let image_outdir = outdir.join(name);
        std::fs::create_dir_all(&image_outdir)
            .with_context(|| format!("Preparing directory for image files: {}", &image_outdir))?;
        let image_files = ext4_extract(image_path.as_str(), image_outdir.as_str())
            .context("Extracting system files")?;
        for (dst, src) in &image_files {
            let dst = format!("data/{}/{}", name, dst);
            builder
                .add_file_as_blob(dst, &src)
                .with_context(|| format!("Adding blob from file: {}", &src))?;
        }

        Ok(image_files)
    }

    fn add_to_odm(
        self,
        src: &fuchsia_pkg::BlobInfo,
        dst: &[&str],
        odm_writer: &mut Writer,
    ) -> Result<String> {
        let src = &src.source_path;
        File::open(src)
            .and_then(|mut file| odm_writer.add_file(dst, &mut file))
            .with_context(|| format!("Adding {src} in HAL package to {dst:?}"))?;
        Ok(src.clone())
    }

    fn clone_package(
        self,
        manifest_path: &Utf8PathBuf,
        outdir: &String,
        deps: &mut Depfile,
    ) -> Result<PackageBuilder> {
        let manifest = PackageManifest::try_load_from(manifest_path)
            .with_context(|| format!("Reading base starnix package: {}", manifest_path))?;

        // Our tool will eventually read everything in the base package.
        deps.add_inputs(manifest.blobs().iter().map(|b| b.source_path.clone()));

        // [`PackageBuilder::from_manifest`] will unpack the contents of the `meta.far` into `outdir`.
        // Track those outputs too.
        if let Some(blob) =
            manifest.blobs().iter().find(|b| b.path == PackageManifest::META_FAR_BLOB_PATH)
        {
            let bytes = std::fs::read(&blob.source_path)
                .with_context(|| format!("reading {}", blob.source_path))?;
            let meta_far =
                fuchsia_archive::Utf8Reader::new(Cursor::new(bytes)).context("reading FAR")?;
            deps.add_outputs(meta_far.list().map(|e| format!("{}/{}", outdir, e.path())));
        }

        let builder = PackageBuilder::from_manifest(manifest, outdir)
            .context("Parsing base starnix package")?;

        Ok(builder)
    }

    pub fn build(self) -> Result<()> {
        // Track inputs and outputs for producing a depfile for incremental build correctness.
        let mut deps = Depfile::new();

        // Bootstrap the package builder with the contents of the base package, but update the
        // internal and published names.
        let mut builder =
            self.clone().clone_package(&self.base, &self.outdir.to_string(), &mut deps)?;
        builder.name(&self.name);
        builder.published_name(&self.name);
        builder.manifest_blobs_relative_to(fuchsia_pkg::RelativeTo::File);

        let system_files =
            self.clone().add_ext4_image("system", &self.outdir, &self.system, &mut builder)?;
        deps.add_input(&self.system);
        deps.add_outputs(system_files.into_values());

        // Combine the vendor image with the system image.
        if let Some(vendor_path) = &self.vendor {
            let vendor_files =
                self.clone().add_ext4_image("vendor", &self.outdir, &vendor_path, &mut builder)?;
            deps.add_input(vendor_path);
            deps.add_outputs(vendor_files.into_values());
        }

        // Initialize ODM filesystem.
        let odm_outdir = self.outdir.join("odm");
        std::fs::create_dir_all(&odm_outdir)
            .with_context(|| format!("Preparing directory for ODM files: {}", &odm_outdir))?;
        let mut odm_writer = Writer::new(&odm_outdir, |path| {
            // Mimic the SELinux labeling patterns defined for "/odm" in AOSP.
            let label: &[u8] = if path.len() == 0 {
                b"u:object_r:vendor_file:s0"
            } else if path.starts_with(&["etc"]) {
                b"u:object_r:vendor_configs_file:s0"
            } else {
                panic!("No SELinux xattr specified for path {:?}", path);
            };
            [((*b"security.selinux").into(), (*label).into())].into()
        })?;
        odm_writer.add_directory(&["etc"]);
        odm_writer.add_directory(&["etc", "init"]);
        odm_writer.add_directory(&["etc", "vintf"]);
        odm_writer.add_directory(&["etc", "vintf", "manifest"]);

        // Add all the HALs as subpackages.
        for hal in &self.hals {
            let manifest = PackageManifest::try_load_from(&hal)
                .with_context(|| format!("Reading hal package manifest: {}", hal))?;

            if !self.skip_subpackages {
                let name: RelativePackageUrl = manifest.name().to_owned().into();
                builder
                    .add_subpackage(&name, manifest.hash(), hal.into())
                    .with_context(|| format!("Adding subpackage from manifest: {}", &hal))?;
            }

            let hal_package_name = manifest.name().to_string();
            let (hal_manifest, hal_manifest_source_path) =
                hal_manifest::load_from_package(&manifest)
                    .with_context(|| format!("Reading hal manifest from package: {}", hal))?;
            deps.add_inputs(hal_manifest_source_path);
            // If a HAL manifest contains `init_rc`, copy that file to
            // `etc/init/{hal_package_name}.rc` in the ODM filesystem.
            if let Some(blob) = hal_manifest.init_rc {
                deps.add_input(self.clone().add_to_odm(
                    &blob,
                    &["etc", "init", &format!("{hal_package_name}.rc")],
                    &mut odm_writer,
                )?);
            }
            // If a HAL manifest contains `vintf_manifest`, copy that file to
            // `etc/vintf/manifest/{hal_package_name}.xml` in the ODM filesystem.
            if let Some(blob) = hal_manifest.vintf_manifest {
                deps.add_input(self.clone().add_to_odm(
                    &blob,
                    &["etc", "vintf", "manifest", &format!("{hal_package_name}.xml")],
                    &mut odm_writer,
                )?);
            }
        }

        // Add the fstab.
        if let Some(fstab) = self.fstab {
            if let Some(file_name) = fstab.file_name() {
                match (fstab.file_stem(), fstab.extension()) {
                    (Some("fstab"), Some(_)) => {}
                    _ => bail!("fstab must be named \"fstab.<ro.hardware>\""),
                }
                let mut fstab_file =
                    File::open(&fstab).with_context(|| format!("opening fstab from {fstab:?}"))?;
                odm_writer
                    .add_file(&["etc", file_name], &mut fstab_file)
                    .with_context(|| format!("adding fstab to odm"))?;
            }
        }

        // Add any extra init files provided.
        for init in self.init {
            if let Some(file_name) = init.file_name() {
                let mut init_file =
                    File::open(&init).with_context(|| format!("opening init from {init}"))?;
                odm_writer
                    .add_file(&["etc", "init", file_name], &mut init_file)
                    .context("adding init to /odm/etc/init")?;
            } else {
                bail!("{init} doesn't have a filename");
            }
        }

        // Put all the ODM files into the container.
        let odm_files = odm_writer.inner.export().context("Exporting ODM files")?;
        for (dst, src) in &odm_files {
            let dst = format!("data/odm/{}", dst);
            builder
                .add_file_as_blob(dst, &src)
                .with_context(|| format!("Adding blob from file: {}", &src))?;
            deps.add_output(src.clone());
        }

        // Build the starnix container.
        let metafar_path = self.outdir.join("meta.far");
        let manifest_path = self.outdir.join("package_manifest.json");
        builder.manifest_path(manifest_path);
        builder.build(&self.outdir, &metafar_path).context("Building starnix container")?;
        deps.add_outputs(
            [
                self.outdir.join("meta.far"),
                self.outdir.join("meta/fuchsia.abi/abi-revision"),
                self.outdir.join("meta/fuchsia.pkg/subpackages"),
                self.outdir.join("meta/package"),
            ]
            .iter()
            .map(|p| p.to_string()),
        );

        if let Some(depfile) = self.depfile {
            // Create the parent directory just in case it doesn't exist, which would cause depfile
            // write to fail.
            let dir = &depfile
                .parent()
                .context(format!("Getting parent dir for depfile: {}", depfile))?;
            std::fs::create_dir_all(dir)
                .with_context(|| format!("Creating directory for depfile: {}", dir))?;

            let _ = deps.write_to(depfile);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use ext4_metadata::{Metadata, NodeInfo, ROOT_INODE_NUM};
    use itertools::Itertools;
    use serde_json::Value;
    use std::str::FromStr;
    use tempfile::TempDir;

    const EXT4_IMAGE_PATH: &str =
        concat!(env!("ROOT_OUT_DIR"), "/test_data/gen-starnix-container/test.img");

    fn fake_base(outdir: &Utf8Path) -> Utf8PathBuf {
        // Build a fake "base".
        let base_manifest_path = outdir.join("base_package_manifest.json");
        let mut builder = PackageBuilder::new_platform_internal_package("test-base");
        builder.add_contents_as_blob("data/test", "test-base-blob", &outdir).unwrap();
        builder.manifest_path(&base_manifest_path);
        let _ = builder.build(&outdir, outdir.join("base-meta.far")).unwrap();
        base_manifest_path
    }

    #[test]
    fn test_generate() -> Result<()> {
        let tmp = TempDir::new().unwrap();
        let outdir = Utf8Path::from_path(tmp.path()).unwrap();
        let base_manifest_path = fake_base(outdir);

        // Build a fake HAL.
        let hal_manifest_path = outdir.join("hal_package_manifest.json");
        let mut builder = PackageBuilder::new_platform_internal_package("test-hal");
        builder.add_contents_as_blob("data/hal", "test-hal-blob", &outdir).unwrap();
        builder.manifest_path(&hal_manifest_path);
        let _ = builder.build(&outdir, outdir.join("hal-meta.far")).unwrap();

        // Run the generator.
        let container = StarnixContainerGenerator {
            name: "test-name".into(),
            outdir: outdir.to_owned(),
            base: base_manifest_path,
            system: Utf8PathBuf::from_str(EXT4_IMAGE_PATH).unwrap(),
            vendor: Some(Utf8PathBuf::from_str(EXT4_IMAGE_PATH).unwrap()),
            hals: vec![hal_manifest_path],
            depfile: None,
            fstab: None,
            init: vec![],
            skip_subpackages: false,
        };
        container.build().unwrap();

        // Read the package manifest, and ensure the correct files are present as blobs, and the
        // HALs are listed as subpackages.
        let manifest_path = outdir.join("package_manifest.json");
        let manifest = PackageManifest::try_load_from(&manifest_path).unwrap();
        assert_eq!(manifest.name().as_ref(), "test-name");
        let (blobs, subpackages) = manifest.into_blobs_and_subpackages();
        assert_eq!(blobs.len(), 7);
        assert_eq!(subpackages.len(), 1);
        let blob_filenames: Vec<String> = blobs.iter().map(|b| b.path.clone()).collect();

        // Check that the paths in the file are relative to the file, not the current directory.
        // We can't use the typed reader since it will resolve them to absolute paths.
        let manifest_file = File::open(&manifest_path)
            .with_context(|| format!("Opening package manifest: {manifest_path}"))?;
        let manifest_json: Value = serde_json::from_reader(manifest_file)?;

        let blob_source_paths: Vec<String> = manifest_json["blobs"]
            .as_array()
            .context("checking relative manifest blobs")?
            .into_iter()
            .filter_map(|b| b["source_path"].as_str())
            .map(|p| p.to_string())
            .sorted()
            .collect();

        assert_eq!(
            blob_filenames,
            vec![
                "meta/".to_string(),
                "data/odm/metadata.v1".to_string(),
                "data/system/13".to_string(),
                "data/system/metadata.v1".to_string(),
                "data/test".to_string(),
                "data/vendor/13".to_string(),
                "data/vendor/metadata.v1".to_string(),
            ]
        );

        assert_eq!(
            blob_source_paths,
            vec![
                "data/test".to_string(),
                "meta.far".to_string(),
                "odm/metadata.v1".to_string(),
                "system/13".to_string(),
                "system/metadata.v1".to_string(),
                "vendor/13".to_string(),
                "vendor/metadata.v1".to_string(),
            ]
        );

        Ok(())
    }

    #[test]
    fn test_skip_subpackages() -> Result<()> {
        let tmp = TempDir::new().unwrap();
        let outdir = Utf8Path::from_path(tmp.path()).unwrap();
        let base_manifest_path = fake_base(outdir);

        // Build a fake HAL.
        let hal_manifest_path = outdir.join("hal_package_manifest.json");
        let mut builder = PackageBuilder::new_platform_internal_package("test-hal");
        builder.add_contents_as_blob("data/hal", "test-hal-blob", &outdir).unwrap();
        builder.manifest_path(&hal_manifest_path);
        let _ = builder.build(&outdir, outdir.join("hal-meta.far")).unwrap();

        // Run the generator.
        let container = StarnixContainerGenerator {
            name: "test-name".into(),
            outdir: outdir.to_owned(),
            base: base_manifest_path,
            system: Utf8PathBuf::from_str(EXT4_IMAGE_PATH).unwrap(),
            vendor: Some(Utf8PathBuf::from_str(EXT4_IMAGE_PATH).unwrap()),
            hals: vec![hal_manifest_path],
            depfile: None,
            fstab: None,
            init: vec![],
            skip_subpackages: true,
        };
        container.build().unwrap();

        // Read the package manifest, and ensure the correct files are present as blobs, and the
        // HALs are not listed as subpackages.
        let manifest_path = outdir.join("package_manifest.json");
        let manifest = PackageManifest::try_load_from(&manifest_path).unwrap();
        assert_eq!(manifest.name().as_ref(), "test-name");
        let (blobs, subpackages) = manifest.into_blobs_and_subpackages();
        assert_eq!(blobs.len(), 7);
        assert_eq!(subpackages.len(), 0);

        Ok(())
    }

    #[test]
    fn test_hal_init_rc() {
        let tmp = TempDir::new().unwrap();
        let outdir = Utf8Path::from_path(tmp.path()).unwrap();
        let base_manifest_path = fake_base(outdir);

        // Build a fake HAL with an init.rc file.
        let hal_manifest_path = outdir.join("hal_package_manifest.json");
        let mut builder = PackageBuilder::new_platform_internal_package("test-hal");
        builder.add_contents_as_blob("data/hal", "test-hal-blob", &outdir).unwrap();
        builder.add_contents_as_blob("system/init.rc", "service foo bar", &outdir).unwrap();
        builder
            .add_contents_as_blob(
                "__android_config__/manifest.json",
                r#"{ "init_rc": "system/init.rc" }"#,
                &outdir,
            )
            .unwrap();
        builder.manifest_path(&hal_manifest_path);
        let _ = builder.build(&outdir, outdir.join("hal-meta.far")).unwrap();

        // Run the generator.
        let container = StarnixContainerGenerator {
            name: "test-name".into(),
            outdir: outdir.to_owned(),
            base: base_manifest_path,
            system: Utf8PathBuf::from_str(EXT4_IMAGE_PATH).unwrap(),
            hals: vec![hal_manifest_path],
            depfile: None,
            vendor: None,
            fstab: None,
            init: vec![],
            skip_subpackages: false,
        };
        container.build().unwrap();

        // Read the package manifest, and ensure the correct files are present as blobs, and
        // there is an additional `.rc` file corresponding to `test-hal`.
        let manifest_path = outdir.join("package_manifest.json");
        let manifest = PackageManifest::try_load_from(manifest_path).unwrap();
        assert_eq!(manifest.name().as_ref(), "test-name");
        let (blobs, _subpackages) = manifest.into_blobs_and_subpackages();
        assert_eq!(blobs.len(), 6);
        let blob_filenames: Vec<String> = blobs.iter().map(|b| b.path.clone()).collect();
        assert_eq!(
            blob_filenames,
            vec![
                "meta/".to_string(),
                "data/odm/7".to_string(),
                "data/odm/metadata.v1".to_string(),
                "data/system/13".to_string(),
                "data/system/metadata.v1".to_string(),
                "data/test".to_string(),
            ]
        );

        // Find the rc file and check its properties.
        let odm_metadata_path =
            &blobs.iter().find(|b| b.path == "data/odm/metadata.v1").unwrap().source_path;
        let m = Metadata::deserialize(
            &std::fs::read(odm_metadata_path).expect("Failed to read metadata"),
        )
        .expect("Failed to deserialize metadata");
        let etc = m.lookup(ROOT_INODE_NUM, "etc").expect("etc not found");
        let init = m.lookup(etc, "init").expect("init not found");
        let rc = m.lookup(init, "test-hal.rc").expect("rc not found");
        let rc = m.get(rc).expect("rc not found");
        assert_matches!(rc.info(), NodeInfo::File(_));
        assert_eq!(rc.mode, 0o100444);
        assert_eq!(rc.uid, 0);
        assert_eq!(rc.gid, 0);
    }

    #[test]
    fn test_hal_vintf_manifest() {
        let tmp = TempDir::new().unwrap();
        let outdir = Utf8Path::from_path(tmp.path()).unwrap();
        let base_manifest_path = fake_base(outdir);

        // Build a fake HAL with an init.rc file.
        let hal_manifest_path = outdir.join("hal_package_manifest.json");
        let mut builder = PackageBuilder::new_platform_internal_package("test-hal");
        builder.add_contents_as_blob("data/hal", "test-hal-blob", &outdir).unwrap();
        builder
            .add_contents_as_blob("system/manifest.xml", "<manifest></manifest>", &outdir)
            .unwrap();
        builder
            .add_contents_as_blob(
                "__android_config__/manifest.json",
                r#"{ "vintf_manifest": "system/manifest.xml" }"#,
                &outdir,
            )
            .unwrap();
        builder.manifest_path(&hal_manifest_path);
        let _ = builder.build(&outdir, outdir.join("hal-meta.far")).unwrap();

        // Run the generator.
        let container = StarnixContainerGenerator {
            name: "test-name".into(),
            outdir: outdir.to_owned(),
            base: base_manifest_path,
            system: Utf8PathBuf::from_str(EXT4_IMAGE_PATH).unwrap(),
            hals: vec![hal_manifest_path],
            depfile: None,
            vendor: None,
            fstab: None,
            init: vec![],
            skip_subpackages: false,
        };
        container.build().unwrap();

        // Read the package manifest, and ensure the correct files are present as blobs, and
        // there is an additional `.xml` file corresponding to `test-hal`.
        let manifest_path = outdir.join("package_manifest.json");
        let manifest = PackageManifest::try_load_from(manifest_path).unwrap();
        assert_eq!(manifest.name().as_ref(), "test-name");
        let (blobs, _subpackages) = manifest.into_blobs_and_subpackages();
        assert_eq!(blobs.len(), 6);
        let blob_filenames: Vec<String> = blobs.iter().map(|b| b.path.clone()).collect();
        assert_eq!(
            blob_filenames,
            vec![
                "meta/".to_string(),
                "data/odm/7".to_string(),
                "data/odm/metadata.v1".to_string(),
                "data/system/13".to_string(),
                "data/system/metadata.v1".to_string(),
                "data/test".to_string(),
            ]
        );

        // Find the xml file and check its properties.
        let odm_metadata_path =
            &blobs.iter().find(|b| b.path == "data/odm/metadata.v1").unwrap().source_path;
        let m = Metadata::deserialize(
            &std::fs::read(odm_metadata_path).expect("Failed to read metadata"),
        )
        .expect("Failed to deserialize metadata");
        let etc = m.lookup(ROOT_INODE_NUM, "etc").expect("etc not found");
        let vintf = m.lookup(etc, "vintf").expect("vintf not found");
        let manifest = m.lookup(vintf, "manifest").expect("manifest not found");
        let xml = m.lookup(manifest, "test-hal.xml").expect("xml not found");
        let xml = m.get(xml).expect("xml not found");
        assert_matches!(xml.info(), NodeInfo::File(_));
        assert_eq!(xml.mode, 0o100444);
        assert_eq!(xml.uid, 0);
        assert_eq!(xml.gid, 0);
    }

    #[test]
    fn test_fstab() {
        const FSTAB: &'static str = r#"
# Android fstab file.
#<dev>  <mnt_point> <type>  <mnt_flags options> <fs_mgr_flags>
tmpfs   /data       tmpfs   defaults            wait
        "#;
        let tmp = TempDir::new().unwrap();
        let outdir = Utf8Path::from_path(tmp.path()).unwrap();
        let base_manifest_path = fake_base(outdir);

        // Make a fake fstab.
        let fstab_path = outdir.join("fstab.foo");
        std::fs::write(&fstab_path, FSTAB).unwrap();

        // Run the generator.
        let container = StarnixContainerGenerator {
            name: "test-name".into(),
            outdir: outdir.to_owned(),
            base: base_manifest_path,
            system: Utf8PathBuf::from_str(EXT4_IMAGE_PATH).unwrap(),
            hals: vec![],
            depfile: None,
            vendor: None,
            fstab: Some(fstab_path),
            init: vec![],
            skip_subpackages: false,
        };
        container.build().unwrap();

        // Read the package manifest, and ensure the correct files are present as blobs, and
        // there is an additional `.xml` file corresponding to `test-hal`.
        let manifest_path = outdir.join("package_manifest.json");
        let manifest = PackageManifest::try_load_from(manifest_path).unwrap();
        assert_eq!(manifest.name().as_ref(), "test-name");
        let (blobs, _subpackages) = manifest.into_blobs_and_subpackages();
        assert_eq!(blobs.len(), 6);
        let blob_filenames: Vec<String> = blobs.iter().map(|b| b.path.clone()).collect();
        assert_eq!(
            blob_filenames,
            vec![
                "meta/".to_string(),
                "data/odm/7".to_string(),
                "data/odm/metadata.v1".to_string(),
                "data/system/13".to_string(),
                "data/system/metadata.v1".to_string(),
                "data/test".to_string(),
            ]
        );

        let odm_metadata_path =
            &blobs.iter().find(|b| b.path == "data/odm/metadata.v1").unwrap().source_path;
        let m = Metadata::deserialize(
            &std::fs::read(odm_metadata_path).expect("Failed to read metadata"),
        )
        .expect("Failed to deserialize metadata");
        let etc = m.lookup(ROOT_INODE_NUM, "etc").expect("etc not found");
        let fstab = m.lookup(etc, "fstab.foo").expect("fstab not found");
        let fstab = m.get(fstab).expect("fstab not found");
        assert_matches!(fstab.info(), NodeInfo::File(_));
    }

    #[test]
    fn test_init() {
        const INIT: &str = "on boot\n  setprop foo.bar 1";
        let tmp = TempDir::new().unwrap();
        let outdir = Utf8Path::from_path(tmp.path()).unwrap();
        let base_manifest_path = fake_base(outdir);

        let init_path = outdir.join("test.rc");
        std::fs::write(&init_path, INIT).unwrap();

        // Run the generator.
        let container = StarnixContainerGenerator {
            name: "test-name".into(),
            outdir: outdir.to_owned(),
            base: base_manifest_path,
            system: Utf8PathBuf::from_str(EXT4_IMAGE_PATH).unwrap(),
            hals: vec![],
            depfile: None,
            vendor: None,
            fstab: None,
            init: vec![init_path],
            skip_subpackages: false,
        };
        container.build().unwrap();

        // Read the package manifest, and ensure the correct files are present as blobs, and
        // there is an additional `.xml` file corresponding to `test-hal`.
        let manifest_path = outdir.join("package_manifest.json");
        let manifest = PackageManifest::try_load_from(manifest_path).unwrap();
        assert_eq!(manifest.name().as_ref(), "test-name");
        let (blobs, _subpackages) = manifest.into_blobs_and_subpackages();
        assert_eq!(blobs.len(), 6);
        let blob_filenames: Vec<String> = blobs.iter().map(|b| b.path.clone()).collect();
        assert_eq!(
            blob_filenames,
            vec![
                "meta/".to_string(),
                "data/odm/7".to_string(),
                "data/odm/metadata.v1".to_string(),
                "data/system/13".to_string(),
                "data/system/metadata.v1".to_string(),
                "data/test".to_string(),
            ]
        );

        let odm_metadata_path =
            &blobs.iter().find(|b| b.path == "data/odm/metadata.v1").unwrap().source_path;
        let m = Metadata::deserialize(
            &std::fs::read(odm_metadata_path).expect("Failed to read metadata"),
        )
        .expect("Failed to deserialize metadata");
        let etc = m.lookup(ROOT_INODE_NUM, "etc").expect("etc not found");
        let etc_init = m.lookup(etc, "init").expect("init dir not found");
        let init = m.lookup(etc_init, "test.rc").expect("test.rc not found");
        let init = m.get(init).expect("test.rc not found");
        assert_matches!(init.info(), NodeInfo::File(_));
    }
}
