// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Embedded in-situ driver library (`driver_lab_rust::embedded`) for DFv2
//! Rust drivers (Spec Phase 2 Sections 2 & 3.1).
//!
//! Enables an active, bound driver to register its mapped MMIO banks (via
//! VMO duplication), protocol resources, and interrupt channels with an
//! embedded `fuchsia.driver.lab` server published on the driver's outgoing
//! [`ServiceFs`].

use crate::fuchsia_backends::{
    FuchsiaClock, FuchsiaGpio, FuchsiaI2c, FuchsiaReset, FuchsiaSerial, FuchsiaSpi, LiveBackend,
};
use crate::platform_provider::MappedMmio;
use crate::server::{self, ProxyState, SharedState, SharedStateData, ZxClock};
use crate::trace::AuditTracer;
use fidl_fuchsia_driver_lab as flab;
use fidl_fuchsia_hardware_clock as fclock;
use fidl_fuchsia_hardware_gpio as fgpio;
use fidl_fuchsia_hardware_i2c as fi2c;
use fidl_fuchsia_hardware_reset as freset;
use fidl_fuchsia_hardware_serial as fserial;
use fidl_fuchsia_hardware_spi as fspi;
use fuchsia_async as fasync;
#[cfg(feature = "fdf_driver")]
use fuchsia_component::server::{ServiceFs, ServiceObj, ServiceObjLocal};
use lab_proxy_core::access_policy::{
    ProtocolCeiling, ResourceCeiling, ResourceId, ResourceKind, WritableRegister,
};
use lab_proxy_core::audit_ring::{AuditRecord, AuditRing, Decision, OpStatus};
use lab_proxy_core::config::{AccessLimitEnforcer, ProxyConfig};
use lab_proxy_core::executor::{ExecLimits, Executor};
use lab_proxy_core::interrupt::{InterruptEvent, InterruptManager};
use lab_proxy_core::provider::{ProvidedResources, ProviderError};
use lab_proxy_core::session::{ProxyIdentity, QuiesceHook, SessionManager};
pub use lab_proxy_core::state_bank::{StateBank, StateEntry, StateSlotHandle};
use lab_proxy_core::target_policy::TargetPolicyManifest;
use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

fn now_ns() -> i64 {
    zx::MonotonicInstant::get().into_nanos()
}

/// Builder for configuring and instantiating an [`EmbeddedLabServer`] inside
/// an existing DFv2 driver.
pub struct DriverLabBuilder {
    bundle: ProvidedResources<LiveBackend>,
    next_id: ResourceId,
    allow_mutating_sessions: bool,
    config: ProxyConfig,
    quiesce_hook: Option<QuiesceHook>,
}

impl DriverLabBuilder {
    /// Creates a new builder for the given stable node/driver identity.
    pub fn new(node_identity: impl Into<String>) -> Self {
        let mut config = ProxyConfig::default();
        config.enabled = cfg!(any(feature = "driver_lab_enabled", test));
        Self {
            bundle: ProvidedResources::embedded(&node_identity.into()),
            next_id: 0,
            allow_mutating_sessions: true,
            config,
            quiesce_hook: None,
        }
    }

    /// Creates a new builder extracting the node identity from `DriverContext`.
    #[cfg(feature = "fdf_driver")]
    pub fn from_context(context: &fdf_component::DriverContext) -> Self {
        let identity = context.node_name().unwrap_or("driver-lab.embedded");
        Self::new(identity)
    }

    /// Explicitly enables or disables the embedded debug server (in release
    /// builds where `enable_driver_lab = false`, this defaults to `false` and
    /// omits outgoing service handlers).
    pub fn with_enabled(mut self, enabled: bool) -> Self {
        self.config.enabled = enabled;
        self
    }

    /// Sets whether mutating sessions are permitted on this embedded server.
    pub fn with_allow_mutating_sessions(mut self, allow: bool) -> Self {
        self.allow_mutating_sessions = allow;
        self
    }

    /// Overrides the proxy configuration bounds (narrowing against baseline).
    pub fn with_config(mut self, config: ProxyConfig) -> Self {
        if let Ok(narrowed) = ProxyConfig::default().narrow_with(&config) {
            self.config = narrowed;
        }
        self
    }

    /// Registers a synchronous quiesce callback (`quiesce_hook(bool paused)`)
    /// invoked with `true` when a mutating session acquires the mutation lease
    /// and `false` when the session closes or the driver stops (Spec Phase 2
    /// Section 2.2).
    pub fn with_quiesce_hook<F>(&mut self, hook: F) -> &mut Self
    where
        F: Fn(bool) + Send + Sync + 'static,
    {
        self.quiesce_hook = Some(Arc::new(hook));
        self
    }

