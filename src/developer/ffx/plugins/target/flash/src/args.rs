// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use argh::{ArgsInfo, FromArgs};
use ffx_config::EnvironmentContext;
use ffx_core::ffx_command;
use ffx_flash_manifest::{Command, ManifestParams, OemFile};
use std::default::Default;
use std::path::PathBuf;

const FLASH_TIMEOUT_RATE_KEY: &str = "fastboot.flash.timeout_rate";
const DEFAULT_FLASH_TIMEOUT_RATE_MB_PER_SEC: f64 = 2.0;
const FLASH_MIN_TIMEOUT_SECS_KEY: &str = "fastboot.flash.min_timeout_secs";
const DEFAULT_FLASH_MIN_TIMEOUT_SECS: u64 = 60;

#[ffx_command()]
#[derive(ArgsInfo, FromArgs, Default, Clone, Debug, PartialEq)]
#[argh(
    subcommand,
    name = "flash",
    description = "Flash an image to a target device",
    example = "To flash a specific image:

    $ ffx target flash --manifest $(fx get-build-dir)/flash.json --product fuchsia

To include SSH keys as well:

    $ ffx target flash
    --authorized-keys ~/fuchsia/.ssh/authorized_keys
    $(fx get-build-dir)/flash.json
    --product fuchsia",
    note = "Flashes an image to a target device using the fastboot protocol.
Requires a specific <manifest> file and <product> name as an input.

This is only applicable to a physical device and not an emulator target.
The target device is typically connected via a micro-USB connection to
the host system.

The <manifest> format is a JSON file generated when building a fuchsia
<product> and can be found in the build output directory.

The `--oem-stage` option can be supplied multiple times for several OEM
files. The format expects a single OEM command to execute after staging
the given file.

The format for the `--oem-stage` parameter is a comma separated pair:
'<OEM_COMMAND>,<FILE_TO_STAGE>'"
)]
pub struct FlashCommand {
    #[argh(
        positional,
        description = "path to flashing manifest or zip file containing images and manifest"
    )]
    pub manifest_path: Option<PathBuf>,

    #[argh(
        option,
        short = 'p',
        description = "product entry in manifest - defaults to `fuchsia`",
        default = "String::from(\"fuchsia\")"
    )]
    pub product: String,

    #[argh(option, short = 'b', description = "optional product bundle name")]
    pub product_bundle: Option<String>,

    #[argh(option, short = 'm', description = "optional manifest path")]
    pub manifest: Option<PathBuf>,

    #[argh(option, description = "oem staged file - can be supplied multiple times")]
    pub oem_stage: Vec<OemFile>,

    #[argh(
        option,
        description = "path to authorized keys file - will default to the value configured for \
           `ssh.pub` key in ffx config. If the file does not exist, it will be created."
    )]
    pub authorized_keys: Option<String>,

    #[argh(
        switch,
        description = "the device should not reboot after bootloader images are flashed"
    )]
    pub no_bootloader_reboot: bool,

    #[argh(
        switch,
        description = "skip hardware verification. This is dangerous, please be sure the images you are flashing match the device"
    )]
    pub skip_verify: bool,

    #[argh(option, description = "flash timeout rate in mb/second.")]
    pub timeout_rate: Option<f64>,

    #[argh(
        option,
        description = "minimum timeout in seconds to wait while flashing per-partition"
    )]
    pub min_timeout_secs: Option<u64>,

    #[argh(
        switch,
        description = "skip uploading ssh authorized keys. This is dangerous, you will be unable to communicate with the target via ffx.",
        hidden_help
    )]
    pub skip_authorized_keys: bool,

    #[argh(
        option,
        default = "true",
        description = "use streaming fastboot flash if supported by the device"
    )]
    pub streaming_flash: bool,
}

impl FlashCommand {
    pub fn min_timeout_secs(
        &self,
        context: &EnvironmentContext,
    ) -> Result<u64, ffx_config::api::ConfigError> {
        match self.min_timeout_secs {
            Some(val) => Ok(val),
            None => Ok(context
                .get_optional::<Option<u64>, _>(FLASH_MIN_TIMEOUT_SECS_KEY)?
                .unwrap_or(DEFAULT_FLASH_MIN_TIMEOUT_SECS)),
        }
    }

