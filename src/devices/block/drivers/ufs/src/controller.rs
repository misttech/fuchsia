// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Low-level UFS Host Controller Interface (UFSHCI) MMIO operations.
//! Manages controller enablement, interrupt configuration, auto-hibernate, and UIC DME execution.
//!
//! [`UfsController`] owns the register block behind a private lock, so it can be shared freely
//! (`Arc<UfsController<M>>`) between the transfer request processor, the interrupt handler, and
//! administrative callers. The lock is held only around individual register accesses: every
//! `async` method here releases it before awaiting, so no caller can block the dispatcher
//! thread on it.

use crate::constants::{
    AUTO_HIBERNATE_MAX_TIMER_SCALE, AUTO_HIBERNATE_MAX_TIMER_VALUE, HOST_CONTROLLER_TIMEOUT,
    LINK_STARTUP_ATTEMPTS, LINK_STARTUP_RETRY_DELAY, PA_PWRMODE, UIC_PHY_ADAPTER_LAYER_LINE_RESET,
};
use crate::registers::{
    AutoHibernateIdleTimerReg, CapabilityReg, HostControllerEnableReg, HostControllerStatusReg,
    HostUicErrorCodeDataLinkLayerReg, HostUicErrorCodeDmeReg, HostUicErrorCodeNetworkLayerReg,
    HostUicErrorCodePhyAdapterLayerReg, HostUicErrorCodeTransportLayerReg, InterruptEnableReg,
    InterruptStatusReg, PowerModeStatus, UtrListDoorBellReg,
};
use crate::uic::{UicCommand, UicCommandOpcode};
use crate::{UfsError, uic};
use fuchsia_sync::Mutex;
use log::{debug, error, info, warn};
use mmio::{Mmio, ReadableRegister, Register, WritableRegister};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Register block and UIC ownership state, guarded by [`UfsController::state`].
struct ControllerState<M: Mmio> {
    mmio: M,
    /// Set while a UIC command owns the UIC registers.
    uic_busy: bool,
    /// Number of active [`AutoHibernateHoldGuard`]s holding Auto-Hibernate disabled across
    /// multi-DME sequences.
    ah8_hold_count: u32,
    /// Configured `AHIT` register value (`0` when Auto-Hibernate is disabled). Cleared on hardware
    /// while `uic_busy` is true or `ah8_hold_count > 0`.
    ah8_timer_raw: u32,
    /// Set once `configure_auto_hibernate` has logged the initial `AHIT` timer at `INFO` level so
    /// subsequent runtime `Sleep`/wake cycles log at `DEBUG` instead of waking `archivist`/`fxfs`.
    ah8_logged: bool,
}

impl<M: Mmio> ControllerState<M> {
    fn is_ah8_enabled_on_hw(&self) -> bool {
        !self.uic_busy && self.ah8_hold_count == 0 && self.ah8_timer_raw != 0
    }

    /// Writes `HCE = enable`.
    fn write_host_controller_enable(&mut self, enable: bool) {
        let mut hce = HostControllerEnableReg::from_raw(0);
        hce.set_host_controller_enable(enable);
        hce.write(&mut self.mmio);
    }

    /// Writes `IE = 0`.
    fn write_interrupts_disabled(&mut self) {
        InterruptEnableReg::from_raw(0).write(&mut self.mmio);
    }

    /// Enables or disables the `UHESE` and `UHXSE` interrupt bits in `IE` when controller
    /// interrupts are currently enabled (`IE != 0`).
    fn set_ah8_interrupts_enabled(&mut self, enabled: bool) {
        let mut enable = InterruptEnableReg::read(&self.mmio);
        if enable.0 == 0 {
            return;
        }
        enable.set_uic_hibernate_enter_status_enable(enabled);
        enable.set_uic_hibernate_exit_status_enable(enabled);
        enable.write(&mut self.mmio);
    }

    /// Disables Auto-Hibernate on hardware (`AHIT = 0`, `IE.UHESE = 0`, `IE.UHXSE = 0`,
    /// b/572020288).
    fn disable_ah8_on_hw(&mut self) {
        self.set_ah8_interrupts_enabled(false);
        AutoHibernateIdleTimerReg::from_raw(0).write(&mut self.mmio);
    }

    /// Restores Auto-Hibernate on hardware (`AHIT` and `IE.UHESE`/`IE.UHXSE`) if configured and
    /// no UIC command or hold guard is active.
    fn restore_ah8_on_hw(&mut self) {
        if self.is_ah8_enabled_on_hw() {
            AutoHibernateIdleTimerReg::from_raw(self.ah8_timer_raw).write(&mut self.mmio);
            self.set_ah8_interrupts_enabled(true);
        }
    }

    /// Returns `Some(())` once `HCE` reads back as `enable`; for use with [`uic::poll_until`].
    fn host_controller_enable_is(&self, enable: bool) -> Option<()> {
        (HostControllerEnableReg::read(&self.mmio).host_controller_enable() == enable).then_some(())
    }

