// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Provides convenience macros for writing to an inspect bounded list (bounded log)
//!
//! ## Example
//!
//! ```rust
//! let inspector = Inspector::default();
//! let list_node = inspector.root().create_child("list_node");
//! let list_node = BoundedListNode::new(list_node, 10);
//! inspect_log!(list_node, k1: "1".to_string(), meaning_of_life: 42u64, k3: 3i64, k4: 4f64);
//! // Inspect now has:
//! // root: {
//! //   list_node: {
//! //     "0": { "@time": <timestamp>, k1: "1", meaning_of_life: 42, k3: 3, k4: 4 }
//! //   }
//! // }
//! ```

use fuchsia_inspect::Node;
use std::borrow::Cow;

mod impls;
mod wrappers;

pub use wrappers::{InspectBytes, InspectList, InspectListClosure, InspectUintArray};

/// Trait for writing to a node in bounded lists.
pub trait WriteInspect {
    /// Write a *single* value (property or child node) to |node| with the specified |key|.
    /// If multiple properties need to be written, consider creating a single child
    /// node with those properties.
    fn write_inspect<'a>(&self, writer: &Node, key: impl Into<Cow<'a, str>>);
}

/// Macro to log a new entry to a bounded list node with the specified key-value pairs. Each value
/// must be a type that implements `WriteInspect`. This macro automatically injects a timestamp
/// to each entry.
///
/// Example:
///
/// ```
/// let bounded_list_node = ...;
/// inspect_log!(bounded_list_node, {});   // log only a timestamped entry
/// inspect_log!(bounded_list_node, k1: "1", k2: 2i64, k3: "3");   // non-block form
/// inspect_log!(bounded_list_node, {   // block form (only difference is syntactic)
///     ba: "dum",
///     tss: "tss",
/// });
/// inspect_log!(bounded_list_node, {   // logging nested data structure
///     k1: {
///        subkey1: "subval1",
///        subkey2: 2,
///     }
/// });
/// ```
#[macro_export]
macro_rules! inspect_log {
    ($bounded_list_node:expr, $($args:tt)+) => {{
        use $crate::{inspect_insert, nodes::{NodeTimeExt, BootTimeline}};
        $bounded_list_node.add_entry(|node| {
            NodeTimeExt::<BootTimeline>::record_time(node, "@time");
            inspect_insert!(@internal_inspect_log node, $($args)+);
        });
    }};
}

/// Macro to insert items using a Node. Each value must be a type that implements
/// `WriteInspect`.
///
/// Example:
///
/// ```
/// let node = ...; // fuchsia_inspect::Node
/// inspect_insert!(node, k1: "1", k2: 2i64, k3: "3");
/// ```
#[macro_export]
macro_rules! inspect_insert {
    (@internal $node_writer:expr,) => {};

    // Insert tree
    (@internal $node_writer:expr, var $key:ident: { $($sub:tt)+ }) => {{
        let child_writer = $node_writer.create_child($key);
        inspect_insert!(@internal child_writer, $($sub)+);
        $node_writer.record(child_writer);
    }};

    (@internal $node_writer:expr, var $key:ident: { $($sub:tt)+ }, $($rest:tt)*) => {{
        inspect_insert!(@internal $node_writer, var $key: { $($sub)+ });
        inspect_insert!(@internal $node_writer, $($rest)*);
    }};

    // Insert properties and metrics
    (@internal $node_writer:expr, var $key:ident: $val:expr) => {{
        $val.write_inspect(&$node_writer, $key);
    }};

    (@internal $node_writer:expr, var $key:ident: $val:expr, $($rest:tt)*) => {{
        inspect_insert!(@internal $node_writer, var $key: $val);
        inspect_insert!(@internal $node_writer, $($rest)*);
    }};

    // Insert optional value
    (@internal $node_writer:expr, var $key:ident?: $val:expr) => {{
        match $val {
            Some(val) => inspect_insert!(@internal $node_writer, var $key: val),
            None => (),
        }
    }};

    (@internal $node_writer:expr, var $key:ident?: $val:expr, $($rest:tt)*) => {{
        inspect_insert!(@internal $node_writer, var $key?: $val);
        inspect_insert!(@internal $node_writer, $($rest)*);
    }};

    // Key identifier format
    (@internal $node_writer:expr, $key:ident: $($rest:tt)+) => {{
        let key = stringify!($key);
        inspect_insert!(@internal $node_writer, var key: $($rest)+);
    }};

    (@internal $node_writer:expr, $key:ident?: $($rest:tt)+) => {{
        let key = stringify!($key);
        inspect_insert!(@internal $node_writer, var key?: $($rest)+);
    }};

    (@internal $node_writer:expr, $key:expr => $($rest:tt)+) => {{
        let key: std::borrow::Cow<'_, _> = $key.into();
        inspect_insert!(@internal $node_writer, var key: $($rest)+);
    }};

    // Entry point: from inspect_log! (mainly to allow empty event)
    (@internal_inspect_log $node_writer:expr, { $($args:tt)* }) => {{
        // User may specify an empty event, so `WriteInspect` may not always
        // be used.
        #[allow(unused_imports)]
        use $crate::log::WriteInspect;
        inspect_insert!(@internal $node_writer, $($args)*);
    }};

    (@internal_inspect_log $node_writer:expr, $($args:tt)+) => {{
        use $crate::log::WriteInspect;
        inspect_insert!(@internal $node_writer, $($args)+);
    }};

    // Entry point: block syntax
    ($node_writer:expr, { $($args:tt)+ }) => {{
        use $crate::log::WriteInspect;
        inspect_insert!(@internal $node_writer, $($args)+);
    }};

    // Entry point: non-block syntax
    ($node_writer:expr, $($args:tt)+) => {{
        use $crate::log::WriteInspect;
        inspect_insert!(@internal $node_writer, $($args)+);
    }};
}

