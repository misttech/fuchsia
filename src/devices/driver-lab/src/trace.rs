// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! System trace emission for driver-lab activity.
//!
//! [`AuditTracer`] mirrors audit records into Fuchsia tracing so host
//! probes, register and knob writes, and interrupt taps share a timeline
//! with scheduler, power, and driver events. An experiment can then relate
//! a register or knob change to its effect on, for example, CPU wakeups or
//! power rail counters within a single trace.
//!
//! Each audit record becomes an instant event named after its operation
//! (for example `read32` or `write32`). Its arguments use the field names of
//! `target-audit.jsonl` evidence rows (`seq`, `session`, `resource`,
//! `offset`, `decision`, `denial`, `status`, `value`, `timestamp_ns`,
//! `run_id`, `item_index`), plus `node` and `resource_name`, so trace events
//! can be joined with evidence bundles. `timestamp_ns` is the audit record's
//! monotonic timestamp; the event's own timestamp is when it was written.
//!
//! Each successful write also samples a counter named
//! `<node>/<resource_name>` with one series per register offset (for
//! example `0x1c`), so A/B arms appear as step functions.
//!
//! Events are written only while a trace session enables their category:
//!
//! - [`CATEGORY`] (`driver_lab`): all activity except interrupts.
//! - [`IRQ_CATEGORY`] (`driver_lab:irq`): interrupt taps, kept separate
//!   because high interrupt rates can crowd other events out of the trace
//!   buffer.
//!
//! When neither category is enabled, the cost is one cached category check
//! per record.

use fuchsia_trace::{ArgValue, Scope, TraceCategoryContext, trace_site_t};
use lab_proxy_core::access_policy::{Denial, MmioResource, ResourceId};
use lab_proxy_core::audit_ring::{AuditObserver, AuditRecord, Decision, OpStatus};
use std::collections::BTreeMap;
use std::ffi::CStr;
use std::sync::Arc;

/// Trace category for driver-lab operations other than interrupts.
pub const CATEGORY: &CStr = c"driver_lab";

/// Trace category for interrupt taps.
pub const IRQ_CATEGORY: &CStr = c"driver_lab:irq";

/// Audit operation name for an observed interrupt.
const INTERRUPT_TAP: &str = "interrupt_tap";

/// Audit operation name for a register or knob write.
const WRITE32: &str = "write32";

static CATEGORY_SITE: trace_site_t = trace_site_t::new(0);
static IRQ_CATEGORY_SITE: trace_site_t = trace_site_t::new(0);

/// Writes one driver-lab instance's audit records as trace events.
#[derive(Debug)]
pub struct AuditTracer {
    node: String,
    resource_names: BTreeMap<ResourceId, String>,
    counter_names: BTreeMap<ResourceId, String>,
}

impl AuditTracer {
    /// Creates a tracer for the instance serving `node` that labels events
    /// with the names of `resources`.
    pub fn new(node: &str, resources: &BTreeMap<ResourceId, MmioResource>) -> Self {
        let resource_names: BTreeMap<ResourceId, String> =
            resources.iter().map(|(id, resource)| (*id, resource.name.clone())).collect();
        let counter_names =
            resource_names.iter().map(|(id, name)| (*id, counter_name(node, name))).collect();
        Self { node: node.to_string(), resource_names, counter_names }
    }

    /// Returns an observer for
    /// [`AuditRing::set_observer`](lab_proxy_core::audit_ring::AuditRing::set_observer)
    /// that passes each appended record to [`Self::record`].
    pub fn observer(self: &Arc<Self>) -> AuditObserver {
        let tracer = Arc::clone(self);
        Box::new(move |seq, record| tracer.record(Some(seq), record))
    }

    /// Writes `record` as an instant event, followed by a counter sample if
    /// it is a successful write. `seq` is the audit sequence number, or
    /// `None` for events that were not appended to an audit ring.
    pub fn record(&self, seq: Option<u64>, record: &AuditRecord) {
        let context = if record.operation == INTERRUPT_TAP {
            TraceCategoryContext::acquire_cached(IRQ_CATEGORY, &IRQ_CATEGORY_SITE)
        } else {
            TraceCategoryContext::acquire_cached(CATEGORY, &CATEGORY_SITE)
        };
        let Some(context) = context else {
            return;
        };

        let resource_name = record.resource.and_then(|id| self.resource_names.get(&id));
        let denial = match record.decision {
            Decision::Allowed => None,
            Decision::Denied(denial) => Some(denial_label(denial)),
        };

        let mut args = Vec::with_capacity(13);
        args.push(ArgValue::of("node", self.node.as_str()));
        if let Some(seq) = seq {
            args.push(ArgValue::of("seq", seq));
        }
        if let Some(session) = record.session {
            args.push(ArgValue::of("session", session));
        }
        if let Some(resource) = record.resource {
            args.push(ArgValue::of("resource", resource));
        }
        if let Some(name) = resource_name {
            args.push(ArgValue::of("resource_name", name.as_str()));
        }
        if let Some(offset) = record.offset {
            args.push(ArgValue::of("offset", offset));
        }
        args.push(ArgValue::of("decision", if denial.is_some() { "denied" } else { "allowed" }));
        if let Some(denial) = denial.as_deref() {
            args.push(ArgValue::of("denial", denial));
        }
        args.push(ArgValue::of("status", status_label(record.status)));
        if let Some(value) = record.value {
            args.push(ArgValue::of("value", value));
        }
        args.push(ArgValue::of("timestamp_ns", record.timestamp_ns));
        if let Some(run_id) = record.run_id.as_deref() {
            args.push(ArgValue::of("run_id", run_id));
        }
        if let Some(item_index) = record.item_index {
            args.push(ArgValue::of("item_index", item_index));
        }
        fuchsia_trace::instant(&context, record.operation, Scope::Thread, &args);

        if record.operation == WRITE32
            && record.decision == Decision::Allowed
            && record.status == OpStatus::Ok
        {
            if let (Some(resource), Some(offset), Some(value)) =
                (record.resource, record.offset, record.value)
            {
                self.write_counter(&context, resource, offset, value);
            }
        }
    }

