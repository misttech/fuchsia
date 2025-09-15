// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{ExtractProductPackageArgs, HybridProductArgs, ProductArgs, common};
use anyhow::{Context, Result};
use assembly_config_schema::ProductConfig;
use assembly_container::AssemblyContainer;
use assembly_release_info::{ProductReleaseInfo, ReleaseInfo};
use camino::Utf8PathBuf;
use depfile::Depfile;
use fuchsia_archive::Utf8Reader;
use fuchsia_pkg::{PackageBuilder, PackageManifest};
use product_input_bundle::ProductInputBundle;
use std::io::Cursor;

pub fn new(args: &ProductArgs) -> Result<()> {
    let mut config = ProductConfig::from_config_path(&args.config)?;

    for path in &args.product_input_bundles {
        let pib = ProductInputBundle::from_dir(path)?;
        config.product_input_bundles.insert(pib.release_info.name.clone(), pib);
    }

    let name = match config.product.build_info {
        Some(ref value) => value.name.clone(),
        // TODO(https://fxbug.dev/418249336): Make
        // product.build_info.name a required field.
        None => "unknown".to_string(),
    };
    let version = common::get_release_version(&args.version, &args.version_file)?;
    let repository = common::get_release_repository(&args.repo, &args.repo_file)?;

    config.product.release_info = ProductReleaseInfo {
        info: ReleaseInfo {
            name: common::validate_string_for_upstream_versioning(name)?,
            version: common::validate_string_for_upstream_versioning(version)?,
            repository: common::validate_string_for_upstream_versioning(repository)?,
        },
        pibs: config.product_input_bundles.values().map(|p| p.release_info.clone()).collect(),
    };

    // Build systems generally don't add package names to the config, so it
    // serializes index numbers in place of package names by default.
    // We add the package names in now, so all the rest of the rules can assume
    // the config has proper package names.
    let config = config.add_package_names()?;
    config.write_to_dir(&args.output, args.depfile.as_ref())?;
    Ok(())
}

pub fn hybrid(args: &HybridProductArgs) -> Result<()> {
    let config = ProductConfig::from_dir(&args.input)?;

    // Normally this would not be necessary, because all generated configs come
    // from this tool, which adds the package names above, but we still need to
    // support older product configs without names.
    let mut config = config.add_package_names()?;

    for package_manifest_path in &args.replace_package {
        let package_manifest = PackageManifest::try_load_from(&package_manifest_path)?;
        let package_name = package_manifest.name();
        if let Some(path) = find_package_in_product(&mut config, &package_name) {
            *path = package_manifest_path.clone();
        }
    }

    // Replace PIBs that match an existing PIB by name.
    for path in &args.product_input_bundles {
        let pib = ProductInputBundle::from_dir(path)?;
        config.product_input_bundles.entry(pib.release_info.name.clone()).and_modify(|e| *e = pib);
    }

    config.write_to_dir(&args.output, args.depfile.as_ref())?;
    Ok(())
}

