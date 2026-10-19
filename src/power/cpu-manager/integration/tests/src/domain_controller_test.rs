// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_hardware_cpu_ctrl as fcpu_ctrl;
use fidl_fuchsia_power_cpu as fcpu;
use fidl_fuchsia_power_cpu_manager as fcpumanager;
use power_manager_integration_test_lib::{TestEnv, TestEnvBuilder};

const CPU_MANAGER_CONFIG_PATH: &'static str =
    "/pkg/domain_controller_test/cpu_manager_node_config.json5";
const CPU_MANAGER_BOOT_SEQUENCE_CONFIG_PATH: &'static str =
    "/pkg/domain_controller_test/cpu_manager_boot_sequence_node_config.json5";
const POWER_MANAGER_CONFIG_PATH: &'static str =
    "/pkg/domain_controller_test/power_manager_node_config.json5";

const DOMAIN_NAME: &'static str = "cluster0";
const DOMAIN_ID: u64 = 0;
const LOGICAL_CORE_IDS: &'static [u64] = &[0, 1, 2, 3];
const AVAILABLE_FREQUENCIES_HZ: &'static [u64] =
    &[20 * 10u64.pow(8), 15 * 10u64.pow(8), 15 * 10u64.pow(8)];

async fn create_test_env() -> TestEnv {
    TestEnvBuilder::new()
        .cpu_manager_node_config_path(CPU_MANAGER_CONFIG_PATH)
        .power_manager_node_config_path(POWER_MANAGER_CONFIG_PATH)
        .build()
        .await
}

async fn create_boot_sequence_test_env(cpu_boost_enabled: bool) -> TestEnv {
    TestEnvBuilder::new()
        .cpu_manager_node_config_path(CPU_MANAGER_BOOT_SEQUENCE_CONFIG_PATH)
        .power_manager_node_config_path(POWER_MANAGER_CONFIG_PATH)
        .cpu_boost_enabled(cpu_boost_enabled)
        .build()
        .await
}

async fn connect_to_cpu_ctrl(env: &TestEnv) -> fcpu_ctrl::DeviceProxy {
    env.connect_to_first_service_instance(fcpu_ctrl::ServiceMarker)
        .await
        .connect_to_device()
        .unwrap()
}

/// Polls until the domain's min/max frequency and fake-cpu-ctrl operating point limits match
/// expected values, allowing asynchronous lease drop notifications through Power Broker to settle.
async fn wait_for_domain_and_driver_state(
    domain_controller: &fcpu::DomainControllerProxy,
    cpu_ctrl: &fcpu_ctrl::DeviceProxy,
    expected_domain_min: u64,
    expected_domain_max: u64,
    expected_driver_current_opp: u32,
    expected_driver_min_opp: u32,
) {
    loop {
        let domain_min = domain_controller.get_min_frequency(DOMAIN_ID).await.unwrap().unwrap();
        let domain_max = domain_controller.get_max_frequency(DOMAIN_ID).await.unwrap().unwrap();
        let driver_current = cpu_ctrl.get_current_operating_point().await.unwrap();
        let (driver_min, _) = cpu_ctrl.get_current_operating_point_limits().await.unwrap().unwrap();
        if domain_min == expected_domain_min
            && domain_max == expected_domain_max
            && driver_current == expected_driver_current_opp
            && driver_min == expected_driver_min_opp
        {
            return;
        }
        fuchsia_async::Timer::new(std::time::Duration::from_millis(10)).await;
    }
}

#[fuchsia::test]
async fn test_list_domains() {
    let mut env = create_test_env().await;
    let domain_controller = env.connect_to_protocol::<fcpu::DomainControllerMarker>();
    let domains = domain_controller.list_domains().await.unwrap();

    assert_eq!(
        domains,
        vec![fcpu::DomainInfo {
            id: Some(DOMAIN_ID),
            core_ids: Some(LOGICAL_CORE_IDS.to_vec()),
            available_frequencies_hz: Some(AVAILABLE_FREQUENCIES_HZ.to_vec()),
            name: Some(DOMAIN_NAME.to_string()),
            ..Default::default()
        }]
    );

    env.destroy().await;
}