    /// Reads (and thereby clears) the UIC error code registers, logs any set errors, and returns
    /// whether any fatal UIC error occurred (PA error other than line reset, DL, NL, TL, or DME).
    fn log_uic_errors(&self) -> bool {
        let pa = HostUicErrorCodePhyAdapterLayerReg::read(&self.mmio);
        let dl = HostUicErrorCodeDataLinkLayerReg::read(&self.mmio);
        let n = HostUicErrorCodeNetworkLayerReg::read(&self.mmio);
        let t = HostUicErrorCodeTransportLayerReg::read(&self.mmio);
        let dme = HostUicErrorCodeDmeReg::read(&self.mmio);
        warn!(uecpa = pa.0, uecdl = dl.0, uecn = n.0, uect = t.0, uecdme = dme.0; "UFS UIC error");
        (pa.uic_phy_adapter_layer_error()
            && pa.uic_phy_adapter_layer_error_code() != UIC_PHY_ADAPTER_LAYER_LINE_RESET)
            || dl.uic_data_link_layer_error()
            || n.uic_network_layer_error()
            || t.uic_transport_layer_error()
            || dme.uic_dme_error()
    }

    /// Logs a snapshot of the UFSHCI status and doorbell registers before link recovery.
    fn dump_debug_registers(&self, status: InterruptStatusReg) {
        let hcs = HostControllerStatusReg::read(&self.mmio);
        let ahit = AutoHibernateIdleTimerReg::read(&self.mmio);
        let utrldbr = UtrListDoorBellReg::read(&self.mmio);
        error!(
            is = status.0,
            hcs = hcs.0,
            upmcrs = hcs.uic_power_mode_change_request_status(),
            ahit = ahit.0,
            utrldbr = utrldbr.0;
            "UFS controller state dump"
        );
    }
}

/// UfsController encapsulates low-level MMIO interactions with the UFS controller.
pub struct UfsController<M: Mmio> {
    state: Mutex<ControllerState<M>>,
    recovery_needed: AtomicBool,
    auto_hibern8_error_count: AtomicU64,
    #[cfg(test)]
    host_controller_timeout_ns: AtomicU64,
}

impl<M: Mmio> std::fmt::Debug for UfsController<M> {
    /// Reports only the lock-free fields; `state` is not read so `fmt` never blocks on the
    /// register lock.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UfsController")
            .field("recovery_needed", &self.recovery_needed.load(Ordering::Relaxed))
            .field(
                "auto_hibern8_error_count",
                &self.auto_hibern8_error_count.load(Ordering::Relaxed),
            )
            .finish_non_exhaustive()
    }
}

impl<M: Mmio> UfsController<M> {
    /// Creates a new UfsController wrapping the provided MMIO region.
    pub fn new(mmio: M) -> Self {
        Self {
            state: Mutex::new(ControllerState {
                mmio,
                uic_busy: false,
                ah8_hold_count: 0,
                ah8_timer_raw: 0,
                ah8_logged: false,
            }),
            recovery_needed: AtomicBool::new(false),
            auto_hibern8_error_count: AtomicU64::new(0),
            #[cfg(test)]
            host_controller_timeout_ns: AtomicU64::new(0),
        }
    }

    /// Marks the controller and link as needing full recovery (`https://fxbug.dev/572019773`).
    pub fn request_link_recovery(&self) {
        self.recovery_needed.store(true, Ordering::SeqCst);
    }

    /// Returns whether link recovery has been requested.
    pub fn is_recovery_needed(&self) -> bool {
        self.recovery_needed.load(Ordering::SeqCst)
    }

    /// Atomically takes and clears any pending link recovery request.
    pub fn take_recovery_needed(&self) -> bool {
        self.recovery_needed.swap(false, Ordering::SeqCst)
    }

    /// Returns the cumulative count of autonomous Auto-Hibernate enter/exit errors observed.
    pub fn auto_hibern8_error_count(&self) -> u64 {
        self.auto_hibern8_error_count.load(Ordering::Relaxed)
    }

    /// Runs `f` with exclusive access to the register block. `f` runs under the controller lock
    /// and must not block; the lock cannot be held across an `.await`.
    pub fn with_mmio<R>(&self, f: impl FnOnce(&mut M) -> R) -> R {
        f(&mut self.state.lock().mmio)
    }

    /// Disables all controller interrupts.
    pub fn disable_interrupts(&self) {
        self.state.lock().write_interrupts_disabled();
    }

    /// Enables standard controller interrupts.
    pub fn enable_interrupts(&self) {
        let mut state = self.state.lock();
        let ah8_enabled = state.is_ah8_enabled_on_hw();
        let mut enable = InterruptEnableReg::from_raw(0);
        enable.set_utp_transfer_request_completion_enable(true);
        enable.set_uic_error_enable(true);
        enable.set_uic_hibernate_exit_status_enable(ah8_enabled);
        enable.set_uic_hibernate_enter_status_enable(ah8_enabled);
        enable.set_uic_link_lost_status_enable(true);
        enable.set_utp_error_enable(true);
        enable.set_device_fatal_error_enable(true);
        enable.set_host_controller_fatal_error_enable(true);
        enable.set_system_bus_fatal_error_enable(true);
        enable.set_crypto_engine_fatal_error_enable(true);
        enable.write(&mut state.mmio);
    }