    /// Ergonomic helper to register a driver MMIO VMO by byte size.
    pub fn with_mmio(
        &mut self,
        name: impl Into<String>,
        vmo: &zx::Vmo,
        offset: usize,
        size: usize,
    ) -> Result<ResourceId, zx::Status> {
        self.add_mmio_vmo(name, vmo, offset, size as u64)
    }

    /// Configures 32-bit writable register offsets on a registered MMIO resource.
    pub fn with_writable_registers(&mut self, id: ResourceId, offsets: Vec<u64>) -> &mut Self {
        if let Some(ceiling) = self.bundle.ceiling.get_mut(&id) {
            ceiling.writable_registers = offsets
                .into_iter()
                .map(|offset| lab_proxy_core::access_policy::WritableRegister {
                    offset,
                    width: 4,
                    allow_mask: 0xFFFF_FFFF,
                    allow_rmw: true,
                    require_precondition: false,
                    precondition_mask: 0,
                    readback: true,
                })
                .collect();
        }
        self
    }

    /// Configures hard-denied byte offset ranges `(start, end)` on a registered MMIO resource.
    pub fn with_hard_denied_ranges(
        &mut self,
        id: ResourceId,
        ranges: Vec<(u64, u64)>,
    ) -> &mut Self {
        if let Some(ceiling) = self.bundle.ceiling.get_mut(&id) {
            ceiling.hard_denied = ranges.into_iter().map(|(start, end)| start..end).collect();
        }
        self
    }

    /// Ergonomic alias for [`Self::add_interrupt`].
    pub fn with_interrupt(&mut self, name: impl Into<String>) -> ResourceId {
        self.add_interrupt(name)
    }

    /// Shares an existing driver MMIO VMO with the embedded server using a
    /// default read/poll-permissive ceiling.
    ///
    /// Duplicates `vmo` (`zx::Rights::SAME_RIGHTS`) and creates an independent
    /// local mapping in the driver host address space.
    pub fn add_mmio_vmo(
        &mut self,
        name: impl Into<String>,
        vmo: &zx::Vmo,
        offset: usize,
        logical_size: u64,
    ) -> Result<ResourceId, zx::Status> {
        let ceiling = ResourceCeiling {
            hard_denied: vec![],
            allow_unknown_reads: true,
            allow_poll: true,
            writable_registers: vec![],
            protocol: None,
            allow_interrupt: false,
        };
        self.add_mmio_vmo_with_ceiling(name, vmo, offset, logical_size, ceiling)
    }

    /// Shares an existing driver MMIO VMO with an explicit [`ResourceCeiling`]
    /// (such as `hard_denied` ranges for clear-on-read/FIFO registers and
    /// `writable_registers` for permitted mutations).
    pub fn add_mmio_vmo_with_ceiling(
        &mut self,
        name: impl Into<String>,
        vmo: &zx::Vmo,
        offset: usize,
        logical_size: u64,
        ceiling: ResourceCeiling,
    ) -> Result<ResourceId, zx::Status> {
        let map_len = usize::try_from(logical_size).map_err(|_| zx::Status::OUT_OF_RANGE)?;
        let mmio = MappedMmio::from_vmo(vmo, offset, map_len)?;
        Ok(self.add_mapped_mmio(name, mmio, logical_size, ceiling))
    }

    /// Registers a pre-mapped [`MappedMmio`] region with an explicit [`ResourceCeiling`].
    pub fn add_mapped_mmio(
        &mut self,
        name: impl Into<String>,
        mmio: MappedMmio,
        logical_size: u64,
        ceiling: ResourceCeiling,
    ) -> ResourceId {
        let id = self.next_id;
        self.next_id += 1;
        let mapped_size = mmio.len() as u64;
        self.bundle.add_mmio(id, name, logical_size, mapped_size, ceiling, LiveBackend::Mmio(mmio));
        id
    }

    /// Registers an in-memory [`StateBank`] exposing software state slots,
    /// runtime knobs, read probes, and diagnostic triggers with a
    /// [`ResourceCeiling`] populated from `bank.writable_offsets()`.
    pub fn with_state_bank(&mut self, name: impl Into<String>, bank: StateBank) -> ResourceId {
        self.add_state_bank(name, bank)
    }

    /// Registers an in-memory [`StateBank`] using its configured writable
    /// offsets ceiling.
    pub fn add_state_bank(&mut self, name: impl Into<String>, bank: StateBank) -> ResourceId {
        let ceiling = bank.ceiling();
        self.add_state_bank_with_ceiling(name, bank, ceiling)
    }

    /// Registers an in-memory [`StateBank`] with an explicit [`ResourceCeiling`].
    pub fn add_state_bank_with_ceiling(
        &mut self,
        name: impl Into<String>,
        bank: StateBank,
        ceiling: ResourceCeiling,
    ) -> ResourceId {
        let id = self.next_id;
        self.next_id += 1;
        let logical_size = bank.logical_size();
        self.bundle.add_mmio(
            id,
            name,
            logical_size,
            logical_size,
            ceiling,
            LiveBackend::State(bank),
        );
        id
    }