#[fuchsia::test]
async fn test_get_and_set_max_frequency() {
    let mut env = create_test_env().await;
    let domain_controller = env.connect_to_protocol::<fcpu::DomainControllerMarker>();
    assert_eq!(0, domain_controller.get_max_frequency(DOMAIN_ID).await.unwrap().unwrap());
    domain_controller.set_max_frequency(DOMAIN_ID, 1).await.unwrap().unwrap();
    assert_eq!(1, domain_controller.get_max_frequency(DOMAIN_ID).await.unwrap().unwrap());
    domain_controller.clear_max_frequency(DOMAIN_ID).await.unwrap().unwrap();
    assert_eq!(0, domain_controller.get_max_frequency(DOMAIN_ID).await.unwrap().unwrap());
    assert_eq!(
        Err(fcpu::DomainControllerError::InvalidArguments),
        domain_controller
            .set_max_frequency(DOMAIN_ID, AVAILABLE_FREQUENCIES_HZ.len() as u64)
            .await
            .unwrap()
    );
    env.destroy().await;
}

#[fuchsia::test]
async fn test_lease_min_and_max_frequency() {
    let mut env = create_test_env().await;
    let domain_controller = env.connect_to_protocol::<fcpu::DomainControllerMarker>();
    let cpu_ctrl = connect_to_cpu_ctrl(&env).await;
    let default_min_index = (AVAILABLE_FREQUENCIES_HZ.len() - 1) as u64;
    assert_eq!(
        default_min_index,
        domain_controller.get_min_frequency(DOMAIN_ID).await.unwrap().unwrap()
    );
    let min_lease =
        domain_controller.lease_min_frequency(DOMAIN_ID, 1, "test_min").await.unwrap().unwrap();
    assert_eq!(1, domain_controller.get_min_frequency(DOMAIN_ID).await.unwrap().unwrap());
    drop(min_lease);
    wait_for_domain_and_driver_state(
        &domain_controller,
        &cpu_ctrl,
        default_min_index,
        0,
        0,
        default_min_index as u32,
    )
    .await;
    assert_eq!(
        Err(fcpu::DomainControllerError::InvalidArguments),
        domain_controller
            .lease_min_frequency(DOMAIN_ID, AVAILABLE_FREQUENCIES_HZ.len() as u64, "bad")
            .await
            .unwrap()
    );

    let max_lease =
        domain_controller.lease_max_frequency(DOMAIN_ID, 1, "test_max").await.unwrap().unwrap();
    assert_eq!(1, domain_controller.get_max_frequency(DOMAIN_ID).await.unwrap().unwrap());
    drop(max_lease);
    wait_for_domain_and_driver_state(
        &domain_controller,
        &cpu_ctrl,
        default_min_index,
        0,
        0,
        default_min_index as u32,
    )
    .await;
    assert_eq!(
        Err(fcpu::DomainControllerError::InvalidArguments),
        domain_controller
            .lease_max_frequency(DOMAIN_ID, AVAILABLE_FREQUENCIES_HZ.len() as u64, "bad")
            .await
            .unwrap()
    );
    env.destroy().await;
}