    /// Reads and clears the active interrupt status bits it handles, logging error sources and
    /// flagging link recovery on Auto-Hibernate, link-lost, or fatal controller/device errors.
    pub fn fetch_and_clear_interrupts(&self) -> InterruptStatusReg {
        let mut state = self.state.lock();
        let uic_busy = state.uic_busy;
        let ah8_configured = state.ah8_timer_raw != 0;
        let status = InterruptStatusReg::read(&state.mmio);
        let mut clear = InterruptStatusReg::default();
        let mut trigger_recovery = false;
        if status.utp_transfer_request_completion_status() {
            clear.set_utp_transfer_request_completion_status(true);
        }
        // Summary of `CQIS`, which the MCQ handler clears.
        if status.cq_event_status() {
            clear.set_cq_event_status(true);
        }
        if status.uic_error() {
            let fatal_uic = state.log_uic_errors();
            clear.set_uic_error(true);
            if fatal_uic && !uic_busy {
                trigger_recovery = true;
            }
        }
        if !uic_busy && (status.uic_hibernate_enter_status() || status.uic_hibernate_exit_status())
        {
            let hcs = HostControllerStatusReg::read(&state.mmio);
            let upmcrs = hcs.uic_power_mode_change_request_status();
            let pm_ok = matches!(
                hcs.power_mode_status(),
                Some(
                    PowerModeStatus::PowerOk
                        | PowerModeStatus::PowerLocal
                        | PowerModeStatus::PowerRemote
                )
            );
            if status.uic_hibernate_enter_status() {
                clear.set_uic_hibernate_enter_status(true);
                if ah8_configured || !pm_ok {
                    error!(status = status.0, upmcrs; "UFS: Auto Hibern8 enter failed");
                    self.auto_hibern8_error_count.fetch_add(1, Ordering::Relaxed);
                    trigger_recovery = true;
                }
            }
            if status.uic_hibernate_exit_status() {
                clear.set_uic_hibernate_exit_status(true);
                if ah8_configured || !pm_ok {
                    error!(status = status.0, upmcrs; "UFS: Auto Hibern8 exit failed");
                    self.auto_hibern8_error_count.fetch_add(1, Ordering::Relaxed);
                    trigger_recovery = true;
                }
            }
        }
        if status.uic_link_lost_status() {
            error!("UFS UIC link lost interrupt detected!");
            clear.set_uic_link_lost_status(true);
            trigger_recovery = true;
        }
        if status.utp_error_status() {
            let hcs = HostControllerStatusReg::read(&state.mmio);
            error!(
                code:? = hcs.error_code(),
                task_tag = hcs.task_tag_of_utp_error(),
                lun = hcs.target_lun_of_utp_error();
                "UFS UTP error"
            );
            clear.set_utp_error_status(true);
        }
        if status.device_fatal_error_status() {
            error!("UFS device fatal error interrupt detected!");
            clear.set_device_fatal_error_status(true);
            trigger_recovery = true;
        }
        if status.host_controller_fatal_error_status() {
            error!("UFS host controller fatal error interrupt detected!");
            clear.set_host_controller_fatal_error_status(true);
            trigger_recovery = true;
        }
        if status.system_bus_fatal_error_status() {
            error!("UFS system bus fatal error interrupt detected!");
            clear.set_system_bus_fatal_error_status(true);
            trigger_recovery = true;
        }
        if status.crypto_engine_fatal_error_status() {
            error!("UFS crypto engine fatal error interrupt detected!");
            clear.set_crypto_engine_fatal_error_status(true);
            trigger_recovery = true;
        }
        if trigger_recovery {
            state.dump_debug_registers(status);
            self.request_link_recovery();
        }
        if clear.0 != 0 {
            clear.write(&mut state.mmio);
        }
        status
    }

    /// Configures Auto-Hibernate (AH8) with an idle timer of about `timer_us` microseconds if
    /// supported. `timer_us == 0` disables auto-hibernate and clears `IE.UHESE`/`IE.UHXSE` before
    /// zeroing `AHIT` so software hibernate polling cannot race with the interrupt handler.
    pub fn configure_auto_hibernate(&self, timer_us: u32) {
        let mut state = self.state.lock();
        if !CapabilityReg::read(&state.mmio).auto_hibernation_support() {
            return;
        }
        if timer_us == 0 {
            state.ah8_timer_raw = 0;
            state.disable_ah8_on_hw();
            debug!("UFS Auto-Hibernate (AH8) disabled");
            return;
        }
        // Scale n means units of 10^n us.
        let mut value = timer_us;
        let mut scale = 0;
        while value > AUTO_HIBERNATE_MAX_TIMER_VALUE && scale < AUTO_HIBERNATE_MAX_TIMER_SCALE {
            value = value.div_ceil(10);
            scale += 1;
        }
        let mut ahit = AutoHibernateIdleTimerReg::from_raw(0);
        ahit.set_timer_scale(scale);
        ahit.set_timer_value(value.min(AUTO_HIBERNATE_MAX_TIMER_VALUE));
        state.ah8_timer_raw = ahit.0;
        state.restore_ah8_on_hw();
        if !state.ah8_logged {
            state.ah8_logged = true;
            info!(timer_us, scale, value; "UFS Auto-Hibernate (AH8) timer set");
        } else {
            debug!(timer_us, scale, value; "UFS Auto-Hibernate (AH8) timer set");
        }
    }

    /// Returns whether autonomous Auto-Hibernate is currently active in hardware (`AHIT != 0` and
    /// not held off by an active UIC command or [`AutoHibernateHoldGuard`]).
    pub fn is_auto_hibernate_active(&self) -> bool {
        self.state.lock().is_ah8_enabled_on_hw()
    }

