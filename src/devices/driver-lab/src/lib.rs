// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use driver_lab_rust::platform_provider;
use driver_lab_rust::server::{self, ProxyState, SharedState, ZxClock};
use driver_lab_rust::trace::AuditTracer;
use fdf_component::{Driver, DriverContext, DriverError, Node, driver_register};
use fidl_fuchsia_driver_lab as flab;
use fuchsia_async as fasync;
use fuchsia_component::server::ServiceFs;
use futures::StreamExt;
use lab_proxy_core::audit_ring::{AuditRecord, AuditRing};
use lab_proxy_core::executor::{ExecLimits, Executor};
use lab_proxy_core::session::{ProxyIdentity, SessionManager};
use log::{info, warn};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

struct LabProxy {
    _node: Node,
    _scope: fasync::Scope,
    state: SharedState,
    irq_cancellers: Mutex<Vec<zx::Interrupt>>,
}

driver_register!(LabProxy);

fn now_ns() -> i64 {
    zx::MonotonicInstant::get().into_nanos()
}

impl Driver for LabProxy {
    const NAME: &str = "lab_proxy";

    async fn start(mut context: DriverContext) -> Result<Self, DriverError> {
        let node = context.take_node()?;
        let node_identity = context.node_name().unwrap_or("driver-lab.unnamed").to_string();

        // Acquire whatever the bound node offers. A node without a
        // platform device yields zero resources; a node whose MMIOs
        // cannot all be mapped fails start rather than serving a
        // partially acquired identity.
        let (bundle, interrupts) =
            platform_provider::acquire(&context, &node_identity).await.map_err(|error| {
                warn!("resource acquisition failed: {error}");
                DriverError::Status(zx::Status::INTERNAL)
            })?;
        if let Err(error) = bundle.validate() {
            warn!("provider bundle is inconsistent: {error:?}");
            return Err(DriverError::Status(zx::Status::INTERNAL));
        }

        // Kernel randomness is the per-boot identity source: there is no
        // kernel boot UUID, and this is the same mechanism RCS uses for
        // ffx's reboot detection. A driver-host restart also regenerates
        // it, which is conservatively correct for staleness -- the
        // boot-timeline generation below distinguishes restart from
        // reboot (it keeps increasing across restarts within one boot).
        let mut boot_id_bytes = [0u8; 16];
        zx::cprng_draw(&mut boot_id_bytes);
        let boot_id: String = boot_id_bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        let proxy_generation = zx::BootInstant::get().into_nanos() as u64;

        let manifest = lab_proxy_core::target_policy::TargetPolicyManifest::engineering_default(
            &bundle.resources,
        );
        let policy_digest = manifest.policy_digest().to_string();
        let digests = bundle.digests();
        let identity = ProxyIdentity {
            boot_id,
            proxy_generation,
            resource_digest: bundle.combined_digest().to_string(),
            policy_digest,
        };
        let resource_digests: BTreeMap<u32, String> =
            digests.iter().map(|(id, digest)| (*id, digest.to_string())).collect();
        let mut interrupts_mgr = lab_proxy_core::interrupt::InterruptManager::new();
        for (id, res) in &bundle.resources {
            if res.kind == lab_proxy_core::access_policy::ResourceKind::Interrupt {
                interrupts_mgr.register(*id);
            }
        }

        let config = match context.take_config::<lab_proxy_config::Config>() {
            Ok(raw) => {
                let parsed = lab_proxy_core::config::ProxyConfig {
                    enabled: raw.enabled,
                    audit_capacity: raw.audit_capacity,
                    max_snapshot_items: raw.max_snapshot_items,
                    max_sequence_items: raw.max_sequence_items,
                    max_delay_ns: raw.max_delay_ns,
                    max_sequence_duration_ns: raw.max_sequence_duration_ns,
                    max_deadline_ns: raw.max_deadline_ns,
                    max_ops_per_second: raw.max_ops_per_second,
                };
                match lab_proxy_core::config::ProxyConfig::default().narrow_with(&parsed) {
                    Ok(narrowed) => narrowed,
                    Err(err) => {
                        warn!(
                            "structured config widened baseline: {err}; falling back to defaults"
                        );
                        lab_proxy_core::config::ProxyConfig::default()
                    }
                }
            }
            Err(_) => lab_proxy_core::config::ProxyConfig::default(),
        };

        let limit_enforcer = lab_proxy_core::config::AccessLimitEnforcer::new(
            config.max_ops_per_second,
            config.max_deadline_ns,
        );
        let mut ceiling_map = manifest.to_ceiling_map();
        for (id, res) in &bundle.resources {
            if let Some(entry) = ceiling_map.get_mut(id) {
                if res.kind != lab_proxy_core::access_policy::ResourceKind::Mmio {
                    entry.protocol =
                        Some(lab_proxy_core::access_policy::ProtocolCeiling::default_for(res.kind));
                }
                entry.allow_interrupt =
                    res.kind == lab_proxy_core::access_policy::ResourceKind::Interrupt;
            }
        }
        let tracer = Arc::new(AuditTracer::new(&node_identity, &bundle.resources));
        let mut sessions = SessionManager::with_limits(
            identity,
            bundle.resources,
            ceiling_map,
            manifest.allow_mutating_sessions,
            limit_enforcer,
        );
        if !config.enabled {
            sessions.reject_new_sessions();
        }

        let executor = Executor::new(
            bundle.backends,
            ZxClock,
            ExecLimits {
                max_snapshot_items: config.max_snapshot_items as usize,
                max_sequence_items: config.max_sequence_items as usize,
                max_delay_ns: config.max_delay_ns as i64,
                max_sequence_duration_ns: config.max_sequence_duration_ns as i64,
            },
        );
        let mut audit = AuditRing::new(config.audit_capacity as usize);
        audit.set_observer(tracer.observer());
        audit.append(AuditRecord::lifecycle("driver_start", now_ns()));
        let state: SharedState = Arc::new(server::SharedStateData {
            inner: Mutex::new(ProxyState {
                sessions,
                executor,
                audit,
                resource_digests,
                interrupts: interrupts_mgr,
                config,
            }),
            abort_token: std::sync::atomic::AtomicBool::new(false),
        });

        let scope = fasync::Scope::new_with_name(Self::NAME);

        let mut irq_cancellers = Vec::new();
        for (irq_id, irq_handle) in interrupts {
            let state = state.clone();
            let tracer = tracer.clone();
            if let Ok(dup) = irq_handle.duplicate_handle(zx::Rights::SAME_RIGHTS) {
                irq_cancellers.push(dup);
            }
            std::thread::Builder::new()
                .name(format!("driver-lab-irq-{irq_id}"))
                .spawn(move || {
                    loop {
                        match irq_handle.wait() {
                            Ok(timestamp) => {
                                let ts_nanos = timestamp.into_nanos();
                                let event = {
                                    let mut guard = state.inner.lock().unwrap();
                                    guard.interrupts.on_interrupt(irq_id, ts_nanos, None)
                                };
                                let _ = irq_handle.ack();
                                if let Ok(event) = event {
                                    tracer.interrupt(irq_id, event.sequence, ts_nanos);
                                }
                            }
                            Err(zx::Status::CANCELED) => {
                                log::info!("IRQ {irq_id} listener canceled");
                                break;
                            }
                            Err(e) => {
                                log::warn!("IRQ {irq_id} wait failed: {e:?}");
                                break;
                            }
                        }
                    }
                })
                .expect("failed to spawn IRQ listener thread");
        }

        let mut outgoing = ServiceFs::new();
        outgoing.dir("svc").add_fidl_service_instance(
            "default",
            |request: flab::ServiceRequest| {
                let flab::ServiceRequest::Proxy(stream) = request;
                stream
            },
        );
        context.serve_outgoing(&mut outgoing)?;

        {
            let state = state.clone();
            let handle = scope.to_handle();
            scope.spawn(async move {
                let mut outgoing = outgoing;
                while let Some(stream) = outgoing.next().await {
                    handle.spawn(server::serve_proxy(state.clone(), handle.clone(), stream));
                }
            });
        }

        info!("LabProxy started; serving fuchsia.driver.lab");
        Ok(Self { _node: node, _scope: scope, state, irq_cancellers: Mutex::new(irq_cancellers) })
    }

