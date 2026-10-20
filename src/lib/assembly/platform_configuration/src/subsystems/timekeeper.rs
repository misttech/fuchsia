// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::subsystems::prelude::*;
use anyhow::{Result, anyhow};
use assembly_constants::BoardFeature;

use assembly_config_capabilities::{Config, ConfigValueType};
use assembly_config_schema::platform_settings::timekeeper_config::TimekeeperConfig;

pub(crate) struct TimekeeperSubsystem;
impl DefineSubsystemConfiguration<TimekeeperConfig> for TimekeeperSubsystem {
    fn define_configuration(
        context: &ConfigurationContext<'_>,
        config: &TimekeeperConfig,
        builder: &mut dyn ConfigurationBuilder,
    ) -> Result<()> {
        // This is an experimental feature that we want to deploy with care.
        // We originally wanted to deploy on eng builds as well, but it proved
        // to be confusing for debugging.
        //
        // See: b/308199171
        let utc_start_at_startup =
            context.board_config.provides_feature(BoardFeature::UtcStartAtStartup);

        // Soft crypto boards don't yet have crypto support, so we exit timekeeper
        // early instead of having it crash repeatedly.
        //
        // See: b/299320231
        let early_exit = context.board_config.provides_feature(BoardFeature::SoftCrypto);

        // Some e2e tests need to change Timekeeper behavior at runtime. This setting
        // allows Timekeeper to serve an endpoint for such runtime behavior changes.
        // Only eng builds are allowed to have this feature.
        let serve_test_protocols = config.serve_test_protocols;
        let use_persistent_storage = config.use_persistent_storage;

        // See:
        // https://fuchsia.dev/fuchsia-src/contribute/governance/rfcs/0115_build_types?hl=en#definitions
        if serve_test_protocols && *context.build_type != BuildType::Eng {
            return Err(anyhow!(
                "`serve_test_protocols==true` is only allowed in `eng` builds, see RFC 0115"
            ));
        }

        // Gives Timekeeper some mutable persistent storage ("data").
        //
        // `serve_test_protocols` is gating this capability because historically it also was
        // used to gate persistent storage. `use_persistent_storage` is a direct setting.
        if serve_test_protocols || use_persistent_storage {
            builder.platform_bundle("timekeeper_persistence")?;
        }

        let has_hrtimer = context.board_config.provides_feature(BoardFeature::Hrtimer);

        // If set, Timekeeper will serve `fuchsia.time.alarms` and will connect
        // to the appropriate hardware device to do so.
        //
        // At the moment the flag is set only if all conditions are met, i.e. the functionality
        // is requested, and the underlying driver is available.
        //
        // The same information is offered as a config capability to other components.
        let serve_fuchsia_time_alarms = config.serve_fuchsia_time_alarms && has_hrtimer;
        builder.set_config_capability(
            "fuchsia.time.config.HasWakeAlarms",
            //TODO: b/416081776 - replace `true` with serve_fuchsia_time_alarms.
            Config::new(ConfigValueType::Bool, true.into()),
        )?;

        // Adds hrtimer routing only for boards that have hardware support for
        // doing so.
        if serve_fuchsia_time_alarms {
            // For all devices.
            builder.platform_bundle("timekeeper_wake_alarms")?;
        }

        // Always on counter is used instead of a persistent RTC on some platforms.
        let has_always_on_counter =
            context.board_config.provides_feature(BoardFeature::AlwaysOnCounter);

        // The always on counter is useless without persistent storage to persist the
        // counter values as would be done in an RTC chip.
        let use_always_on_counter = has_always_on_counter && use_persistent_storage;

        // Allows Timekeeper to short-circuit RTC driver detection at startup.
        let has_real_time_clock =
            context.board_config.provides_feature(BoardFeature::RealTimeClock)
                || use_always_on_counter;

        // Refer to //src/sys/time/timekeeper/config.shard.cml
        // for details.
        builder.set_config_capability(
            "fuchsia.time.config.WritableUTCTime",
            Config::new(ConfigValueType::Bool, config.serve_fuchsia_time_external_adjust.into()),
        )?;

        // Refer to //src/sys/time/timekeeper/config.shard.cml for details.
        // Non-configurable fields (disable_delays, oscillator_error_std_dev_ppm,
        // max_frequency_error_ppm, primary_time_source_url, monitor_time_source_url,
        // initial_frequency_ppm, primary_uses_pull, monitor_uses_pull) use their .cml defaults.
        builder.set_config_capability(
            "fuchsia.time.config.BackOffTimeBetweenPullSamplesSec",
            Config::new(
                ConfigValueType::Int64,
                config.back_off_time_between_pull_samples_sec.into(),
            ),
        )?;
        builder.set_config_capability(
            "fuchsia.time.config.FirstSamplingDelaySec",
            Config::new(ConfigValueType::Int64, config.first_sampling_delay_sec.into()),
        )?;
        builder.set_config_capability(
            "fuchsia.time.config.UtcStartAtStartup",
            Config::new(ConfigValueType::Bool, utc_start_at_startup.into()),
        )?;
        builder.set_config_capability(
            "fuchsia.time.config.EarlyExit",
            Config::new(ConfigValueType::Bool, early_exit.into()),
        )?;
        builder.set_config_capability(
            "fuchsia.time.config.PowerTopologyIntegrationEnabled",
            Config::new(ConfigValueType::Bool, config.power_topology_integration_enabled.into()),
        )?;
        builder.set_config_capability(
            "fuchsia.time.config.ServeTestProtocols",
            Config::new(ConfigValueType::Bool, serve_test_protocols.into()),
        )?;
        builder.set_config_capability(
            "fuchsia.time.config.HasRealTimeClock",
            Config::new(ConfigValueType::Bool, has_real_time_clock.into()),
        )?;
        builder.set_config_capability(
            "fuchsia.time.config.HasAlwaysOnCounter",
            Config::new(ConfigValueType::Bool, use_always_on_counter.into()),
        )?;
        builder.set_config_capability(
            "fuchsia.time.config.UtcStartAtStartupWhenInvalidRtc",
            Config::new(ConfigValueType::Bool, config.utc_start_at_startup_when_invalid_rtc.into()),
        )?;
        builder.set_config_capability(
            "fuchsia.time.config.UtcMaxAllowedDeltaPastSec",
            Config::new(
                ConfigValueType::Int64,
                (config.utc_max_allowed_delta_past_sec as i64).into(),
            ),
        )?;
        builder.set_config_capability(
            "fuchsia.time.config.UtcMaxAllowedDeltaFutureSec",
            Config::new(
                ConfigValueType::Int64,
                (config.utc_max_allowed_delta_future_sec as i64).into(),
            ),
        )?;
        builder.set_config_capability(
            "fuchsia.time.config.UseConnectivity",
            Config::new(ConfigValueType::Bool, config.use_connectivity.into()),
        )?;
        builder.set_config_capability(
            "fuchsia.time.config.MinUtcReferenceToBackstopDiffMinutes",
            Config::new(
                ConfigValueType::Uint64,
                config.min_utc_reference_to_backstop_diff_minutes.into(),
            ),
        )?;
        builder.set_config_capability(
            "fuchsia.time.config.MinAcceptableBootTimeForUtcUpdateMinutes",
            Config::new(
                ConfigValueType::Uint64,
                config.min_acceptable_boot_time_for_utc_update_minutes.into(),
            ),
        )?;
        builder.set_config_capability(
            "fuchsia.time.config.PeriodicRtcUpdateIntervalMinutes",
            Config::new(
                ConfigValueType::Uint32,
                config.periodic_rtc_update.into_structured_config_value().into(),
            ),
        )?;
        builder.set_config_capability(
            "fuchsia.time.config.RtcAllowSettingPastUtc",
            Config::new(
                ConfigValueType::String { max_size: 64 },
                String::from(config.rtc_allow_setting_past_utc).into(),
            ),
        )?;
        builder.set_config_capability(
            "fuchsia.time.config.ServeFuchsiaTimeAlarms",
            Config::new(ConfigValueType::Bool, serve_fuchsia_time_alarms.into()),
        )?;

        // Refer to //src/sys/time/httpsdate_time_source/meta/service.cml
        // for details. Other httpsdate_time_source config fields use their
        // .cml defaults.
        builder.set_config_capability(
            "fuchsia.time.config.TimeSourceEndpointUrl",
            Config::new(
                ConfigValueType::String { max_size: 1024 },
                (&*config.time_source_endpoint_url).into(),
            ),
        )?;

        if has_hrtimer && matches!(context.build_type, BuildType::Eng) {
            match context.feature_set_level {
                FeatureSetLevel::Embeddable | FeatureSetLevel::Bootstrap => {}
                FeatureSetLevel::Utility | FeatureSetLevel::Standard => {
                    builder.platform_bundle("hrtimer-ctl")?;
                }
            }
        }
        Ok(())
    }
}