    pub fn timeout_rate(
        &self,
        context: &EnvironmentContext,
    ) -> Result<f64, ffx_config::api::ConfigError> {
        match self.timeout_rate {
            Some(val) => Ok(val),
            None => Ok(context
                .get_optional::<Option<f64>, _>(FLASH_TIMEOUT_RATE_KEY)?
                .unwrap_or(DEFAULT_FLASH_TIMEOUT_RATE_MB_PER_SEC)),
        }
    }

    pub fn to_manifest(self, context: &EnvironmentContext) -> ManifestParams {
        let flash_min_timeout_seconds = self.min_timeout_secs(context).ok().unwrap();
        let flash_timeout_rate_mb_per_second = self.timeout_rate(context).ok().unwrap();
        let manifest = self.manifest.or(self.manifest_path);
        ManifestParams {
            manifest,
            product: self.product.clone(),
            product_bundle: self.product_bundle,
            oem_stage: self.oem_stage,
            skip_verify: self.skip_verify,
            no_bootloader_reboot: self.no_bootloader_reboot,
            op: Command::Flash,
            flash_min_timeout_seconds,
            flash_timeout_rate_mb_per_second,
            ssh_key: self.authorized_keys,
            streaming_flash: self.streaming_flash,
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use ffx_config::ConfigLevel;
    use tempfile::NamedTempFile;

    #[fuchsia::test]
    fn test_oem_staged_file_from_str() {
        let test_oem_cmd = "test-oem-cmd";
        let tmp_file = NamedTempFile::new().expect("tmp access failed");
        let tmp_file_name = tmp_file.path().to_string_lossy().to_string();
        let test_staged_file = format!("{},{}", test_oem_cmd, tmp_file_name)
            .parse::<OemFile>()
            .expect("parse OemFile");
        assert_eq!(test_oem_cmd, test_staged_file.command());
        assert_eq!(tmp_file_name, test_staged_file.file());
    }

    #[fuchsia::test]
    fn test_oem_staged_file_from_str_fails_with_nonexistent_file() {
        let test_oem_cmd = "test-oem-cmd";
        let tmp_file_name = "/fake/test/for/testing/that/should/not/exist";
        let test_staged_file = format!("{},{}", test_oem_cmd, tmp_file_name).parse::<OemFile>();
        assert!(test_staged_file.is_err());
    }

    #[fuchsia::test]
    fn test_oem_staged_file_from_str_fails_with_malformed_string() {
        let test_oem_cmd = "test-oem-cmd";
        let tmp_file_name = "/fake/test/for/testing/that/should/not/exist";
        let test_staged_file = format!("{}..{}", test_oem_cmd, tmp_file_name).parse::<OemFile>();
        assert!(test_staged_file.is_err());
    }

    #[fuchsia::test]
    fn test_oem_staged_file_from_str_fails_with_empty_string() {
        let test_staged_file = "".parse::<OemFile>();
        assert!(test_staged_file.is_err());
    }

    #[fuchsia::test]
    fn test_oem_staged_files_are_in_manifest_params() {
        let env = ffx_config::test_env().build().expect("test env");
        let test_oem_cmd = "test-oem-cmd";
        let tmp_file = NamedTempFile::new().expect("tmp access failed");
        let tmp_file_name = tmp_file.path().to_string_lossy().to_string();
        let test_staged_file = format!("{},{}", test_oem_cmd, tmp_file_name)
            .parse::<OemFile>()
            .expect("parse OemFile");
        let cmd = FlashCommand {
            manifest_path: None,
            manifest: None,
            product: "fuchsia".to_string(),
            product_bundle: None,
            authorized_keys: None,
            no_bootloader_reboot: false,
            skip_verify: false,
            oem_stage: vec![test_staged_file],
            min_timeout_secs: Some(100),
            timeout_rate: Some(1.0),
            skip_authorized_keys: false,
            streaming_flash: true,
        };

        let params: ManifestParams = cmd.to_manifest(&env.context);
        assert_eq!(params.oem_stage[0].file(), tmp_file_name);
        assert_eq!(params.oem_stage[0].command(), test_oem_cmd);
        assert!(params.streaming_flash);
    }

    #[fuchsia::test]
    fn test_flash_config_defaults() {
        let env = ffx_config::test_env().build().expect("test env");

        // Ensure `src/developer/ffx/data/config.json` provides the expected default values
        // for the exact configuration keys used by infra and external callers.
        let default_min_timeout: u64 = env
            .context
            .query("fastboot.flash.min_timeout_secs")
            .level(Some(ConfigLevel::Default))
            .build()
            .get(&env.context)
            .expect("default fastboot.flash.min_timeout_secs");
        assert_eq!(default_min_timeout, 60);

        let default_timeout_rate: f64 = env
            .context
            .query("fastboot.flash.timeout_rate")
            .level(Some(ConfigLevel::Default))
            .build()
            .get(&env.context)
            .expect("default fastboot.flash.timeout_rate");
        assert_eq!(default_timeout_rate, 2.0);

        let cmd = FlashCommand::default();
        assert_eq!(cmd.min_timeout_secs(&env.context).expect("min_timeout_secs"), 60);
        assert_eq!(cmd.timeout_rate(&env.context).expect("timeout_rate"), 2.0);

        let params = cmd.to_manifest(&env.context);
        assert_eq!(params.flash_min_timeout_seconds, 60);
        assert_eq!(params.flash_timeout_rate_mb_per_second, 2.0);
    }

    #[fuchsia::test]
    fn test_flash_config_user_overrides() {
        let env = ffx_config::test_env()
            .user_config("fastboot.flash.min_timeout_secs", 600)
            .user_config("fastboot.flash.timeout_rate", 4.5)
            .build()
            .expect("test env");

        let cmd = FlashCommand::default();
        assert_eq!(cmd.min_timeout_secs(&env.context).expect("min_timeout_secs"), 600);
        assert_eq!(cmd.timeout_rate(&env.context).expect("timeout_rate"), 4.5);

        let params = cmd.to_manifest(&env.context);
        assert_eq!(params.flash_min_timeout_seconds, 600);
        assert_eq!(params.flash_timeout_rate_mb_per_second, 4.5);
    }

    #[fuchsia::test]
    fn test_flash_config_runtime_string_overrides() {
        // Infra and external callers pass runtime overrides via `-c key=value`, which
        // may be stored as strings (e.g. "600", "1").
        let env = ffx_config::test_env()
            .runtime_config("fastboot.flash.min_timeout_secs", "600")
            .runtime_config("fastboot.flash.timeout_rate", "1")
            .build()
            .expect("test env");

        let cmd = FlashCommand::default();
        assert_eq!(cmd.min_timeout_secs(&env.context).expect("min_timeout_secs"), 600);
        assert_eq!(cmd.timeout_rate(&env.context).expect("timeout_rate"), 1.0);

        let params = cmd.to_manifest(&env.context);
        assert_eq!(params.flash_min_timeout_seconds, 600);
        assert_eq!(params.flash_timeout_rate_mb_per_second, 1.0);
    }

    #[fuchsia::test]
    fn test_flash_cli_args_override_config() {
        let env = ffx_config::test_env()
            .runtime_config("fastboot.flash.min_timeout_secs", 600)
            .runtime_config("fastboot.flash.timeout_rate", 4.0)
            .build()
            .expect("test env");

        let cmd = FlashCommand {
            min_timeout_secs: Some(120),
            timeout_rate: Some(8.0),
            ..Default::default()
        };
        assert_eq!(cmd.min_timeout_secs(&env.context).expect("min_timeout_secs"), 120);
        assert_eq!(cmd.timeout_rate(&env.context).expect("timeout_rate"), 8.0);

        let params = cmd.to_manifest(&env.context);
        assert_eq!(params.flash_min_timeout_seconds, 120);
        assert_eq!(params.flash_timeout_rate_mb_per_second, 8.0);
    }
}
