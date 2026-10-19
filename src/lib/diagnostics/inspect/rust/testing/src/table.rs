// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fuchsia_inspect as inspect;
use std::sync::atomic::{AtomicUsize, Ordering};

static CURRENT_SUFFIX: AtomicUsize = AtomicUsize::new(0);

pub fn reset_unique_names() {
    CURRENT_SUFFIX.store(0, Ordering::SeqCst);
}

pub fn unique_name(name: &str) -> String {
    let suffix = CURRENT_SUFFIX.fetch_add(1, Ordering::SeqCst);
    let result = format!("{name}:0x{suffix:x}");
    result
}

pub struct Table {
    _rows: Vec<Row>,
    _node: inspect::Node,
    _object_name: inspect::StringProperty,
    _binary_data: inspect::BytesProperty,
}

impl Table {
    pub fn new(row_count: usize, col_count: usize, node: inspect::Node) -> Self {
        let _object_name = node.create_string("object_name", "Example Table");
        let _binary_data = node.create_bytes("binary_data", vec![0x20, 0x0, 0x11, 0x12, 0x5]);
        let total = (row_count * col_count) as f64;
        let mut idx: f64 = 0.0;
        let mut rows = vec![];
        for i in 0..row_count {
            let node_name = unique_name("row");
            let mut row = Row::new(node.create_child(&node_name));
            for j in 0..col_count {
                idx += 1.0;
                row.add_cell(&format!("({i},{j})"), (i * j) as i64, 100.0 * idx / total);
            }
            rows.push(row);
        }
        Self { _node: node, _object_name, _binary_data, _rows: rows }
    }
}

struct Cell {
    _node: inspect::Node,
    _name: inspect::StringProperty,
    _value: inspect::IntProperty,
    _double_value: inspect::DoubleProperty,
}

impl Cell {
    fn new(name: &str, value: i64, double_value: f64, node: inspect::Node) -> Self {
        let _name = node.create_string("name", name);
        let _value = node.create_int("value", value);
        let _double_value = node.create_double("double_value", double_value);
        Self { _node: node, _name, _value, _double_value }
    }
}

struct Row {
    node: inspect::Node,
    cells: Vec<Cell>,
}

impl Row {
    fn new(node: inspect::Node) -> Self {
        Self { node, cells: vec![] }
    }

    fn add_cell(&mut self, name: &str, value: i64, double_value: f64) {
        let node_name = unique_name("cell");
        self.cells.push(Cell::new(name, value, double_value, self.node.create_child(&node_name)));
    }
}