    /// Holds autonomous Auto-Hibernate (`AHIT = 0`, `IE.UHESE = 0`, `IE.UHXSE = 0`) disabled for
    /// the lifetime of the returned [`AutoHibernateHoldGuard`] and waits for `HCS.UCRDY == 1`.
    /// Used around multi-command UIC sequences such as gear changes so `AHIT` is not restored
    /// between back-to-back `DME_GET`/`DME_SET` commands.
    pub async fn hold_auto_hibernate(
        &self,
        timeout: zx::MonotonicDuration,
    ) -> Result<AutoHibernateHoldGuard<'_, M>, UfsError> {
        {
            let mut state = self.state.lock();
            let was_enabled = state.is_ah8_enabled_on_hw();
            state.ah8_hold_count =
                state.ah8_hold_count.checked_add(1).expect("ah8_hold_count overflow");
            if was_enabled {
                state.disable_ah8_on_hw();
            }
        }
        let guard = AutoHibernateHoldGuard { controller: self };
        uic::poll_until(timeout, || {
            let mut state = self.state.lock();
            if state.uic_busy || !uic::uic_command_ready(&state.mmio) {
                return None;
            }
            uic::clear_interrupt_status(&mut state.mmio, |is| {
                is.set_uic_hibernate_enter_status(true);
                is.set_uic_hibernate_exit_status(true);
            });
            Some(())
        })
        .await?;
        Ok(guard)
    }

    /// Returns whether the device answered link startup (`HCS.DP`).
    #[cfg(test)]
    pub(crate) fn device_present(&self) -> bool {
        HostControllerStatusReg::read(&self.state.lock().mmio).device_present()
    }

    /// Returns whether a UIC command currently owns the UIC registers.
    #[cfg(test)]
    pub(crate) fn uic_busy(&self) -> bool {
        self.state.lock().uic_busy
    }

    /// Overrides [`HOST_CONTROLLER_TIMEOUT`] for `HCE` polling in unit tests.
    #[cfg(test)]
    pub(crate) fn set_host_controller_timeout_for_test(&self, timeout: zx::MonotonicDuration) {
        self.host_controller_timeout_ns.store(timeout.into_nanos() as u64, Ordering::Relaxed);
    }

    fn host_controller_timeout(&self) -> zx::MonotonicDuration {
        #[cfg(test)]
        {
            let ns = self.host_controller_timeout_ns.load(Ordering::Relaxed);
            if ns != 0 {
                return zx::MonotonicDuration::from_nanos(ns as i64);
            }
        }
        HOST_CONTROLLER_TIMEOUT
    }

    /// Waits for `HCE` to read back as `enable`.
    async fn wait_host_controller_enable(&self, enable: bool) -> Result<(), UfsError> {
        uic::poll_until(self.host_controller_timeout(), || {
            self.state.lock().host_controller_enable_is(enable)
        })
        .await
    }

    /// Disables interrupts and resets/enables the host controller (`HCE`).
    ///
    /// # Errors
    ///
    /// Returns `Timeout` if `HCE` does not change within [`HOST_CONTROLLER_TIMEOUT`].
    pub async fn reset_and_enable(&self) -> Result<(), UfsError> {
        let was_enabled = {
            let mut state = self.state.lock();
            self.recovery_needed.store(false, Ordering::SeqCst);
            state.ah8_timer_raw = 0;
            state.ah8_logged = false;
            state.write_interrupts_disabled();
            let enabled = HostControllerEnableReg::read(&state.mmio).host_controller_enable();
            if enabled {
                state.write_host_controller_enable(false);
            }
            enabled
        };
        if was_enabled {
            self.wait_host_controller_enable(false).await?;
        }
        self.state.lock().write_host_controller_enable(true);
        self.wait_host_controller_enable(true).await
    }

    /// Disables interrupts, writes `HCE = 0` and waits for `HCE` to read 0. Once this returns
    /// `Ok`, the controller performs no more DMA.
    ///
    /// # Errors
    ///
    /// Returns `Timeout` if `HCE` does not read 0 within [`HOST_CONTROLLER_TIMEOUT`].
    pub async fn disable_host_controller(&self) -> Result<(), UfsError> {
        {
            let mut state = self.state.lock();
            state.ah8_timer_raw = 0;
            state.ah8_logged = false;
            state.write_interrupts_disabled();
            state.write_host_controller_enable(false);
        }
        self.wait_host_controller_enable(false).await
    }

    /// Waits until `HCS.UCRDY` is 1 so any in-flight autonomous Auto-Hibernate transition has
    /// settled.
    pub async fn wait_uic_command_ready(
        &self,
        timeout: zx::MonotonicDuration,
    ) -> Result<(), UfsError> {
        uic::poll_until(timeout, || {
            let state = self.state.lock();
            uic::uic_command_ready(&state.mmio).then_some(())
        })
        .await
    }

    /// Claims the UIC registers with Auto-Hibernate held disabled (`AHIT = 0`, `UHESE = 0`,
    /// `UHXSE = 0`), waits for `HCS.UCRDY`, runs `before_issue`, and issues `command`.
    async fn issue_uic_with_guard(
        &self,
        command: &UicCommand,
        timeout: zx::MonotonicDuration,
        before_issue: impl FnOnce(&mut M),
    ) -> Result<UicBusyGuard<'_, M>, UfsError> {
        if !command.opcode.completes_with_uccs() {
            return Err(UfsError::InvalidParameter);
        }
        let mut guard: Option<UicBusyGuard<'_, M>> = None;
        let mut before_issue = Some(before_issue);
        uic::poll_until(timeout, || {
            let mut state = self.state.lock();
            if guard.is_none() {
                if state.uic_busy {
                    return None;
                }
                let was_ah8_enabled = state.is_ah8_enabled_on_hw();
                state.uic_busy = true;
                if was_ah8_enabled {
                    state.disable_ah8_on_hw();
                }
                guard = Some(UicBusyGuard { controller: self });
            }
            if !uic::uic_command_ready(&state.mmio) {
                return None;
            }
            if let Some(hook) = before_issue.take() {
                hook(&mut state.mmio);
            }
            uic::issue_uic_command(&mut state.mmio, command);
            Some(())
        })
        .await?;
        Ok(guard.expect("UicBusyGuard must be acquired before poll_until completes"))
    }

    /// Polls `IS.UCCS` for an already-issued UIC command while the caller holds [`UicBusyGuard`].
    async fn poll_uic_completion(
        &self,
        opcode: UicCommandOpcode,
        timeout: zx::MonotonicDuration,
    ) -> Result<u32, UfsError> {
        let result = uic::poll_until(timeout, || {
            self.with_mmio(|mmio| uic::poll_uic_command_completion(mmio, opcode))
        })
        .await;
        if result.is_err() {
            self.with_mmio(|mmio| {
                uic::clear_interrupt_status(mmio, |is| is.set_uic_command_completion_status(true))
            });
        }
        result?
    }

    /// Sends a UIC command that completes through `IS.UCCS`. Concurrent commands are serialized.
    ///
    /// # Errors
    ///
    /// Returns `InvalidParameter` for hibernate commands, `Timeout`, or `UicError`.
    pub async fn send_uic_command(&self, command: &UicCommand) -> Result<u32, UfsError> {
        let _busy = self.issue_uic_with_guard(command, HOST_CONTROLLER_TIMEOUT, |_| {}).await?;
        self.poll_uic_completion(command.opcode, HOST_CONTROLLER_TIMEOUT).await
    }

    /// Runs `DME_LINKSTARTUP` up to [`LINK_STARTUP_ATTEMPTS`] times until it succeeds and
    /// `HCS.DP` reads 1, then clears `IS.UE` left over from link training.
    ///
    /// # Errors
    ///
    /// Returns the last UIC error, or `IoError` if the device never became present.
    pub async fn link_startup(&self) -> Result<(), UfsError> {
        let mut last_error = UfsError::IoError;
        let command = UicCommand::new(UicCommandOpcode::DmeLinkStartup);
        for attempt in 1..=LINK_STARTUP_ATTEMPTS {
            match self.send_uic_command(&command).await {
                Ok(_) => {
                    let mut state = self.state.lock();
                    if HostControllerStatusReg::read(&state.mmio).device_present() {
                        uic::clear_interrupt_status(&mut state.mmio, |is| is.set_uic_error(true));
                        return Ok(());
                    }
                    drop(state);
                    warn!(attempt; "UFS device not present after link startup");
                }
                Err(e) => {
                    warn!(attempt, err:? = e; "UFS link startup attempt failed");
                    last_error = e;
                }
            }
            if attempt < LINK_STARTUP_ATTEMPTS {
                fuchsia_async::Timer::new(zx::MonotonicInstant::after(LINK_STARTUP_RETRY_DELAY))
                    .await;
            }
        }
        Err(last_error)
    }

    /// Sends `DME_GET` (local). See [`Self::send_uic_command`].
    pub async fn dme_get(
        &self,
        mib_attribute: u16,
        gen_selector_index: u16,
    ) -> Result<u32, UfsError> {
        let command =
            UicCommand::dme(UicCommandOpcode::DmeGet, mib_attribute, gen_selector_index, 0);
        self.send_uic_command(&command).await
    }

    /// Sends volatile `DME_SET` (local). See [`Self::send_uic_command`].
    pub async fn dme_set(
        &self,
        mib_attribute: u16,
        gen_selector_index: u16,
        value: u32,
    ) -> Result<(), UfsError> {
        let command =
            UicCommand::dme(UicCommandOpcode::DmeSet, mib_attribute, gen_selector_index, value);
        self.send_uic_command(&command).await.map(|_| ())
    }

    /// Sends `DME_PEER_GET` (device). See [`Self::send_uic_command`].
    pub async fn dme_peer_get(
        &self,
        mib_attribute: u16,
        gen_selector_index: u16,
    ) -> Result<u32, UfsError> {
        let command =
            UicCommand::dme(UicCommandOpcode::DmePeerGet, mib_attribute, gen_selector_index, 0);
        self.send_uic_command(&command).await
    }

    /// Sends volatile `DME_PEER_SET` (device). See [`Self::send_uic_command`].
    pub async fn dme_peer_set(
        &self,
        mib_attribute: u16,
        gen_selector_index: u16,
        value: u32,
    ) -> Result<(), UfsError> {
        let command =
            UicCommand::dme(UicCommandOpcode::DmePeerSet, mib_attribute, gen_selector_index, value);
        self.send_uic_command(&command).await.map(|_| ())
    }

    /// Sets `PA_PWRMode` and waits for `IS.UPMS`, then requires `HCS.UPMCRS == PWR_LOCAL`
    /// (UFSHCI 3.0/4.0 section 7.4).
    ///
    /// Holds the UIC registers across the entire power-mode transition so concurrent UIC callers
    /// cannot interleave between `DME_SET(PA_PWRMode)` and `IS.UPMS`.
    ///
    /// # Errors
    ///
    /// Returns `Timeout`, or `UicError` with the `DME_SET` result code or the `UPMCRS` value.
    pub async fn change_power_mode(
        &self,
        power_mode: u32,
        timeout: zx::MonotonicDuration,
    ) -> Result<(), UfsError> {
        let command = UicCommand::dme(UicCommandOpcode::DmeSet, PA_PWRMODE, 0, power_mode);
        let _busy = self
            .issue_uic_with_guard(&command, timeout, |mmio| {
                uic::clear_interrupt_status(mmio, |is| is.set_uic_power_mode_status(true));
            })
            .await?;
        self.poll_uic_completion(command.opcode, timeout).await?;
        uic::poll_until(timeout, || {
            let mut state = self.state.lock();
            if !InterruptStatusReg::read(&state.mmio).uic_power_mode_status() {
                return None;
            }
            let result = uic::check_power_mode_status(&state.mmio, UicCommandOpcode::DmeSet);
            uic::clear_interrupt_status(&mut state.mmio, |is| is.set_uic_power_mode_status(true));
            Some(result)
        })
        .await?
    }
}