pub fn extract_package(args: &ExtractProductPackageArgs) -> Result<()> {
    let mut config = ProductConfig::from_dir(&args.config)?;
    let mut deps = Depfile::new();

    if let Some(package_manifest_path) = find_package_in_product(&mut config, &args.package_name) {
        let manifest =
            PackageManifest::try_load_from(&package_manifest_path).with_context(|| {
                format!("Loading package manifest to extract: {}", &package_manifest_path)
            })?;

        if args.depfile.is_some() {
            // The config file is a dependency.
            let config_path = args.config.join("product_configuration.json");
            deps.add_input(config_path.as_str());

            // The manifest we are extracting from is a dependency.
            deps.add_input(package_manifest_path.as_str());

            // The source blobs of that manifest are dependencies.
            deps.add_inputs(manifest.blobs().iter().map(|b| b.source_path.clone()));

            // The contents of the `meta.far` like components will be extracted into `outdir`.
            // Track those outputs too.
            if let Some(blob) =
                manifest.blobs().iter().find(|b| b.path == PackageManifest::META_FAR_BLOB_PATH)
            {
                let bytes = std::fs::read(&blob.source_path)
                    .with_context(|| format!("reading {}", blob.source_path))?;
                let meta_far = Utf8Reader::new(Cursor::new(bytes)).context("reading FAR")?;
                deps.add_outputs(
                    meta_far
                        .list()
                        .map(|e| args.outdir.join(e.path()).to_string())
                        .filter(|p| !p.ends_with('/')),
                );
            }
        }

        let mut builder = PackageBuilder::from_manifest(manifest, &args.outdir)
            .with_context(|| format!("Loading package to extract: {}", &args.package_name))?;

        let metafar_path =
            args.output_package_manifest.parent().context("Invalid outdir")?.join("meta.far");
        builder.manifest_path(args.output_package_manifest.clone());
        builder
            .build(&args.outdir, &metafar_path)
            .with_context(|| format!("Writing out extracted package: {}", &args.package_name))?;

        if let Some(depfile_path) = &args.depfile {
            deps.add_outputs(
                [
                    metafar_path,
                    args.outdir.join("meta/fuchsia.abi/abi-revision"),
                    args.outdir.join("meta/fuchsia.pkg/subpackages"),
                    args.outdir.join("meta/package"),
                ]
                .iter()
                .map(|p| p.to_string()),
            );
            deps.add_output(args.output_package_manifest.as_str());
            deps.write_to(depfile_path)?;
        }
    } else {
        anyhow::bail!("Could not find package to extract: {}", &args.package_name);
    }

    Ok(())
}