    /// Configures the permitted 32-bit writable registers on an existing MMIO
    /// resource's target ceiling.
    pub fn set_writable_registers(
        &mut self,
        resource_id: ResourceId,
        writable_registers: Vec<WritableRegister>,
    ) {
        if let Some(ceiling) = self.bundle.ceiling.get_mut(&resource_id) {
            ceiling.writable_registers = writable_registers;
        }
    }

    /// Configures the hard-denied byte offset ranges on an existing MMIO
    /// resource's target ceiling.
    pub fn set_hard_denied_ranges(&mut self, resource_id: ResourceId, ranges: Vec<Range<u64>>) {
        if let Some(ceiling) = self.bundle.ceiling.get_mut(&resource_id) {
            ceiling.hard_denied = ranges;
        }
    }

    /// Registers a GPIO protocol resource.
    pub fn add_gpio(
        &mut self,
        name: impl Into<String>,
        proxy: fgpio::GpioSynchronousProxy,
    ) -> ResourceId {
        let id = self.next_id;
        self.next_id += 1;
        self.bundle.add_protocol(
            id,
            name,
            ResourceKind::Gpio,
            ProtocolCeiling::default_for(ResourceKind::Gpio),
            LiveBackend::Gpio(FuchsiaGpio::new(proxy)),
        );
        id
    }

    /// Registers an I2C protocol resource.
    pub fn add_i2c(
        &mut self,
        name: impl Into<String>,
        proxy: fi2c::DeviceSynchronousProxy,
    ) -> ResourceId {
        let id = self.next_id;
        self.next_id += 1;
        self.bundle.add_protocol(
            id,
            name,
            ResourceKind::I2c,
            ProtocolCeiling::default_for(ResourceKind::I2c),
            LiveBackend::I2c(FuchsiaI2c::new(proxy)),
        );
        id
    }

    /// Registers a SPI protocol resource.
    pub fn add_spi(
        &mut self,
        name: impl Into<String>,
        proxy: fspi::DeviceSynchronousProxy,
    ) -> ResourceId {
        let id = self.next_id;
        self.next_id += 1;
        self.bundle.add_protocol(
            id,
            name,
            ResourceKind::Spi,
            ProtocolCeiling::default_for(ResourceKind::Spi),
            LiveBackend::Spi(FuchsiaSpi::new(proxy)),
        );
        id
    }

    /// Registers a Clock protocol resource.
    pub fn add_clock(
        &mut self,
        name: impl Into<String>,
        proxy: fclock::ClockSynchronousProxy,
    ) -> ResourceId {
        let id = self.next_id;
        self.next_id += 1;
        self.bundle.add_protocol(
            id,
            name,
            ResourceKind::Clock,
            ProtocolCeiling::default_for(ResourceKind::Clock),
            LiveBackend::Clock(FuchsiaClock::new(proxy)),
        );
        id
    }

    /// Registers a Reset protocol resource.
    pub fn add_reset(
        &mut self,
        name: impl Into<String>,
        proxy: freset::ResetSynchronousProxy,
    ) -> ResourceId {
        let id = self.next_id;
        self.next_id += 1;
        self.bundle.add_protocol(
            id,
            name,
            ResourceKind::Reset,
            ProtocolCeiling::default_for(ResourceKind::Reset),
            LiveBackend::Reset(FuchsiaReset::new(proxy)),
        );
        id
    }

    /// Registers a Serial protocol resource.
    pub fn add_serial(
        &mut self,
        name: impl Into<String>,
        proxy: fserial::DeviceSynchronousProxy,
    ) -> ResourceId {
        let id = self.next_id;
        self.next_id += 1;
        self.bundle.add_protocol(
            id,
            name,
            ResourceKind::Serial,
            ProtocolCeiling::default_for(ResourceKind::Serial),
            LiveBackend::Serial(FuchsiaSerial::new(proxy)),
        );
        id
    }

    /// Registers an interrupt observation resource for event tapping.
    pub fn add_interrupt(&mut self, name: impl Into<String>) -> ResourceId {
        let id = self.next_id;
        self.next_id += 1;
        self.bundle.add_interrupt(id, name, LiveBackend::Interrupt);
        id
    }

