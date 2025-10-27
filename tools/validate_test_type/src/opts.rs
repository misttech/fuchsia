// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Error, ensure};
use camino::Utf8PathBuf;
use clap::Parser;

#[derive(Debug, PartialEq, Clone, clap::ValueEnum)]
#[clap(rename_all = "snake_case")]
pub enum ValidateType {
    Hermetic,
    HostOnly,
    EndToEnd,
}

#[derive(Parser, Debug)]
pub struct Opt {
    #[arg(long = "test-group-name")]
    pub test_group_name: String,

    #[arg(short = 'i', long = "test_list")]
    /// Path to the test list file.
    pub test_list: Utf8PathBuf,

    #[arg(short = 't', long = "test-components")]
    /// Path to the test components list file.
    pub test_components_list: Utf8PathBuf,

    /// Validation type.
    /// Possible values: [ hermetic ]
    #[arg(short = 'v', long = "validate", ignore_case = true, value_enum, default_value_t = ValidateType::Hermetic)]
    pub validation_type: ValidateType,

    #[arg(short = 'b', long = "build-dir")]
    /// Path to the build directory.
    pub build_dir: Utf8PathBuf,

    #[arg(short = 'o', long = "output")]
    /// Path to an optional output file with the results.
    pub output: Option<Utf8PathBuf>,

    #[arg(short = 'd', long = "depfile")]
    // Path to output a depfile.
    pub depfile: Option<Utf8PathBuf>,
}

impl Opt {
    pub fn validate(&self) -> Result<(), Error> {
        ensure!(self.test_list.exists(), "test_list {:?} does not exist", self.test_list);
        ensure!(
            self.test_components_list.exists(),
            "test_components_list {:?} does not exist",
            self.test_components_list
        );
        ensure!(self.build_dir.exists(), "build-dir {:?} does not exist", self.build_dir);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::ValueEnum;
    use tempfile::{NamedTempFile, tempdir};

    #[test]
    fn test_validate_type_from_str() {
        assert_eq!(ValidateType::from_str("hermetic", true), Ok(ValidateType::Hermetic));
        assert_eq!(ValidateType::from_str("unknown", true), Err("invalid variant: unknown".into()));
    }

    #[test]
    fn test_opt() {
        // Modify these paths to point to actual or non-existent paths for testing
        let test_list = NamedTempFile::new().expect("Failed to create temporary test_list");
        let test_components_list =
            NamedTempFile::new().expect("Failed to create temporary test_components_list");
        let build_dir = tempdir().expect("Failed to create temporary build_dir");
        let output = build_dir.path().join("output.txt");

        let opt = Opt {
            test_group_name: "test_group".into(),
            test_list: Utf8PathBuf::from_path_buf(test_list.path().into()).unwrap(),
            test_components_list: Utf8PathBuf::from_path_buf(test_components_list.path().into())
                .unwrap(),
            validation_type: ValidateType::Hermetic,
            build_dir: Utf8PathBuf::from_path_buf(build_dir.path().into()).unwrap(),
            output: Some(Utf8PathBuf::from_path_buf(output).unwrap()),
            depfile: Some(Utf8PathBuf::from("output.d")),
        };

        // Test valid paths
        assert!(opt.validate().is_ok());

        // Test missing paths
        let invalid_opt = Opt {
            test_group_name: "test_group".into(),
            test_list: Utf8PathBuf::from("/tmp/nonexistent"),
            test_components_list: Utf8PathBuf::from("/tmp/nonexistent"),
            validation_type: ValidateType::Hermetic,
            build_dir: Utf8PathBuf::from("/tmp/nonexistent"),
            output: None,
            depfile: None,
        };
        assert!(invalid_opt.validate().is_err());
    }
}