    async fn stop(&self) {
        if let Ok(mut cancellers) = self.irq_cancellers.lock() {
            for irq in cancellers.drain(..) {
                let _ = irq.destroy();
            }
        }
        self.state.abort_token.store(true, std::sync::atomic::Ordering::SeqCst);
        let mut state = self.state.inner.lock().unwrap();
        state.sessions.reject_new_sessions();
        state.interrupts.cancel_waiters();
        let seq = state.audit.append(AuditRecord::lifecycle("driver_stop", now_ns()));
        info!("LabProxy::stop() audit seq {seq}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fdf_component::testing::harness::TestHarness;

    #[fuchsia::test]
    async fn test_driver_start() {
        let mut harness = TestHarness::<LabProxy>::new();
        let started_driver = harness.start_driver().await.unwrap();

        // Verify driver started successfully
        assert!(true);

        started_driver.stop_driver().await;
    }

    #[fuchsia::test]
    async fn test_driver_stop_sets_abort_token_and_rejects_sessions() {
        let mut harness = TestHarness::<LabProxy>::new();
        let started_driver = harness.start_driver().await.unwrap();
        let state = started_driver.get_driver().expect("driver").state.clone();

        assert!(!state.abort_token.load(std::sync::atomic::Ordering::SeqCst));
        {
            let inner = state.inner.lock().unwrap();
            assert!(inner.sessions.is_accepting());
        }

        started_driver.stop_driver().await;

        assert!(state.abort_token.load(std::sync::atomic::Ordering::SeqCst));
        {
            let inner = state.inner.lock().unwrap();
            assert!(!inner.sessions.is_accepting());
            let page = inner.audit.read(0, 100);
            assert!(page.entries.iter().any(|e| e.record.operation == "driver_stop"));
        }
    }
}