    /// Validates all registered resources and builds the [`EmbeddedLabServer`].
    pub fn build(self) -> Result<EmbeddedLabServer, ProviderError> {
        self.bundle.validate()?;

        let mut boot_id_bytes = [0u8; 16];
        zx::cprng_draw(&mut boot_id_bytes);
        let boot_id: String = boot_id_bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        let proxy_generation = zx::BootInstant::get().into_nanos() as u64;

        let manifest = TargetPolicyManifest::from_bundle_ceiling(
            &self.bundle.resources,
            &self.bundle.ceiling,
            self.allow_mutating_sessions,
        );
        let policy_digest = manifest.policy_digest().to_string();
        let digests = self.bundle.digests();
        let identity = ProxyIdentity {
            boot_id,
            proxy_generation,
            resource_digest: self.bundle.combined_digest().to_string(),
            policy_digest,
        };
        let resource_digests: BTreeMap<u32, String> =
            digests.iter().map(|(id, digest)| (*id, digest.to_string())).collect();

        let mut interrupts = InterruptManager::new();
        for (id, res) in &self.bundle.resources {
            if res.kind == ResourceKind::Interrupt {
                interrupts.register(*id);
            }
        }

        // Disabled servers, which include every embedded server in builds
        // without `enable_driver_lab`, emit no trace events.
        let tracer = self.config.enabled.then(|| {
            Arc::new(AuditTracer::new(&self.bundle.node_identity, &self.bundle.resources))
        });

        let limit_enforcer =
            AccessLimitEnforcer::new(self.config.max_ops_per_second, self.config.max_deadline_ns);
        let mut sessions = SessionManager::with_limits(
            identity,
            self.bundle.resources,
            self.bundle.ceiling,
            self.allow_mutating_sessions,
            limit_enforcer,
        );
        if let Some(hook) = self.quiesce_hook {
            sessions.set_quiesce_hook(hook);
        }
        let quiesce_flag = sessions.quiesce_flag();
        if !self.config.enabled {
            sessions.reject_new_sessions();
        }

        let executor = Executor::new(
            self.bundle.backends,
            ZxClock,
            ExecLimits {
                max_snapshot_items: self.config.max_snapshot_items as usize,
                max_sequence_items: self.config.max_sequence_items as usize,
                max_delay_ns: self.config.max_delay_ns as i64,
                max_sequence_duration_ns: self.config.max_sequence_duration_ns as i64,
            },
        );
        let mut audit = AuditRing::new(self.config.audit_capacity as usize);
        if let Some(tracer) = &tracer {
            audit.set_observer(tracer.observer());
        }
        audit.append(AuditRecord::lifecycle("embedded_driver_start", now_ns()));

        let state: SharedState = Arc::new(SharedStateData {
            inner: Mutex::new(ProxyState {
                sessions,
                executor,
                audit,
                resource_digests,
                interrupts,
                config: self.config,
            }),
            abort_token: AtomicBool::new(false),
        });

        Ok(EmbeddedLabServer { state, quiesce_flag, tracer })
    }
}

/// Handle to the embedded `fuchsia.driver.lab` server running inside an
/// existing DFv2 driver.
#[derive(Clone)]
pub struct EmbeddedLabServer {
    state: SharedState,
    quiesce_flag: Arc<AtomicBool>,
    tracer: Option<Arc<AuditTracer>>,
}

impl EmbeddedLabServer {
    /// Returns whether the embedded server is enabled.
    pub fn is_enabled(&self) -> bool {
        self.state.inner.lock().unwrap().config.enabled
    }

    /// Publishes `fuchsia.driver.lab.Service` (`default` instance) onto a
    /// thread-safe driver [`ServiceFs`], spawning connection handlers onto
    /// `scope_handle`.
    ///
    /// When disabled (`enable_driver_lab = false`), this is a no-op and omits
    /// outgoing service handlers (Spec Phase 2 Section 4.1).
    #[cfg(feature = "fdf_driver")]
    pub fn publish(
        &self,
        outgoing: &mut ServiceFs<ServiceObj<'static, ()>>,
        scope_handle: fasync::ScopeHandle,
    ) {
        if !self.is_enabled() {
            return;
        }
        let state = self.state.clone();
        outgoing.dir("svc").add_fidl_service_instance(
            "default",
            move |request: flab::ServiceRequest| {
                let flab::ServiceRequest::Proxy(stream) = request;
                scope_handle.spawn(server::serve_proxy(
                    state.clone(),
                    scope_handle.clone(),
                    stream,
                ));
            },
        );
    }

    /// Publishes `fuchsia.driver.lab.Service` (`default` instance) onto a
    /// local driver [`ServiceFs`], spawning connection handlers onto
    /// `scope_handle`.
    ///
    /// When disabled (`enable_driver_lab = false`), this is a no-op and omits
    /// outgoing service handlers (Spec Phase 2 Section 4.1).
    #[cfg(feature = "fdf_driver")]
    pub fn publish_local(
        &self,
        outgoing: &mut ServiceFs<ServiceObjLocal<'static, ()>>,
        scope_handle: fasync::ScopeHandle,
    ) {
        if !self.is_enabled() {
            return;
        }
        let state = self.state.clone();
        outgoing.dir("svc").add_fidl_service_instance(
            "default",
            move |request: flab::ServiceRequest| {
                let flab::ServiceRequest::Proxy(stream) = request;
                scope_handle.spawn(server::serve_proxy(
                    state.clone(),
                    scope_handle.clone(),
                    stream,
                ));
            },
        );
    }