fn find_package_in_product<'a>(
    config: &'a mut ProductConfig,
    package_name: impl AsRef<str>,
) -> Option<&'a mut Utf8PathBuf> {
    config.product.packages.base.iter_mut().chain(&mut config.product.packages.cache).find_map(
        |(name, pkg)| {
            if name == package_name.as_ref() {
                return Some(&mut pkg.manifest);
            }
            return None;
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuchsia_pkg::PackageName;
    use std::fs;
    use std::fs::File;
    use std::io::Write;
    use tempfile::{NamedTempFile, tempdir};
    use version_history::AbiRevision;

    const FAKE_ABI_REVISION: AbiRevision = AbiRevision::from_u64(0x5836508c2defac54);

    fn create_tmp_file(content: String) -> (NamedTempFile, Utf8PathBuf) {
        let file = NamedTempFile::new().unwrap();
        let path = Utf8PathBuf::from_path_buf(file.path().to_path_buf()).unwrap();
        let file_write = file.reopen();
        file_write.unwrap().write_all(content.as_bytes()).unwrap();
        (file, path)
    }

    #[test]
    fn test_versioned() {
        let (_version_file, version_path) = create_tmp_file("fake_version".to_string());
        let (_jiri_snapshot_file, jiri_snapshot_path) = create_tmp_file("snapshot".to_string());
        let (_latest_commit_date_file, latest_commit_date_path) =
            create_tmp_file("timestamp".to_string());

        let tmp_dir = tempdir().unwrap();
        let tmp_path = Utf8PathBuf::from_path_buf(tmp_dir.path().to_path_buf()).unwrap();
        let product_path = tmp_path.join("my_product");
        fs::create_dir(&product_path).unwrap();

        let config_path = product_path.join("product_configuration.json");
        let config_file = File::create(&config_path).unwrap();
        let config_value = serde_json::json!({
            "platform": {
                "build_type": "eng",
            },
            "product": {
                "build_info": {
                    "name": "fake_product_name",
                    "version": version_path,
                    "jiri_snapshot": jiri_snapshot_path,
                    "latest_commit_date": latest_commit_date_path,
                }
            }
        });
        serde_json::to_writer(&config_file, &config_value).unwrap();

        let args = ProductArgs {
            config: config_path,
            repo: None,
            repo_file: None,
            output: product_path.clone(),
            depfile: None,
            product_input_bundles: vec![],
            version: Some("fake_version".to_string()),
            version_file: None,
        };
        let _ = new(&args);
        let config = ProductConfig::from_dir(product_path).unwrap();
        let expected = "fake_version".to_string();
        assert_eq!(expected, config.product.release_info.info.version);
    }

    #[test]
    fn test_unversioned() {
        let tmp_dir = tempdir().unwrap();
        let tmp_path = Utf8PathBuf::from_path_buf(tmp_dir.path().to_path_buf()).unwrap();
        let product_path = tmp_path.join("my_product");
        fs::create_dir(&product_path).unwrap();

        let config_path = product_path.join("product_configuration.json");
        let config_file = File::create(&config_path).unwrap();
        let config_value = serde_json::json!({
            "platform": {
                "build_type": "eng",
            },
            "product": {
            }
        });
        serde_json::to_writer(&config_file, &config_value).unwrap();

        let args = ProductArgs {
            config: config_path,
            repo: None,
            repo_file: None,
            output: product_path.clone(),
            depfile: None,
            product_input_bundles: vec![],
            version: None,
            version_file: None,
        };
        let _ = new(&args);
        let config = ProductConfig::from_dir(product_path).unwrap();
        let expected = "unversioned".to_string();
        assert_eq!(expected, config.product.release_info.info.version);
    }

    #[test]
    fn test_extract_package() {
        let tmp_dir = tempdir().unwrap();
        let tmp_path = Utf8PathBuf::from_path_buf(tmp_dir.path().to_path_buf()).unwrap();
        let product_path = tmp_path.join("my_product");
        fs::create_dir(&product_path).unwrap();

        let packages_path = product_path.join("packages");
        fs::create_dir(&packages_path).unwrap();

        let gendir = tmp_path.join("gendir");
        fs::create_dir(&gendir).unwrap();

        let test_package_path = packages_path.join("test");
        let mut builder = PackageBuilder::new("test", FAKE_ABI_REVISION);
        builder.add_contents_as_blob("some/file", "foobar", &gendir).unwrap();
        builder.manifest_path(test_package_path.clone());
        let metafar_path = packages_path.join("meta.far");
        builder.build(&packages_path, &metafar_path).unwrap();

        let config_path = product_path.join("product_configuration.json");
        let config_file = File::create(&config_path).unwrap();
        let config_value = serde_json::json!({
            "platform": {
                "build_type": "eng",
            },
            "product": {
                "packages": {
                    "base": {
                       "test" : {
                         "manifest": "packages/test"
                       },
                    },
                },
            },
        });
        serde_json::to_writer(&config_file, &config_value).unwrap();

        let outdir_path = tmp_path.join("outdir");
        let output_package_manifest_path = tmp_path.join("manifest.json");
        let depfile_path = tmp_path.join("depfile");

        let args = ExtractProductPackageArgs {
            config: product_path,
            package_name: "test".into(),
            outdir: outdir_path.clone(),
            output_package_manifest: output_package_manifest_path.clone(),
            depfile: Some(depfile_path.clone()),
        };
        extract_package(&args).unwrap();
        let extracted_package = PackageManifest::try_load_from(&output_package_manifest_path)
            .expect("Package manifest loaded");

        assert_eq!(extracted_package.name(), &"test".parse::<PackageName>().unwrap());

        let depfile_contents = fs::read_to_string(depfile_path).unwrap();
        println!("{}", depfile_contents);
        let mut expected_outputs = vec![
            tmp_path.join("meta.far").to_string(),
            outdir_path.join("meta/contents").to_string(),
            outdir_path.join("meta/fuchsia.abi/abi-revision").to_string(),
            outdir_path.join("meta/fuchsia.pkg/subpackages").to_string(),
            outdir_path.join("meta/package").to_string(),
            output_package_manifest_path.to_string(),
        ];
        expected_outputs.sort();
        let mut expected_inputs = vec![
            packages_path.join("meta.far").to_string(),
            config_path.to_string(),
            test_package_path.to_string(),
            gendir.join("some/file").into(),
        ];
        expected_inputs.sort();

        let mut actual_parts = depfile_contents.split(":").collect::<Vec<_>>();
        let actual_inputs_str = actual_parts.pop().unwrap().trim();
        let actual_outputs_str = actual_parts.pop().unwrap().trim();

        let mut actual_outputs =
            actual_outputs_str.split_whitespace().filter(|x| x != &"\\").collect::<Vec<_>>();
        actual_outputs.sort();

        let mut actual_inputs =
            actual_inputs_str.split_whitespace().filter(|x| x != &"\\").collect::<Vec<_>>();
        actual_inputs.sort();

        assert_eq!(actual_inputs, expected_inputs);
        assert_eq!(actual_outputs, expected_outputs);
    }
}