/// Scope guard that inhibits Auto-Hibernate (`AHIT`, b/572020288). Used to prevent hibernate
/// during multi-command UIC sequences such as gear changes.
#[must_use = "dropping the guard immediately re-enables Auto-Hibernate"]
pub struct AutoHibernateHoldGuard<'a, M: Mmio> {
    controller: &'a UfsController<M>,
}

impl<M: Mmio> Drop for AutoHibernateHoldGuard<'_, M> {
    fn drop(&mut self) {
        let mut state = self.controller.state.lock();
        state.ah8_hold_count = state.ah8_hold_count.saturating_sub(1);
        state.restore_ah8_on_hw();
    }
}

/// Releases the UIC registers when a UIC command finishes or is cancelled.
#[must_use = "dropping the guard immediately releases the UIC registers"]
struct UicBusyGuard<'a, M: Mmio> {
    controller: &'a UfsController<M>,
}

impl<M: Mmio> Drop for UicBusyGuard<'_, M> {
    fn drop(&mut self) {
        let mut state = self.controller.state.lock();
        state.uic_busy = false;
        state.restore_ah8_on_hw();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::{PA_MAXRXHSGEAR, PA_TXGEAR};
    use crate::registers::{UicCommandArgument1Reg, UicCommandReg};
    use crate::testing::fake_ufs::{
        CAP_AUTO_HIBERNATE_SUPPORT, FakeMmio, HCS_DEVICE_PRESENT, complete_uic_command,
        hcs_power_mode_status, uic_hardware, uic_opcode, wait_until,
    };

    #[fuchsia::test]
    async fn test_reset_enable_and_link_startup() {
        let vmo = zx::Vmo::create(1024).unwrap();
        let mut regs = uic_hardware(&vmo, 0);

        let hardware = std::thread::spawn(move || {
            assert!(wait_until(|| uic_opcode(&regs) == UicCommandOpcode::DmeLinkStartup as u32));
            // The device answers link startup: HCS.DP.
            let hcs = regs.load32(HostControllerStatusReg::OFFSET);
            regs.store32(HostControllerStatusReg::OFFSET, hcs | HCS_DEVICE_PRESENT);
            complete_uic_command(&mut regs, 0, 0, |_| {});
        });

        let controller = UfsController::new(FakeMmio::new(&vmo, 1024));
        controller.reset_and_enable().await.unwrap();
        assert!(
            controller
                .with_mmio(|mmio| HostControllerEnableReg::read(mmio).host_controller_enable())
        );

        controller.link_startup().await.unwrap();
        assert!(controller.device_present());
        hardware.join().unwrap();
    }

    #[fuchsia::test]
    async fn test_send_uic_command() {
        let vmo = zx::Vmo::create(1024).unwrap();
        let controller = UfsController::new(FakeMmio::new(&vmo, 1024));
        let mut regs = uic_hardware(&vmo, 0);

        let hardware = std::thread::spawn(move || {
            assert!(wait_until(|| uic_opcode(&regs) == UicCommandOpcode::DmeGet as u32));
            let arg1 = UicCommandArgument1Reg::read(&regs);
            assert_eq!(arg1.mib_attribute() as u16, PA_MAXRXHSGEAR);
            complete_uic_command(&mut regs, 0, 0xABCD, |_| {});
            regs
        });

        let value = controller.dme_get(PA_MAXRXHSGEAR, 0).await;
        assert_eq!(value, Ok(0xABCD));
        let regs = hardware.join().unwrap();
        assert!(!InterruptStatusReg::read(&regs).uic_command_completion_status());
        assert!(!controller.uic_busy());
    }

    #[fuchsia::test]
    async fn test_send_uic_command_error_clears_status() {
        let vmo = zx::Vmo::create(1024).unwrap();
        let controller = UfsController::new(FakeMmio::new(&vmo, 1024));
        let mut regs = uic_hardware(&vmo, 0);

        let hardware = std::thread::spawn(move || {
            assert!(wait_until(|| uic_opcode(&regs) == UicCommandOpcode::DmeSet as u32));
            complete_uic_command(&mut regs, 1, 0, |_| {});
            regs
        });

        let result = controller.dme_set(PA_TXGEAR, 0, 4).await;
        assert_eq!(
            result,
            Err(UfsError::UicError { opcode: UicCommandOpcode::DmeSet as u8, code: 1 })
        );
        let regs = hardware.join().unwrap();
        assert!(!InterruptStatusReg::read(&regs).uic_command_completion_status());
        assert!(!controller.uic_busy());
    }

    #[fuchsia::test]
    async fn test_send_uic_command_rejects_hibernate() {
        let vmo = zx::Vmo::create(1024).unwrap();
        let controller = UfsController::new(FakeMmio::new(&vmo, 1024));
        let _regs = uic_hardware(&vmo, 0);
        let result = controller
            .send_uic_command(&UicCommand::new(UicCommandOpcode::DmeHibernateEnter))
            .await;
        assert_eq!(result, Err(UfsError::InvalidParameter));
        assert_eq!(controller.with_mmio(|mmio| UicCommandReg::read(mmio).command_opcode()), 0);
    }

    #[fuchsia::test]
    async fn test_auto_hibernate_toggles_uhese_uhxse_and_holds_during_dme() {
        let vmo = zx::Vmo::create(1024).unwrap();
        let controller = UfsController::new(FakeMmio::new(&vmo, 1024));
        let mut regs = uic_hardware(&vmo, 0);
        regs.store32(CapabilityReg::OFFSET, CAP_AUTO_HIBERNATE_SUPPORT);

        controller.enable_interrupts();
        let ie = controller.with_mmio(|mmio| InterruptEnableReg::read(mmio));
        assert!(!ie.uic_hibernate_enter_status_enable());
        assert!(!ie.uic_hibernate_exit_status_enable());
        assert!(ie.uic_link_lost_status_enable());

        controller.configure_auto_hibernate(2000);
        let expected_ahit = controller.with_mmio(|mmio| AutoHibernateIdleTimerReg::read(mmio).0);
        assert_ne!(expected_ahit, 0);
        let ie = controller.with_mmio(|mmio| InterruptEnableReg::read(mmio));
        assert!(ie.uic_hibernate_enter_status_enable());
        assert!(ie.uic_hibernate_exit_status_enable());

        // While a DME command is in flight, AHIT is cleared to 0 and UHESE/UHXSE are disabled so
        // autonomous AH8 cannot race with the UIC command.
        let hardware = std::thread::spawn(move || {
            assert!(wait_until(|| uic_opcode(&regs) == UicCommandOpcode::DmeGet as u32));
            assert_eq!(AutoHibernateIdleTimerReg::read(&regs).0, 0);
            let ie_during = InterruptEnableReg::read(&regs);
            assert!(!ie_during.uic_hibernate_enter_status_enable());
            assert!(!ie_during.uic_hibernate_exit_status_enable());
            complete_uic_command(&mut regs, 0, 0x1234, |_| {});
        });

        assert_eq!(controller.dme_get(PA_MAXRXHSGEAR, 0).await, Ok(0x1234));
        hardware.join().unwrap();

        // After the DME command finishes, AHIT and UHESE/UHXSE are restored.
        assert_eq!(
            controller.with_mmio(|mmio| AutoHibernateIdleTimerReg::read(mmio).0),
            expected_ahit
        );
        let ie = controller.with_mmio(|mmio| InterruptEnableReg::read(mmio));
        assert!(ie.uic_hibernate_enter_status_enable());
        assert!(ie.uic_hibernate_exit_status_enable());

        // Disabling AH8 clears UHESE/UHXSE and AHIT.
        controller.configure_auto_hibernate(0);
        assert_eq!(controller.with_mmio(|mmio| AutoHibernateIdleTimerReg::read(mmio).0), 0);
        let ie = controller.with_mmio(|mmio| InterruptEnableReg::read(mmio));
        assert!(!ie.uic_hibernate_enter_status_enable());
        assert!(!ie.uic_hibernate_exit_status_enable());
    }

    #[fuchsia::test]
    async fn test_hold_auto_hibernate_across_multiple_dme_commands() {
        let vmo = zx::Vmo::create(1024).unwrap();
        let controller = UfsController::new(FakeMmio::new(&vmo, 1024));
        let mut regs = uic_hardware(&vmo, 0);
        regs.store32(CapabilityReg::OFFSET, CAP_AUTO_HIBERNATE_SUPPORT);

        controller.enable_interrupts();
        controller.configure_auto_hibernate(2000);
        let expected_ahit = controller.with_mmio(|mmio| AutoHibernateIdleTimerReg::read(mmio).0);
        assert_ne!(expected_ahit, 0);
        assert!(controller.is_auto_hibernate_active());

        let outer_guard = controller.hold_auto_hibernate(HOST_CONTROLLER_TIMEOUT).await.unwrap();
        assert!(!controller.is_auto_hibernate_active());
        assert_eq!(controller.with_mmio(|mmio| AutoHibernateIdleTimerReg::read(mmio).0), 0);
        let ie = controller.with_mmio(|mmio| InterruptEnableReg::read(mmio));
        assert!(!ie.uic_hibernate_enter_status_enable());
        assert!(!ie.uic_hibernate_exit_status_enable());

        let inner_guard = controller.hold_auto_hibernate(HOST_CONTROLLER_TIMEOUT).await.unwrap();

        let hardware = std::thread::spawn(move || {
            assert!(wait_until(|| uic_opcode(&regs) == UicCommandOpcode::DmeGet as u32));
            assert_eq!(AutoHibernateIdleTimerReg::read(&regs).0, 0);
            complete_uic_command(&mut regs, 0, 0x1111, |_| {});

            assert!(wait_until(|| uic_opcode(&regs) == UicCommandOpcode::DmeSet as u32));
            assert_eq!(AutoHibernateIdleTimerReg::read(&regs).0, 0);
            complete_uic_command(&mut regs, 0, 0, |_| {});
        });

        assert_eq!(controller.dme_get(PA_MAXRXHSGEAR, 0).await, Ok(0x1111));
        // Between DME commands while a hold guard is active, AHIT remains 0.
        assert_eq!(controller.with_mmio(|mmio| AutoHibernateIdleTimerReg::read(mmio).0), 0);
        assert_eq!(controller.dme_set(PA_TXGEAR, 0, 4).await, Ok(()));
        assert_eq!(controller.with_mmio(|mmio| AutoHibernateIdleTimerReg::read(mmio).0), 0);
        hardware.join().unwrap();

        drop(inner_guard);
        assert!(!controller.is_auto_hibernate_active());
        assert_eq!(controller.with_mmio(|mmio| AutoHibernateIdleTimerReg::read(mmio).0), 0);

        drop(outer_guard);
        assert!(controller.is_auto_hibernate_active());
        assert_eq!(
            controller.with_mmio(|mmio| AutoHibernateIdleTimerReg::read(mmio).0),
            expected_ahit
        );
        let ie = controller.with_mmio(|mmio| InterruptEnableReg::read(mmio));
        assert!(ie.uic_hibernate_enter_status_enable());
        assert!(ie.uic_hibernate_exit_status_enable());
    }

    #[fuchsia::test]
    fn test_fetch_and_clear_interrupts_flags_ah8_and_link_lost_errors() {
        let vmo = zx::Vmo::create(1024).unwrap();
        let controller = UfsController::new(FakeMmio::new(&vmo, 1024));
        let mut regs = uic_hardware(&vmo, 0);
        regs.store32(CapabilityReg::OFFSET, CAP_AUTO_HIBERNATE_SUPPORT);
        controller.enable_interrupts();
        controller.configure_auto_hibernate(2000);

        // Simulate an AH8 exit failure interrupt (IS.UHXS + UPMCRS = PWR_FATAL_ERROR).
        regs.store32(
            HostControllerStatusReg::OFFSET,
            hcs_power_mode_status(PowerModeStatus::PowerFatalError),
        );
        let mut is = InterruptStatusReg::from_raw(0);
        is.set_uic_hibernate_exit_status(true);
        regs.store32(InterruptStatusReg::OFFSET, is.0);

        let cleared = controller.fetch_and_clear_interrupts();
        assert!(cleared.uic_hibernate_exit_status());
        assert_eq!(InterruptStatusReg::read(&regs).0, 0);
        assert_eq!(controller.auto_hibern8_error_count(), 1);
        assert!(controller.is_recovery_needed());
        assert!(controller.take_recovery_needed());
        assert!(!controller.is_recovery_needed());

        // Simulate UIC error with PA line-reset code (0x10): non-fatal, does not request recovery.
        regs.store32(
            HostUicErrorCodePhyAdapterLayerReg::OFFSET,
            (1 << 31) | UIC_PHY_ADAPTER_LAYER_LINE_RESET,
        );
        let mut is = InterruptStatusReg::from_raw(0);
        is.set_uic_error(true);
        regs.store32(InterruptStatusReg::OFFSET, is.0);
        let cleared = controller.fetch_and_clear_interrupts();
        assert!(cleared.uic_error());
        assert_eq!(InterruptStatusReg::read(&regs).0, 0);
        assert!(!controller.is_recovery_needed());

        // Simulate UIC error with non-line-reset PA error code (0x01): requests recovery.
        regs.store32(HostUicErrorCodePhyAdapterLayerReg::OFFSET, (1 << 31) | 0x01);
        let mut is = InterruptStatusReg::from_raw(0);
        is.set_uic_error(true);
        regs.store32(InterruptStatusReg::OFFSET, is.0);
        let cleared = controller.fetch_and_clear_interrupts();
        assert!(cleared.uic_error());
        assert_eq!(InterruptStatusReg::read(&regs).0, 0);
        assert!(controller.take_recovery_needed());

        // Simulate UIC link lost (IS.ULLS).
        let mut is = InterruptStatusReg::from_raw(0);
        is.set_uic_link_lost_status(true);
        regs.store32(InterruptStatusReg::OFFSET, is.0);
        let cleared = controller.fetch_and_clear_interrupts();
        assert!(cleared.uic_link_lost_status());
        assert_eq!(InterruptStatusReg::read(&regs).0, 0);
        assert!(controller.take_recovery_needed());

        // Simulate host controller fatal error (IS.HCFES).
        let mut is = InterruptStatusReg::from_raw(0);
        is.set_host_controller_fatal_error_status(true);
        regs.store32(InterruptStatusReg::OFFSET, is.0);
        let cleared = controller.fetch_and_clear_interrupts();
        assert!(cleared.host_controller_fatal_error_status());
        assert_eq!(InterruptStatusReg::read(&regs).0, 0);
        assert!(controller.take_recovery_needed());
    }
}