/// Convenience macro to construct a closure that implements WriteInspect, so it can be
/// used in `inspect_log!` and `inspect_insert!`.
///
/// Note that this macro constructs a *move* closure underneath, unlike `inspect_log!` and
/// `inspect_insert!` where variables are only borrowed.
///
/// Example 1:
///
/// ```
/// let bounded_list_node = ...;
/// let obj = make_inspect_loggable!(k1: "1", k2: 2i64, k3: "3");
/// inspect_log!(bounded_list_node, some_key: obj);
/// ```
///
/// Example 2
///
/// ```
/// let bounded_list_node = ...;
/// let point = Some((10, 50));
/// inspect_log!(bounded_list_node, point?: point.map(|(x, y)| make_inspect_loggable!({
///     x: x,
///     y: y,
/// })))
/// ```
#[macro_export]
macro_rules! make_inspect_loggable {
    ($($args:tt)+) => {{
        use $crate::inspect_insert;
        use fuchsia_inspect::Node;
        use std::borrow::Cow;
        struct WriteInspectClosure<F>(F);
        impl<F> WriteInspect for WriteInspectClosure<F>
            where
                F: Fn(&Node, String) {
            fn write_inspect<'b>(&self, writer: &Node, key: impl Into<Cow<'b, str>>) {
                self.0(writer, key.into().to_string());
            }
        }
        let f = WriteInspectClosure(move |writer: &Node, key| {
            let child = writer.create_child(key);
            inspect_insert!(child, $($args)+);
            writer.record(child);
        });
        f
    }};
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nodes::BoundedListNode;
    use diagnostics_assertions::{assert_data_tree, AnyProperty};
    use fuchsia_inspect::{DiagnosticsHierarchyGetter, Inspector};
    use fuchsia_sync::Mutex;
    use test_util::assert_lt;

    #[fuchsia::test]
    fn test_inspect_log_basic() {
        let (inspector, mut node) = inspector_and_list_node();

        // Logging string and full-size numeric type
        inspect_log!(node, k1: "1".to_string(), meaning_of_life: 42u64, k3: 3i64, k4: 4f64);

        // Logging smaller numeric types (which should be converted to bigger types)
        inspect_log!(node, small_uint: 1u8, small_int: 2i8, float: 3f32);

        // Logging reference types + using bracket format
        inspect_log!(node, {
            s: "str",
            uint: &13u8,
        });

        // Logging empty event
        inspect_log!(node, {});

        let hierarchy = inspector.get_diagnostics_hierarchy();
        assert_data_tree!(hierarchy, root: {
            list_node: {
                "0": { "@time": AnyProperty, k1: "1", meaning_of_life: 42u64, k3: 3i64, k4: 4f64 },
                "1": { "@time": AnyProperty, small_uint: 1u64, small_int: 2i64, float: 3f64 },
                "2": { "@time": AnyProperty, s: "str", uint: 13u64 },
                "3": { "@time": AnyProperty },
            }
        });

        let get_time = |index| {
            hierarchy
                .get_property_by_path(&["list_node", index, "@time"])
                .and_then(|p| p.int())
                .unwrap()
        };
        assert_lt!(get_time("0"), get_time("1"));
        assert_lt!(get_time("1"), get_time("2"));
        assert_lt!(get_time("2"), get_time("3"));
    }

    #[fuchsia::test]
    fn test_inspect_log_nested() {
        let (inspector, mut node) = inspector_and_list_node();
        inspect_log!(node, {
            k1: {
                sub1: "subval1",
                sub2: {
                    subsub1: "subsubval1",
                },
                sub3: 3u64,
            },
            k2: if true { 10u64 } else { 20 }
        });

        assert_data_tree!(inspector, root: {
            list_node: {
                "0": {
                    "@time": AnyProperty,
                    k1: {
                        sub1: "subval1",
                        sub2: {
                            subsub1: "subsubval1",
                        },
                        sub3: 3u64,
                    },
                    k2: 10u64
                }
            }
        });
    }

    #[fuchsia::test]
    fn test_inspect_log_var_key_syntax() {
        let (inspector, mut node) = inspector_and_list_node();
        let key = "@@@";
        inspect_log!(node, var key: "!!!");

        assert_data_tree!(inspector, root: {
            list_node: {
                "0": {
                    "@time": AnyProperty,
                    "@@@": "!!!"
                }
            }
        });
    }

    #[fuchsia::test]
    fn test_inspect_log_parsing() {
        // if this test compiles, it's considered as succeeded
        let (_inspector, mut node) = inspector_and_list_node();

        // Non-block version, no trailing comma
        inspect_log!(node, k1: "v1", k2: "v2");

        // Non-block version, trailing comma
        inspect_log!(node, k1: "v1", k2: "v2",);

        // Block version, no trailing comma
        inspect_log!(node, {
            k1: "v1",
            k2: "v2"
        });

        // Block version, trailing comma
        inspect_log!(node, {
            k1: "v1",
            k2: "v2",
        });
    }

    #[fuchsia::test]
    fn test_inspect_log_allows_mutex_guard_temporary() {
        // if this test compiles, it's considered as succeeded
        let (_inspector, node) = inspector_and_list_node();
        let node = Mutex::new(node);
        inspect_log!(node.lock(), k1: "v1");
    }

    #[fuchsia::test]
    fn test_inspect_log_macro_does_not_move_value() {
        // if this test compiles, it's considered as succeeded
        let (_inspector, mut node) = inspector_and_list_node();
        let s = String::from("s");
        inspect_log!(node, s: s);

        // Should not cause compiler error since value is not moved
        println!("{s}");
    }

    #[fuchsia::test]
    fn test_log_option() {
        let (inspector, mut node) = inspector_and_list_node();

        inspect_log!(node, some?: Some("a"));

        inspect_log!(node, none?: None as Option<String>);

        assert_data_tree!(inspector, root: {
            list_node: {
                "0": { "@time": AnyProperty, some: "a" },
                "1": { "@time": AnyProperty },
            }
        });
    }

    #[fuchsia::test]
    fn test_log_inspect_bytes() {
        let (inspector, mut node) = inspector_and_list_node();
        let bytes = [11u8, 22, 33];

        inspect_log!(node, bytes: InspectBytes(&bytes));
        inspect_log!(node, bytes: InspectBytes(&bytes[..]));
        inspect_log!(node, bytes: InspectBytes(bytes));

        assert_data_tree!(inspector, root: {
            list_node: {
                "0": { "@time": AnyProperty, bytes: vec![11u8, 22, 33] },
                "1": { "@time": AnyProperty, bytes: vec![11u8, 22, 33] },
                "2": { "@time": AnyProperty, bytes: vec![11u8, 22, 33] },
            }
        });
    }

    #[fuchsia::test]
    fn test_log_inspect_list() {
        let (inspector, mut node) = inspector_and_list_node();
        let list = [11u8, 22, 33];

        inspect_log!(node, list: InspectList(&list));

        assert_data_tree!(inspector, root: {
            list_node: {
                "0": {
                    "@time": AnyProperty,
                    list: {
                        "0": 11u64,
                        "1": 22u64,
                        "2": 33u64,
                    }
                }
            }
        });
    }

    #[fuchsia::test]
    fn test_log_inspect_list_closure() {
        let (inspector, mut node) = inspector_and_list_node();
        let list = [13u32, 17, 29];
        let list_mapped = InspectListClosure(&list, |node_writer, key, item| {
            inspect_insert!(node_writer, var key: item * 2);
        });

        inspect_log!(node, list: list_mapped);

        assert_data_tree!(inspector, root: {
            list_node: {
                "0": {
                    "@time": AnyProperty,
                    list: {
                        "0": 26u64,
                        "1": 34u64,
                        "2": 58u64,
                    }
                }
            }
        });
    }

    #[fuchsia::test]
    fn test_log_inspect_uint_array() {
        let (inspector, mut node) = inspector_and_list_node();
        let list = [1u32, 2, 3, 4, 5, 6];

        inspect_log!(node, list: InspectUintArray::new(&list));

        assert_data_tree!(inspector, root: {
            list_node: {
                "0": {
                    "@time": AnyProperty,
                    list: vec![1u64, 2, 3, 4, 5, 6],
                }
            }
        });
    }

    #[fuchsia::test]
    fn test_inspect_insert_parsing() {
        // if this test compiles, it's considered as succeeded
        let (_inspector, mut node) = inspector_and_list_node();
        let node_writer = node.add_entry(|node_writer| {
            // Non-block version, no trailing comma
            inspect_insert!(node_writer, k1: "v1".to_string(), k2: if true { 10u64 } else { 20 });
        });

        // Non-block version, trailing comma
        inspect_insert!(node_writer, k1: 1i64, k2: 2f64,);

        // Block version, no trailing comma
        inspect_insert!(node_writer, {
            k1: 1u8,
            k2: 2i8
        });

        // Block version, trailing comma
        inspect_insert!(node_writer, {
            k1: &1u64,
            k2?: Some("v2"),
        });
    }

    #[fuchsia::test]
    fn test_make_inspect_loggable() {
        let (inspector, mut node) = inspector_and_list_node();

        let obj = make_inspect_loggable!(k1: "1", k2: 2i64, k3: "3");
        inspect_log!(node, some_key: obj);

        let point = Some((10i64, 50i64));
        inspect_log!(node, point?: point.map(|(x, y)| make_inspect_loggable!({
            x: x,
            y: y,
        })));

        assert_data_tree!(inspector, root: {
            list_node: {
                "0": {
                    "@time": AnyProperty,
                    some_key: { k1: "1", k2: 2i64, k3: "3" },
                },
                "1": {
                    "@time": AnyProperty,
                    point: { x: 10i64, y: 50i64 },
                },
            }
        });
    }

    #[fuchsia::test]
    fn test_log_inspect_string_reference() {
        let (inspector, mut node) = inspector_and_list_node();

        inspect_log!(node, "foo" => "foo_1");
        inspect_log!(node, "foo" => "foo_2");
        inspect_log!(node, "foo" => "foo_3");
        inspect_log!(node, "foo" => "foo_4");

        assert_data_tree!(inspector, root: {
            list_node: {
                "0": { "@time": AnyProperty, foo: "foo_1" },
                "1": { "@time": AnyProperty, foo: "foo_2" },
                "2": { "@time": AnyProperty, foo: "foo_3" },
                "3": { "@time": AnyProperty, foo: "foo_4" },
            }
        });
    }

    fn inspector_and_list_node() -> (Inspector, BoundedListNode) {
        let inspector = Inspector::default();
        let list_node = inspector.root().create_child("list_node");
        let list_node = BoundedListNode::new(list_node, 10);
        (inspector, list_node)
    }
}