    /// Spawns a single `fuchsia.driver.lab.Proxy` stream on `scope_handle`.
    pub fn serve_proxy_stream(
        &self,
        scope_handle: fasync::ScopeHandle,
        stream: flab::Proxy_RequestStream,
    ) {
        let state = self.state.clone();
        scope_handle.spawn(server::serve_proxy(state, scope_handle.clone(), stream));
    }

    /// Spawns a single `fuchsia.driver.lab.Proxy` channel server-end on `scope_handle`.
    pub fn serve_proxy_channel(&self, scope_handle: fasync::ScopeHandle, channel: zx::Channel) {
        let stream = fidl::endpoints::ServerEnd::<flab::Proxy_Marker>::new(channel).into_stream();
        self.serve_proxy_stream(scope_handle, stream);
    }

    /// Returns a reference to the underlying shared proxy state.
    pub fn state(&self) -> &SharedState {
        &self.state
    }

    /// Returns whether the driver is currently quiesced due to an active
    /// mutating session (lock-free atomic read).
    pub fn is_quiesced(&self) -> bool {
        self.quiesce_flag.load(Ordering::SeqCst)
    }

    /// Returns a cloneable handle to the atomic quiesce flag so driver
    /// background loops can check quiesce status without locking.
    pub fn quiesce_flag(&self) -> Arc<AtomicBool> {
        self.quiesce_flag.clone()
    }

    /// Samples the `driver_lab` trace counter for `offset` on
    /// `resource_id` with `value`.
    ///
    /// Successful host writes are sampled automatically. Call this when
    /// the driver itself changes a register or knob that a host may also
    /// write, so both sources share one counter track. No-op unless the
    /// server is enabled and a trace session has enabled `driver_lab`.
    pub fn trace_counter(&self, resource_id: ResourceId, offset: u64, value: u32) {
        if let Some(tracer) = &self.tracer {
            tracer.counter(resource_id, offset, value);
        }
    }

    /// Taps an interrupt event from the driver's own interrupt service routine
    /// (`lab.notify_interrupt(resource_id)`) without stealing or binding the
    /// raw `zx::Interrupt` handle (Spec Phase 2 Section 2.3).
    ///
    /// Increments the interrupt sequence and total count, appends to the audit
    /// ring, and satisfies pending host `WaitForInterrupt` hanging gets.
    pub fn notify_interrupt(&self, resource_id: ResourceId) -> Option<InterruptEvent> {
        self.notify_interrupt_with_timestamp(resource_id, now_ns())
    }

    /// Taps an interrupt event with an explicit timestamp.
    pub fn notify_interrupt_with_timestamp(
        &self,
        resource_id: ResourceId,
        timestamp_ns: i64,
    ) -> Option<InterruptEvent> {
        let mut guard = self.state.inner.lock().unwrap();
        let event = guard.interrupts.on_interrupt(resource_id, timestamp_ns, None).ok();
        if let Some(ref ev) = event {
            guard.audit.append(AuditRecord {
                session: None,
                resource: Some(resource_id),
                operation: "interrupt_tap",
                offset: None,
                decision: Decision::Allowed,
                status: OpStatus::Ok,
                value: Some(ev.sequence as u32),
                timestamp_ns,
                run_id: None,
                item_index: None,
            });
        }
        event
    }