#[fuchsia::test]
async fn test_boot_init_starnix_boost_and_thermal_hal_sequence() {
    let mut env = create_boot_sequence_test_env(true).await;
    let cpu_ctrl = connect_to_cpu_ctrl(&env).await;
    let boot_opp = 1u32;
    let idle_opp = (AVAILABLE_FREQUENCIES_HZ.len() - 1) as u32;
    let thermal_opp: u32 = 2;

    let domain_controller = env.connect_to_protocol::<fcpu::DomainControllerMarker>();
    let boost = env.connect_to_protocol::<fcpumanager::BoostMarker>();

    // Stage 1: Boot initialization (`boot_min_opp = 1`, `boot_max_opp = 1` applied at startup).
    wait_for_domain_and_driver_state(
        &domain_controller,
        &cpu_ctrl,
        boot_opp as u64,
        boot_opp as u64,
        boot_opp,
        boot_opp,
    )
    .await;

    // Stage 2: Starnix `fuchsia.power.cpu.manager.Boost`.
    // Enabling boost raises the minimum frequency floor to `boost_opp` (0). `GetMinFrequency`
    // continues to report the domain floor (`boot_min_opp = 1`) while boost is active.
    let boost_token = boost.boost().await.unwrap().unwrap();
    wait_for_domain_and_driver_state(
        &domain_controller,
        &cpu_ctrl,
        boot_opp as u64,
        boot_opp as u64,
        boot_opp,
        0,
    )
    .await;

    // Disabling boost (dropping the token) clears `boot_min_opp` and drops minimum frequency to
    // the idle floor (`opps.len() - 1 = 2`).
    drop(boost_token);
    wait_for_domain_and_driver_state(
        &domain_controller,
        &cpu_ctrl,
        idle_opp as u64,
        boot_opp as u64,
        boot_opp,
        idle_opp,
    )
    .await;

    // Verify `boot_min_opp` remains cleared across subsequent Floor leases.
    let floor_lease = domain_controller
        .lease_min_frequency(DOMAIN_ID, 1, "starnix:powerhint_min_freq")
        .await
        .unwrap()
        .unwrap();
    wait_for_domain_and_driver_state(
        &domain_controller,
        &cpu_ctrl,
        1,
        boot_opp as u64,
        boot_opp,
        1,
    )
    .await;
    drop(floor_lease);
    wait_for_domain_and_driver_state(
        &domain_controller,
        &cpu_ctrl,
        idle_opp as u64,
        boot_opp as u64,
        boot_opp,
        idle_opp,
    )
    .await;

    // Stage 3: Thermal HAL and Power Broker ceiling/floor arbitration.
    // Simulate Starnix `soft_max_freq` taking ownership of the boot ceiling before clearing the
    // legacy `SetMaxFrequency` cap.
    let soft_max_lease = domain_controller
        .lease_max_frequency(DOMAIN_ID, boot_opp as u64, "starnix:soft_max_freq")
        .await
        .unwrap()
        .unwrap();
    domain_controller.clear_max_frequency(DOMAIN_ID).await.unwrap().unwrap();
    wait_for_domain_and_driver_state(
        &domain_controller,
        &cpu_ctrl,
        idle_opp as u64,
        boot_opp as u64,
        boot_opp,
        idle_opp,
    )
    .await;

    // Thermal HAL initializes cooling state 0 (`SetMaxFrequency(0)`), which does not override the
    // active `soft_max_freq` ceiling (OPP 1).
    domain_controller.set_max_frequency(DOMAIN_ID, 0).await.unwrap().unwrap();
    wait_for_domain_and_driver_state(
        &domain_controller,
        &cpu_ctrl,
        idle_opp as u64,
        boot_opp as u64,
        boot_opp,
        idle_opp,
    )
    .await;

    // Thermal HAL throttles the domain to OPP 2 (`SetMaxFrequency(2)`), which is stricter than
    // `soft_max_freq` (OPP 1) and caps the driver to OPP 2.
    domain_controller.set_max_frequency(DOMAIN_ID, thermal_opp as u64).await.unwrap().unwrap();
    wait_for_domain_and_driver_state(
        &domain_controller,
        &cpu_ctrl,
        idle_opp as u64,
        thermal_opp as u64,
        thermal_opp,
        idle_opp,
    )
    .await;

    // Dropping `soft_max_freq` (on `sys.boot_completed=1`) while thermal throttling is active
    // keeps the domain capped at OPP 2.
    drop(soft_max_lease);
    wait_for_domain_and_driver_state(
        &domain_controller,
        &cpu_ctrl,
        idle_opp as u64,
        thermal_opp as u64,
        thermal_opp,
        idle_opp,
    )
    .await;

    // Clearing the thermal cap (`ClearMaxFrequency`) uncaps the domain to `thermal_states[0]` (OPP 0).
    domain_controller.clear_max_frequency(DOMAIN_ID).await.unwrap().unwrap();
    wait_for_domain_and_driver_state(
        &domain_controller,
        &cpu_ctrl,
        idle_opp as u64,
        0,
        0,
        idle_opp,
    )
    .await;

    env.destroy().await;
}

#[fuchsia::test]
async fn test_boost_disabled_returns_not_supported() {
    let mut env = create_boot_sequence_test_env(false).await;
    let domain_controller = env.connect_to_protocol::<fcpu::DomainControllerMarker>();
    let boost = env.connect_to_protocol::<fcpumanager::BoostMarker>();
    let cpu_ctrl = connect_to_cpu_ctrl(&env).await;

    // When `BoostEnabled` is false (e.g. headless minimal builds), `Boost()` returns `NotSupported`
    // and `boot_min_opp` (OPP 1) remains pinned.
    assert_eq!(Err(fcpumanager::BoostError::NotSupported), boost.boost().await.unwrap());
    wait_for_domain_and_driver_state(&domain_controller, &cpu_ctrl, 1, 1, 1, 1).await;

    env.destroy().await;
}
