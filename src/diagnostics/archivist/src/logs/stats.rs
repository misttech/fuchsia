// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use diagnostics_data::Severity;
use fuchsia_inspect::{
    ArrayProperty, IntArrayProperty, IntProperty, Node, NumericProperty, Property, StringProperty,
    UintArrayProperty, UintProperty,
};
use fuchsia_inspect_derive::Inspect;
use fuchsia_sync::Mutex;

#[derive(Debug, Default, Inspect)]
pub struct LogStreamStats {
    sockets_opened: UintProperty,
    sockets_closed: UintProperty,
    last_timestamp: IntProperty,
    total: LogCounter,
    rolled_out: LogCounter,
    fatal: LogCounter,
    error: LogCounter,
    warn: LogCounter,
    info: LogCounter,
    debug: LogCounter,
    trace: LogCounter,
    url: StringProperty,
    invalid: LogCounter,
    inspect_node: Node,
}

impl LogStreamStats {
    pub fn set_url(&self, url: &str) {
        self.url.set(url);
    }

    pub fn open_socket(&self) {
        self.sockets_opened.add(1);
    }

    pub fn close_socket(&self) {
        self.sockets_closed.add(1);
    }

    pub fn increment_rolled_out(&self, msg_len: usize) {
        self.rolled_out.increment_bytes(msg_len);
    }

    pub fn increment_invalid(&self, bytes: usize) {
        self.invalid.increment_bytes(bytes);
    }

    pub fn ingest_message(&self, bytes: usize, severity: Severity) {
        self.total.count(bytes);
        match severity {
            Severity::Trace => self.trace.count(bytes),
            Severity::Debug => self.debug.count(bytes),
            Severity::Info => self.info.count(bytes),
            Severity::Warn => self.warn.count(bytes),
            Severity::Error => self.error.count(bytes),
            Severity::Fatal => self.fatal.count(bytes),
        }
    }
}

#[derive(Debug, Default, Inspect)]
struct LogCounter {
    number: UintProperty,
    bytes: UintProperty,

    inspect_node: Node,
}

impl LogCounter {
    fn count(&self, bytes: usize) {
        self.number.add(1);
        self.bytes.add(bytes as u64);
    }

    fn increment_bytes(&self, bytes: usize) {
        self.number.add(1);
        self.bytes.add(bytes as u64);
    }
}

pub struct GlobalAnalytics {
    _node: Node,
    logs_node: Node,
}

impl GlobalAnalytics {
    pub fn new(parent: &Node) -> Self {
        let node = parent.create_child("global_analytics");
        let logs_node = node.create_child("logs");
        Self { _node: node, logs_node }
    }

    pub fn logs_node(&self) -> &Node {
        &self.logs_node
    }
}

pub struct SaturationCurve {
    boot_times: IntArrayProperty,
    message_counts: UintArrayProperty,
    cursor: Mutex<usize>,
    size: usize,
    _node: Node,
}

impl SaturationCurve {
    pub fn new(parent: &Node, size: usize) -> Self {
        let node = parent.create_child("saturation_curve");
        let boot_times = node.create_int_array("boot_times", size);
        let message_counts = node.create_uint_array("message_counts", size);
        Self { boot_times, message_counts, cursor: Mutex::new(0), size, _node: node }
    }

    pub fn record(&self, boot_time: i64, message_count: u64) {
        let mut cursor = self.cursor.lock();
        self.boot_times.set(*cursor, boot_time);
        self.message_counts.set(*cursor, message_count);
        *cursor = (*cursor + 1) % self.size;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diagnostics_assertions::assert_data_tree;
    use fuchsia_inspect::Inspector;

    #[fuchsia::test]
    async fn saturation_curve_circular() {
        let inspector = Inspector::default();
        let curve = SaturationCurve::new(inspector.root(), 3);

        curve.record(1, 10);
        curve.record(2, 20);
        curve.record(3, 30);

        assert_data_tree!(inspector,
        root: {
            saturation_curve: {
                boot_times: vec![1i64, 2, 3],
                message_counts: vec![10u64, 20, 30],
            }
        });

        // Should wrap around
        curve.record(4, 40);

        assert_data_tree!(inspector,
        root: {
            saturation_curve: {
                boot_times: vec![4i64, 2, 3],
                message_counts: vec![40u64, 20, 30],
            }
        });
    }
}