    /// Stops accepting new sessions, releases any active mutation lease and
    /// quiesce hook, cancels pending interrupt waiters, and appends a stop
    /// lifecycle audit record.
    pub fn stop(&self) {
        self.state.abort_token.store(true, Ordering::SeqCst);
        let mut state = self.state.inner.lock().unwrap();
        state.sessions.reject_new_sessions();
        state.interrupts.cancel_waiters();
        state.audit.append(AuditRecord::lifecycle("embedded_driver_stop", now_ns()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lab_proxy_core::access_policy::WritableRegister;
    use mmio::Mmio as _;
    use mmio::vmo::VmoMapping;
    use zx::CachePolicy;

    #[fuchsia::test]
    async fn embedded_server_describes_and_reads_live_mmio_over_fidl() {
        const SIZE: usize = 4096;
        let vmo = zx::Vmo::create(SIZE as u64).unwrap();
        let driver_vmo = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();

        let mut driver_mmio =
            VmoMapping::map_with_cache_policy(0, SIZE, driver_vmo, CachePolicy::Cached).unwrap();
        // Simulated live registers:
        // 0x00 = DEVICE_ID (0xD1A6_0001)
        // 0x04 = STATUS (0x0000_0007)
        // 0x40 = FIFO_DATA (clear-on-read, marked hard_denied)
        driver_mmio.try_store32(0x00, 0xD1A6_0001).unwrap();
        driver_mmio.try_store32(0x04, 0x0000_0007).unwrap();
        driver_mmio.try_store32(0x40, 0xDEAD_BEEF).unwrap();

        let mut builder = DriverLabBuilder::new("sample-driver");
        let mmio_id = builder
            .add_mmio_vmo_with_ceiling(
                "regs",
                &vmo,
                0,
                0x100,
                ResourceCeiling {
                    hard_denied: vec![0x40..0x44],
                    allow_unknown_reads: true,
                    allow_poll: true,
                    writable_registers: vec![WritableRegister {
                        offset: 0x08,
                        width: 4,
                        allow_mask: 0xFFFF_FFFF,
                        allow_rmw: true,
                        require_precondition: false,
                        precondition_mask: 0,
                        readback: true,
                    }],
                    protocol: None,
                    allow_interrupt: false,
                },
            )
            .unwrap();
        assert_eq!(mmio_id, 0);

        let server = builder.build().unwrap();
        let scope = fasync::Scope::new_with_name("embedded-test");
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<flab::Proxy_Marker>();
        server.serve_proxy_stream(scope.to_handle(), stream);

        // 1. Describe the embedded server
        let desc = proxy.describe().await.unwrap();
        assert_eq!(desc.protocol_major, Some(1));
        let resources = desc.resources.as_ref().unwrap();
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].name.as_deref(), Some("regs"));
        assert_eq!(resources[0].logical_size, Some(0x100));

        // 2. Open a read-only session allowing 0x00 and 0x04
        let (session_proxy, session_server) =
            fidl::endpoints::create_proxy::<flab::SessionMarker>();
        let open_res = proxy
            .open_session(
                &flab::RunContext {
                    run_id: Some("run-cs25".to_string()),
                    case_id: Some("case-cs25".to_string()),
                    plan_digest: Some("sha256:test".to_string()),
                    host_tool_version: Some("0.1.0".to_string()),
                    ..Default::default()
                },
                flab::SessionMode::ReadOnly,
                &flab::Expectations {
                    boot_id: desc.boot_id.clone(),
                    proxy_generation: desc.proxy_generation,
                    resource_digest: desc.resource_digest.clone(),
                    policy_digest: desc.policy_digest.clone(),
                    ..Default::default()
                },
                &[
                    flab::AccessRule {
                        resource: 0,
                        offset: 0x00,
                        width: 4,
                        class: flab::AccessClass::ReadOnce,
                    },
                    flab::AccessRule {
                        resource: 0,
                        offset: 0x04,
                        width: 4,
                        class: flab::AccessClass::Snapshot,
                    },
                ],
                session_server,
            )
            .await
            .unwrap();
        assert!(open_res.is_ok());

        // 3. Read32 at 0x00 reflects live driver state
        let (read_val, _, _) = session_proxy.read32(0, 0x00).await.unwrap().unwrap();
        assert_eq!(read_val, 0xD1A6_0001);

        // 4. Snapshot at 0x04 reflects live driver state
        let (snap_results, snap_complete) = session_proxy
            .snapshot(&[flab::SnapshotItem { resource: 0, offset: 0x04 }])
            .await
            .unwrap()
            .unwrap();
        assert!(snap_complete);
        assert_eq!(snap_results.len(), 1);
        assert_eq!(snap_results[0].value, 0x0000_0007);

        // 5. Hard-denied destructive FIFO register at 0x40 is rejected when opening a session
        let (_bad_session_proxy, bad_session_server) =
            fidl::endpoints::create_proxy::<flab::SessionMarker>();
        let bad_open = proxy
            .open_session(
                &flab::RunContext::default(),
                flab::SessionMode::ReadOnly,
                &flab::Expectations {
                    boot_id: desc.boot_id.clone(),
                    proxy_generation: desc.proxy_generation,
                    resource_digest: desc.resource_digest.clone(),
                    policy_digest: desc.policy_digest.clone(),
                    ..Default::default()
                },
                &[flab::AccessRule {
                    resource: 0,
                    offset: 0x40,
                    width: 4,
                    class: flab::AccessClass::ReadOnce,
                }],
                bad_session_server,
            )
            .await
            .unwrap();
        assert_eq!(bad_open, Err(flab::OpenSessionError::RejectedAllowlist));

        // 6. Stop rejects new sessions
        server.stop();
    }

    #[fuchsia::test]
    async fn embedded_mutating_session_engages_quiesce_and_disconnect_releases_lease() {
        const SIZE: usize = 4096;
        let vmo = zx::Vmo::create(SIZE as u64).unwrap();
        let driver_vmo = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        let mut driver_mmio =
            VmoMapping::map_with_cache_policy(0, SIZE, driver_vmo, CachePolicy::Cached).unwrap();
        driver_mmio.try_store32(0x08, 0x0000_0001).unwrap();

        let hook_log = Arc::new(Mutex::new(Vec::new()));
        let hook_log_clone = hook_log.clone();

        let mut builder = DriverLabBuilder::new("sample-driver");
        builder.with_quiesce_hook(move |paused| {
            hook_log_clone.lock().unwrap().push(paused);
        });
        builder
            .add_mmio_vmo_with_ceiling(
                "regs",
                &vmo,
                0,
                0x100,
                ResourceCeiling {
                    hard_denied: vec![],
                    allow_unknown_reads: true,
                    allow_poll: true,
                    writable_registers: vec![WritableRegister {
                        offset: 0x08,
                        width: 4,
                        allow_mask: 0xFFFF_FFFF,
                        allow_rmw: true,
                        require_precondition: false,
                        precondition_mask: 0,
                        readback: true,
                    }],
                    protocol: None,
                    allow_interrupt: false,
                },
            )
            .unwrap();
        let irq_id = builder.add_interrupt("irq0");
        assert_eq!(irq_id, 1);

        let server = builder.build().unwrap();
        let scope = fasync::Scope::new_with_name("embedded-quiesce-test");
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<flab::Proxy_Marker>();
        server.serve_proxy_stream(scope.to_handle(), stream);

        let desc = proxy.describe().await.unwrap();
        assert!(!server.is_quiesced());

        // Open mutating session with write + interrupt access
        let (session_proxy, session_server) =
            fidl::endpoints::create_proxy::<flab::SessionMarker>();
        let open_res = proxy
            .open_session(
                &flab::RunContext { run_id: Some("run-cs26".to_string()), ..Default::default() },
                flab::SessionMode::Mutating,
                &flab::Expectations {
                    boot_id: desc.boot_id.clone(),
                    proxy_generation: desc.proxy_generation,
                    resource_digest: desc.resource_digest.clone(),
                    policy_digest: desc.policy_digest.clone(),
                    ..Default::default()
                },
                &[
                    flab::AccessRule {
                        resource: 0,
                        offset: 0x08,
                        width: 4,
                        class: flab::AccessClass::Write,
                    },
                    flab::AccessRule {
                        resource: 1,
                        offset: 0,
                        width: 0,
                        class: flab::AccessClass::Interrupt,
                    },
                ],
                session_server,
            )
            .await
            .unwrap();
        assert!(open_res.is_ok());

        // Driver is now quiesced
        assert!(server.is_quiesced());
        assert_eq!(*hook_log.lock().unwrap(), vec![true]);

        // Perform Write32 while quiesced
        let (readback_val, _, _) = session_proxy
            .write32(0, 0x08, 0xABCD_EF01, 0xFFFF_FFFF, None, true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(readback_val, 0xABCD_EF01);
        assert_eq!(driver_mmio.try_load32(0x08), Ok(0xABCD_EF01));

        // Tap interrupt from driver ISR and verify WaitForInterrupt wakes up
        let server_for_irq = server.clone();
        scope.spawn(async move {
            server_for_irq.notify_interrupt(1);
        });
        let (irq_res, irq_seq, _, _, _) =
            session_proxy.wait_for_interrupt(1, 0, 500_000_000).await.unwrap().unwrap();
        assert_eq!(irq_res, 1);
        assert_eq!(irq_seq, 1);

        // Simulate host disconnect by dropping session_proxy
        drop(session_proxy);

        // Open a second mutating session to prove lease was released and driver unquiesced
        for _ in 0..50 {
            if !server.is_quiesced() {
                break;
            }
            fasync::Timer::new(zx::MonotonicInstant::after(zx::Duration::from_millis(5))).await;
        }
        assert!(!server.is_quiesced());
        assert_eq!(*hook_log.lock().unwrap(), vec![true, false]);

        let audit_page = server.state().inner.lock().unwrap().audit.read(0, 100);
        assert!(audit_page.entries.iter().any(|e| e.record.operation == "quiesce_engaged"));
        assert!(audit_page.entries.iter().any(|e| e.record.operation == "quiesce_released"));
        assert!(audit_page.entries.iter().any(|e| e.record.operation == "interrupt_tap"));
    }

    #[fuchsia::test]
    fn embedded_server_mirrors_audit_into_tracing_only_when_enabled() {
        let enabled = DriverLabBuilder::new("sample-driver").build().unwrap();
        assert!(enabled.state().inner.lock().unwrap().audit.has_observer());
        // Without an active trace session, sampling is a no-op.
        enabled.trace_counter(0, 0x1c, 1);

        let disabled = DriverLabBuilder::new("sample-driver").with_enabled(false).build().unwrap();
        assert!(!disabled.state().inner.lock().unwrap().audit.has_observer());
        disabled.trace_counter(0, 0x1c, 1);
    }

    #[fuchsia::test]
    async fn embedded_state_bank_supports_slots_knobs_and_triggers_over_fidl() {
        let mut bank = StateBank::new(0x20);
        let violations_slot = bank.define_state_slot(0x00, 0);
        let delay_knob = bank.define_knob(0x04, 0);
        let fix_knob = bank.define_knob(0x08, 0);

        let violations_for_trigger = violations_slot.clone();
        let delay_for_trigger = delay_knob.clone();
        let fix_for_trigger = fix_knob.clone();
        bank.define_trigger(0x0C, move |_arg| {
            if delay_for_trigger.load() > 0 && fix_for_trigger.load() == 0 {
                violations_for_trigger.fetch_add(1);
                Ok(1)
            } else {
                Ok(0)
            }
        });

        let mut builder = DriverLabBuilder::new("sample-driver");
        let state_id = builder.with_state_bank("state0", bank);
        assert_eq!(state_id, 0);

        let server = builder.build().unwrap();
        let scope = fasync::Scope::new_with_name("embedded-state-bank-test");
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<flab::Proxy_Marker>();
        server.serve_proxy_stream(scope.to_handle(), stream);

        let desc = proxy.describe().await.unwrap();
        let resources = desc.resources.as_ref().unwrap();
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].name.as_deref(), Some("state0"));
        assert_eq!(resources[0].logical_size, Some(0x20));

        // Attempting to open a session with a Write rule on read-only state slot 0x00 fails.
        let (_bad_session, bad_server) = fidl::endpoints::create_proxy::<flab::SessionMarker>();
        let bad_open = proxy
            .open_session(
                &flab::RunContext::default(),
                flab::SessionMode::Mutating,
                &flab::Expectations {
                    boot_id: desc.boot_id.clone(),
                    proxy_generation: desc.proxy_generation,
                    resource_digest: desc.resource_digest.clone(),
                    policy_digest: desc.policy_digest.clone(),
                    ..Default::default()
                },
                &[flab::AccessRule {
                    resource: 0,
                    offset: 0x00,
                    width: 4,
                    class: flab::AccessClass::Write,
                }],
                bad_server,
            )
            .await
            .unwrap();
        assert_eq!(bad_open, Err(flab::OpenSessionError::RejectedAllowlist));

        // Open mutating session with read access to 0x00 and write access to 0x04, 0x08, 0x0C.
        let (session_proxy, session_server) =
            fidl::endpoints::create_proxy::<flab::SessionMarker>();
        let open_res = proxy
            .open_session(
                &flab::RunContext { run_id: Some("run-state".to_string()), ..Default::default() },
                flab::SessionMode::Mutating,
                &flab::Expectations {
                    boot_id: desc.boot_id.clone(),
                    proxy_generation: desc.proxy_generation,
                    resource_digest: desc.resource_digest.clone(),
                    policy_digest: desc.policy_digest.clone(),
                    ..Default::default()
                },
                &[
                    flab::AccessRule {
                        resource: 0,
                        offset: 0x00,
                        width: 4,
                        class: flab::AccessClass::ReadOnce,
                    },
                    flab::AccessRule {
                        resource: 0,
                        offset: 0x04,
                        width: 4,
                        class: flab::AccessClass::Write,
                    },
                    flab::AccessRule {
                        resource: 0,
                        offset: 0x08,
                        width: 4,
                        class: flab::AccessClass::Write,
                    },
                    flab::AccessRule {
                        resource: 0,
                        offset: 0x0C,
                        width: 4,
                        class: flab::AccessClass::Write,
                    },
                ],
                session_server,
            )
            .await
            .unwrap();
        assert!(open_res.is_ok());

        // Set delay knob = 50 us, fix knob = 0, then trigger -> violation observed.
        let (delay_rb, _, _) =
            session_proxy.write32(0, 0x04, 50, 0xFFFF_FFFF, None, true).await.unwrap().unwrap();
        assert_eq!(delay_rb, 50);
        let (trig_rb, _, _) =
            session_proxy.write32(0, 0x0C, 1, 0xFFFF_FFFF, None, true).await.unwrap().unwrap();
        assert_eq!(trig_rb, 1);
        let (violations, _, _) = session_proxy.read32(0, 0x00).await.unwrap().unwrap();
        assert_eq!(violations, 1);

        // Set fix knob = 1, then trigger -> no violation (readback == 0, violations stays 1).
        let (fix_rb, _, _) =
            session_proxy.write32(0, 0x08, 1, 0xFFFF_FFFF, None, true).await.unwrap().unwrap();
        assert_eq!(fix_rb, 1);
        let (trig_fixed_rb, _, _) =
            session_proxy.write32(0, 0x0C, 1, 0xFFFF_FFFF, None, true).await.unwrap().unwrap();
        assert_eq!(trig_fixed_rb, 0);
        let (violations_after, _, _) = session_proxy.read32(0, 0x00).await.unwrap().unwrap();
        assert_eq!(violations_after, 1);
    }
}