    /// Writes an `interrupt_tap` event for an interrupt observed outside
    /// the audit ring, such as by the standalone proxy's interrupt threads.
    /// `sequence` is the per-resource interrupt sequence number.
    pub fn interrupt(&self, resource: ResourceId, sequence: u64, timestamp_ns: i64) {
        self.record(
            None,
            &AuditRecord {
                session: None,
                resource: Some(resource),
                operation: INTERRUPT_TAP,
                offset: None,
                decision: Decision::Allowed,
                status: OpStatus::Ok,
                // Matches the truncation in the embedded interrupt tap
                // audit record.
                value: Some(sequence as u32),
                timestamp_ns,
                run_id: None,
                item_index: None,
            },
        );
    }

    /// Samples the counter series for `offset` on `resource`.
    ///
    /// Successful host writes are sampled automatically. Drivers call this
    /// when they change a register or knob themselves, so driver and host
    /// changes share one counter track.
    pub fn counter(&self, resource: ResourceId, offset: u64, value: u32) {
        if let Some(context) = TraceCategoryContext::acquire_cached(CATEGORY, &CATEGORY_SITE) {
            self.write_counter(&context, resource, offset, value);
        }
    }

    fn write_counter(
        &self,
        context: &TraceCategoryContext,
        resource: ResourceId,
        offset: u64,
        value: u32,
    ) {
        let unnamed;
        let name = match self.counter_names.get(&resource) {
            Some(name) => name,
            None => {
                unnamed = counter_name(&self.node, &resource.to_string());
                &unnamed
            }
        };
        let series = offset_series(offset);
        fuchsia_trace::counter(context, name, 0, &[ArgValue::of(series.as_str(), value)]);
    }
}

fn counter_name(node: &str, resource_name: &str) -> String {
    format!("{node}/{resource_name}")
}

fn offset_series(offset: u64) -> String {
    format!("{offset:#x}")
}

/// Converts a [`Denial`] to the snake_case label used in evidence bundles,
/// for example `NotInAllowlist` to `not_in_allowlist`.
fn denial_label(denial: Denial) -> String {
    let name = format!("{denial:?}");
    let mut label = String::with_capacity(name.len() + 4);
    for (index, ch) in name.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if index > 0 {
                label.push('_');
            }
            label.push(ch.to_ascii_lowercase());
        } else {
            label.push(ch);
        }
    }
    label
}

/// Returns the evidence bundle label for `status`.
fn status_label(status: OpStatus) -> &'static str {
    match status {
        OpStatus::Ok => "ok",
        OpStatus::Rejected => "rejected",
        OpStatus::BackendFault => "backend_fault",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denial_labels_match_evidence_format() {
        assert_eq!(denial_label(Denial::NotInAllowlist), "not_in_allowlist");
        assert_eq!(denial_label(Denial::HardDenied), "hard_denied");
        assert_eq!(denial_label(Denial::UnknownReadsNotPermitted), "unknown_reads_not_permitted");
        assert_eq!(denial_label(Denial::Timeout), "timeout");
    }

    #[test]
    fn status_labels_match_evidence_format() {
        assert_eq!(status_label(OpStatus::Ok), "ok");
        assert_eq!(status_label(OpStatus::Rejected), "rejected");
        assert_eq!(status_label(OpStatus::BackendFault), "backend_fault");
    }

    #[test]
    fn counter_tracks_are_named_by_node_resource_and_offset() {
        let mut resources = BTreeMap::new();
        resources.insert(0, MmioResource::mmio("regs", 0x100, 0x1000));
        resources.insert(3, MmioResource::interrupt("irq0"));
        let tracer = AuditTracer::new("sample-driver", &resources);
        assert_eq!(tracer.counter_names.get(&0).map(String::as_str), Some("sample-driver/regs"));
        assert_eq!(tracer.counter_names.get(&3).map(String::as_str), Some("sample-driver/irq0"));
        assert_eq!(offset_series(0x1c), "0x1c");
        assert_eq!(offset_series(0), "0x0");
    }

    #[test]
    fn emitting_without_an_active_trace_session_is_a_no_op() {
        let mut resources = BTreeMap::new();
        resources.insert(0, MmioResource::mmio("regs", 0x100, 0x1000));
        let tracer = Arc::new(AuditTracer::new("sample-driver", &resources));
        let observer = tracer.observer();
        observer(
            7,
            &AuditRecord {
                session: Some(1),
                resource: Some(0),
                operation: WRITE32,
                offset: Some(0x1c),
                decision: Decision::Allowed,
                status: OpStatus::Ok,
                value: Some(1),
                timestamp_ns: 1,
                run_id: Some("run".to_string()),
                item_index: Some(2),
            },
        );
        observer(
            8,
            &AuditRecord {
                decision: Decision::Denied(Denial::HardDenied),
                status: OpStatus::Rejected,
                ..AuditRecord::lifecycle("read32", 2)
            },
        );
        tracer.interrupt(0, 1, 3);
        tracer.counter(0, 0x1c, 0);
        tracer.counter(99, 0x1c, 0);
    }
}
