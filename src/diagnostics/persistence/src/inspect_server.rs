// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::borrow::Cow;

use crate::fetcher::TagData;
use crate::file_handler::{self, Timestamps};
use diagnostics_data::{Data, DiagnosticsHierarchy, InspectMetadata, Property};
use fuchsia_inspect::hierarchy::{ExponentialHistogram, LinearHistogram, MissingValue};
use fuchsia_inspect::reader::ArrayContent;
use fuchsia_inspect::{
    ArrayProperty, ExponentialHistogramParams, HistogramProperty, LinearHistogramParams, Node,
    component,
};
use itertools::Either;

fn store_data(node: &fuchsia_inspect::Node, data: TagData) {
    let TagData { max_bytes: _, total_bytes, timestamps, selectors: _, data, errors } = data;

    // Record total bytes.
    node.record_uint("@persist_size", total_bytes as u64);

    // Record errors.
    let array = node.create_string_array("@errors", errors.len());
    for (index, err) in errors.iter().enumerate() {
        array.set(index, err);
    }
    node.record(array);

    // Record timestamps.
    node.record_child("@timestamps", |timestamps_node| {
        let Timestamps { last_sample_boot, last_sample_utc } = timestamps;
        timestamps_node.record_int("last_sample_boot", last_sample_boot.into_nanos());
        timestamps_node.record_int("last_sample_utc", last_sample_utc.into_nanos());
    });

    // Record Inspect data by moniker.
    for (moniker, data) in data {
        node.record_child(moniker.to_string(), |node| {
            let Data { data_source: _, metadata, moniker: _, payload, version: _ } = data;
            let InspectMetadata { errors, name: _, component_url: _, timestamp: _, escrowed: _ } =
                metadata;

            // Record errors, if available.
            if let Some(errs) = errors
                && !errs.is_empty()
            {
                let array = node.create_string_array("@errors", errs.len());
                for (index, err) in errs.into_iter().enumerate() {
                    array.set(index, err.message);
                }
                node.record(array);
            }

            if let Some(payload) = payload {
                // Skip the root payload, as this will always exist.
                if payload.name == "root" {
                    let DiagnosticsHierarchy { name: _, properties, children, missing } = payload;
                    store_hierarchy_inner(node, properties, children, missing);
                } else {
                    store_hierarchy(node, payload);
                }
            }
        })
    }
}

fn store_hierarchy(node: &fuchsia_inspect::Node, data: DiagnosticsHierarchy) {
    let DiagnosticsHierarchy { name, properties, children, missing } = data;
    // Perform an atomic write such that the child only becomes available when
    // its fully populated.
    node.record_child(name, |node| {
        store_hierarchy_inner(node, properties, children, missing);
    });
}

fn store_hierarchy_inner(
    node: &fuchsia_inspect::Node,
    properties: Vec<Property>,
    children: Vec<DiagnosticsHierarchy>,
    missing: Vec<MissingValue>,
) {
    // Record missing values, if available.
    if !missing.is_empty() {
        node.record_child("@missing", |missing_node| {
            for MissingValue { name, reason } in missing {
                missing_node.record_string(name.clone(), format!("{reason:?}"));
            }
        })
    }

    // Record all properties.
    for property in properties {
        match property {
            Property::String(k, v) => {
                node.record_string(k, v);
            }
            Property::Bytes(k, v) => {
                node.record_bytes(k, v);
            }
            Property::Int(k, v) => {
                node.record_int(k, v);
            }
            Property::Uint(k, v) => {
                node.record_uint(k, v);
            }
            Property::Double(k, v) => {
                node.record_double(k, v);
            }
            Property::Bool(k, v) => {
                node.record_bool(k, v);
            }
            Property::DoubleArray(k, v) => v.record(node, k),
            Property::IntArray(k, v) => v.record(node, k),
            Property::UintArray(k, v) => v.record(node, k),
            Property::StringList(k, v) => {
                let array = node.create_string_array(k, v.len());
                for (i, v) in v.iter().enumerate() {
                    array.set(i, v);
                }
                node.record(array);
            }
        }
    }

    // Record all children recursively.
    for child in children {
        store_hierarchy(node, child);
    }
}

/// Data that is able to be represented in the Inspect VMO.
trait InspectData<'a> {
    /// Record the Inspect VMO representation of this type to the specified Inspect node.
    fn record(self, node: &Node, name: impl Into<Cow<'a, str>>);
}

fn get_index_counts<T>(
    indexes: Option<Vec<usize>>,
    counts: Vec<T>,
) -> impl Iterator<Item = (usize, T)> {
    match indexes {
        None => Either::Left(counts.into_iter().enumerate()),
        Some(indexes) => Either::Right(indexes.into_iter().zip(counts)),
    }
}

impl<'a> InspectData<'a> for ArrayContent<f64>
where
    Self: 'a,
{
    fn record(self, node: &Node, name: impl Into<Cow<'a, str>>) {
        match self {
            Self::Values(values) => {
                let array = node.create_double_array(name, values.len());
                for (index, value) in values.iter().enumerate() {
                    array.set(index, *value);
                }
                node.record(array);
            }
            Self::LinearHistogram(LinearHistogram { size, floor, step, counts, indexes }) => {
                let name = name.into();

                let array = node.create_double_linear_histogram(
                    name,
                    LinearHistogramParams {
                        floor,
                        step_size: step,
                        // Do not include underflow and overflow buckets.
                        buckets: size.saturating_sub(2),
                    },
                );

                for (bucket_index, count) in get_index_counts(indexes, counts) {
                    if count < 1.0 {
                        continue;
                    }
                    let value = if bucket_index == 0 {
                        if floor == f64::NEG_INFINITY {
                            // Floor starts at the lowest possible value; there
                            // is no underflow bucket.
                            continue;
                        }
                        f64::NEG_INFINITY
                    } else {
                        floor + step * (bucket_index - 1) as f64
                    };
                    array.insert_multiple(value, count.round() as usize);
                }

                node.record(array);
            }
            Self::ExponentialHistogram(ExponentialHistogram {
                size,
                floor,
                initial_step,
                step_multiplier,
                counts,
                indexes,
            }) => {
                let name = name.into();

                let array = node.create_double_exponential_histogram(
                    name,
                    ExponentialHistogramParams {
                        floor,
                        initial_step,
                        step_multiplier,
                        // Do not include underflow and overflow buckets.
                        buckets: size.saturating_sub(2),
                    },
                );

                for (bucket_index, count) in get_index_counts(indexes, counts) {
                    if count < 1.0 {
                        continue;
                    }
                    let value = if bucket_index == 0 {
                        if floor == f64::NEG_INFINITY {
                            // Floor starts at the lowest possible value; there
                            // is no underflow bucket.
                            continue;
                        }
                        f64::NEG_INFINITY
                    } else if bucket_index == 1 {
                        floor
                    } else {
                        let multiplier = step_multiplier.powi((bucket_index - 2) as i32);
                        initial_step.mul_add(multiplier, floor)
                    };
                    array.insert_multiple(value, count.round() as usize);
                }

                node.record(array);
            }
        }
    }
}

impl<'a> InspectData<'a> for ArrayContent<i64>
where
    Self: 'a,
{
    fn record(self, node: &Node, name: impl Into<Cow<'a, str>>) {
        match self {
            Self::Values(values) => {
                let array = node.create_int_array(name, values.len());
                for (index, value) in values.iter().enumerate() {
                    array.set(index, *value);
                }
                node.record(array);
            }
            Self::LinearHistogram(LinearHistogram { size, floor, step, counts, indexes }) => {
                let name = name.into();

                let array = node.create_int_linear_histogram(
                    name,
                    LinearHistogramParams {
                        floor,
                        step_size: step,
                        // Do not include underflow and overflow buckets.
                        buckets: size.saturating_sub(2),
                    },
                );

                for (bucket_index, count) in get_index_counts(indexes, counts) {
                    if count == 0 {
                        continue;
                    }
                    let value = if bucket_index == 0 {
                        if let Some(res) = floor.checked_sub(step.signum()) {
                            res
                        } else {
                            // Floor starts at either the minimum or maximum
                            // value; there is no underflow bucket.
                            continue;
                        }
                    } else {
                        // Use larger data types to avoid underflow/overflow
                        // while calculating the value.
                        let step = step as i128;
                        let floor = floor as i128;

                        // floor + step * index
                        let value =
                            floor.saturating_add(step.saturating_mul((bucket_index - 1) as i128));

                        if value > i64::MAX as i128 {
                            i64::MAX
                        } else if value < i64::MIN as i128 {
                            i64::MIN
                        } else {
                            value as i64
                        }
                    };
                    array.insert_multiple(value, count as usize);
                }

                node.record(array);
            }
            Self::ExponentialHistogram(ExponentialHistogram {
                size,
                floor,
                initial_step,
                step_multiplier,
                counts,
                indexes,
            }) => {
                let name = name.into();

                let array = node.create_int_exponential_histogram(
                    name,
                    ExponentialHistogramParams {
                        floor,
                        initial_step,
                        step_multiplier,
                        // Do not include underflow and overflow buckets.
                        buckets: size.saturating_sub(2),
                    },
                );

                for (bucket_index, count) in get_index_counts(indexes, counts) {
                    if count == 0 {
                        continue;
                    }
                    let value = if bucket_index == 0 {
                        if let Some(res) = floor.checked_sub(initial_step.signum()) {
                            res
                        } else {
                            // Floor starts at either the minimum or maximum
                            // value; there is no underflow bucket.
                            continue;
                        }
                    } else if bucket_index == 1 {
                        floor
                    } else {
                        // Use larger data types to avoid underflow/overflow
                        // while calculating the value.
                        let step_multiplier = step_multiplier as i128;
                        let initial_step = initial_step as i128;
                        let floor = floor as i128;

                        // floor + initial_step * step_multiplier^(index)
                        let value = floor.saturating_add(initial_step.saturating_mul(
                            step_multiplier.saturating_pow((bucket_index - 2) as u32),
                        ));

                        if value > i64::MAX as i128 {
                            i64::MAX
                        } else if value < i64::MIN as i128 {
                            i64::MIN
                        } else {
                            value as i64
                        }
                    };
                    array.insert_multiple(value, count as usize);
                }

                node.record(array);
            }
        }
    }
}

impl<'a> InspectData<'a> for ArrayContent<u64>
where
    Self: 'a,
{
    fn record(self, node: &Node, name: impl Into<Cow<'a, str>>) {
        match self {
            Self::Values(values) => {
                let array = node.create_uint_array(name, values.len());
                for (index, value) in values.iter().enumerate() {
                    array.set(index, *value);
                }
                node.record(array);
            }
            Self::LinearHistogram(LinearHistogram { size, floor, step, counts, indexes }) => {
                let name = name.into();

                let array = node.create_uint_linear_histogram(
                    name,
                    LinearHistogramParams {
                        floor,
                        step_size: step,
                        // Do not include underflow and overflow buckets.
                        buckets: size.saturating_sub(2),
                    },
                );

                for (bucket_index, count) in get_index_counts(indexes, counts) {
                    if count == 0 {
                        continue;
                    }
                    let value = if bucket_index == 0 {
                        if floor == u64::MIN {
                            // Floor starts at the minimum value; there is no
                            // underflow bucket.
                            continue;
                        }
                        u64::MIN
                    } else {
                        // floor + step * index
                        floor.saturating_add(step.saturating_mul((bucket_index - 1) as u64))
                    };
                    array.insert_multiple(value, count as usize);
                }

                node.record(array);
            }
            Self::ExponentialHistogram(ExponentialHistogram {
                size,
                floor,
                initial_step,
                step_multiplier,
                counts,
                indexes,
            }) => {
                let name = name.into();

                let array = node.create_uint_exponential_histogram(
                    name,
                    ExponentialHistogramParams {
                        floor,
                        initial_step,
                        step_multiplier,
                        // Do not include underflow and overflow buckets.
                        buckets: size.saturating_sub(2),
                    },
                );

                for (bucket_index, count) in get_index_counts(indexes, counts) {
                    if count == 0 {
                        continue;
                    }
                    let value = if bucket_index == 0 {
                        if floor == u64::MIN {
                            // Floor starts at the minimum value; there is no
                            // underflow bucket.
                            continue;
                        }
                        u64::MIN
                    } else if bucket_index == 1 {
                        floor
                    } else {
                        // floor + initial_step * step_multiplier^(index)
                        floor.saturating_add(initial_step.saturating_mul(
                            step_multiplier.saturating_pow((bucket_index - 2) as u32),
                        ))
                    };
                    array.insert_multiple(value, count as usize);
                }

                node.record(array);
            }
        }
    }
}

pub async fn record_persist_node(name: &str) -> Result<(), anyhow::Error> {
    if let Some(data) = file_handler::previous_data().await? {
        component::inspector().root().record_child(name, |persist_node| {
            for (service, service_data) in data.0 {
                persist_node.record_child(service.to_string(), |service_node| {
                    for (tag, tag_data) in service_data.0 {
                        service_node.record_child(tag.to_string(), |tag_node| {
                            store_data(tag_node, tag_data);
                        })
                    }
                });
            }
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::str::FromStr;

    use super::*;
    use anyhow::Error;
    use diagnostics_assertions::{PropertyAssertion, assert_data_tree};
    use diagnostics_data::{InspectError, InspectHandleName, hierarchy};
    use flyweights::FlyStr;
    use fuchsia_inspect::Inspector;
    use hashbrown::HashMap;
    use test_case::test_case;
    use zx::Instant;

    // Verify TagData is published to Inspect in the format we expect.
    #[fuchsia::test]
    async fn store_data_works() -> Result<(), Error> {
        let inspector = Inspector::default();
        let inspect = inspector.root();
        assert_data_tree!(
            inspector,
            root: contains {
            }
        );

        let moniker = diagnostics_data::ExtendedMoniker::from_str("types").unwrap();

        let tag_data = TagData {
            max_bytes: 1,
            total_bytes: 2,
            timestamps: Timestamps {
                last_sample_boot: zx::BootInstant::from_nanos(3),
                last_sample_utc: fuchsia_runtime::UtcInstant::from_nanos(4),
            },
            selectors: vec![],
            data: HashMap::from([(
                moniker.clone().into(),
                Data {
                    data_source: diagnostics_data::DataSource::Inspect,
                    metadata: InspectMetadata {
                        errors: Some(vec![InspectError {
                            message: "test InspectError".to_string(),
                        }]),
                        name: InspectHandleName::Name(FlyStr::new("inspect_handle_name")),
                        component_url: FlyStr::new("component_url"),
                        timestamp: Instant::from_nanos(0),
                        escrowed: false,
                    },
                    moniker,
                    payload: Some(hierarchy! {
                        root: {
                            negint: -5,
                            int: 42,
                            unsigned: 9223372036854775808u64,
                            float: 45.6f64,
                            bool: true,
                            obj: {
                                child: "child",
                                grandchild: {
                                    hello: "world",
                                }
                            }
                        }
                    }),
                    version: 1,
                },
            )]),
            errors: VecDeque::from(["test TagData error".to_string()]),
        };

        store_data(inspect, tag_data);

        assert_data_tree!(
            inspector,
            root: {
                "@errors": vec!["test TagData error"],
                "@persist_size": 2u64,
                "@timestamps": {
                    last_sample_boot: 3,
                    last_sample_utc: 4,
                },
                types: {
                    "@errors": vec!["test InspectError"],
                    negint: -5i64,
                    int: 42i64,
                    unsigned: 9223372036854775808u64,
                    float: 45.6f64,
                    bool: true,
                    obj: {
                        child: "child",
                        grandchild: {
                            hello: "world",
                        }
                    }
                }
            }
        );
        Ok(())
    }

    #[test_case(vec![1.0, 2.0, 3.0, 4.0] ; "f64_many")]
    #[test_case(vec![1i64, 2i64, 3i64, 4i64] ; "i64_many")]
    #[test_case(vec![1u64, 2u64, 3u64, 4u64] ; "u64_many")]
    #[test_case(vec![1.0] ; "f64_one")]
    #[test_case(vec![1i64] ; "i64_one")]
    #[test_case(vec![1u64] ; "u64_one")]
    #[test_case(Vec::<f64>::new() ; "f64_none")]
    #[test_case(Vec::<i64>::new() ; "i64_none")]
    #[test_case(Vec::<u64>::new() ; "u64_none")]
    #[fuchsia::test]
    async fn record_values<'a, T>(values: Vec<T>)
    where
        ArrayContent<T>: InspectData<'a>,
        Vec<T>: PropertyAssertion,
        T: Clone + 'static,
    {
        let inspector = Inspector::default();
        ArrayContent::Values(values.clone()).record(inspector.root(), "child");
        assert_data_tree!(
            inspector,
            root: {
                child: values,
            }
        );
    }

    #[test_case(
        LinearHistogram {
            size: 4,
            floor: 0.0,
            step: 10.0,
            counts: vec![1.0, 2.0, 3.0, 4.0],
            indexes: None,
        } ;
        "f64_dense"
    )]
    #[test_case(
        LinearHistogram {
            size: 4,
            floor: 0i64,
            step: 10i64,
            counts: vec![1i64, 2i64, 3i64, 4i64],
            indexes: None,
        } ;
        "i64_dense"
    )]
    #[test_case(
        LinearHistogram {
            size: 4,
            floor: 0u64,
            step: 10u64,
            // Not possible to have an underflow bucket with floor = 0.
            counts: vec![0u64, 2u64, 3u64, 4u64],
            indexes: None,
        } ;
        "u64_dense"
    )]
    #[test_case(
        LinearHistogram {
            size: 4,
            floor: 0.0,
            step: 10.0,
            counts: vec![5.0],
            indexes: Some(vec![1]),
        } ;
        "f64_sparse"
    )]
    #[test_case(
        LinearHistogram {
            size: 4,
            floor: 0i64,
            step: 10i64,
            counts: vec![5i64],
            indexes: Some(vec![1]),
        } ;
        "i64_sparse"
    )]
    #[test_case(
        LinearHistogram {
            size: 4,
            floor: 0u64,
            step: 10u64,
            counts: vec![5u64],
            indexes: Some(vec![1]),
        } ;
        "u64_sparse"
    )]
    // | Bucket | Range       |
    // | ------ | ----------- |
    // |      0 | (-inf, MIN) |
    // |      1 | [MIN, 0)    |
    // |      2 | [0, MAX)    |
    // |      3 | [MAX, +inf) |
    #[test_case(
        // f64 defines its underflow/overflow buckets ranges based on -inf/+inf
        // instead of min/max values.
        LinearHistogram {
            size: 4,
            floor: f64::MIN,
            step: f64::MAX,
            counts: vec![1.0, 2.0, 3.0, 4.0],
            indexes: None,
        } ;
        "f64_bounds"
    )]
    // | Bucket | Range          |
    // | ------ | -------------- |
    // |      0 | (-inf, MIN)    |
    // |      1 | [MIN-1, MIN-2) |
    // |      2 | [MIN-2, +inf)  |
    #[test_case(
        // Should not have an underflow bucket due to bucket ranges.
        LinearHistogram {
            size: 3,
            floor: i64::MIN,
            step: 1,
            counts: vec![0i64, 1i64, 2i64],
            indexes: None,
        } ;
        "i64_bounds_min"
    )]
    // | Bucket | Range         |
    // | ------ | ------------- |
    // |      0 | (-inf, MIN+1) |
    // |      1 | [MIN+1, 0)    |
    // |      2 | [0, MAX)      |
    // |      3 | [MAX, +inf)   |
    #[test_case(
        LinearHistogram {
            size: 4,
            // MIN is 1 less than MAX due to two's complement. Modify this such
            // that bucket ends with MAX.
            floor: i64::MIN+1,
            step: i64::MAX,
            counts: vec![1i64, 2i64, 3i64, 4i64],
            indexes: None,
        } ;
        "i64_bounds_max"
    )]
    #[test_case(
        LinearHistogram {
            size: 3,
            floor: u64::MIN,
            step: u64::MAX / 2,
            // Should not have an underflow bucket due to bucket ranges.
            counts: vec![0u64, 1u64, 2u64],
            indexes: None,
        } ;
        "u64_bounds"
    )]
    #[fuchsia::test]
    async fn record_linear_histogram<'a, T>(histogram: LinearHistogram<T>)
    where
        ArrayContent<T>: InspectData<'a>,
        LinearHistogram<T>: PropertyAssertion + Clone,
        T: 'static,
    {
        let inspector = Inspector::default();
        ArrayContent::LinearHistogram(histogram.clone()).record(inspector.root(), "child");
        assert_data_tree!(
            inspector,
            root: {
                child: histogram,
            }
        );
    }

    // Table for the dense/sparse exponential tests:
    //
    // | Bucket | Range            |
    // | ------ | ---------------- |
    // |      0 | (-inf, 1)        |
    // |      1 | [1, 1+1*2^0 = 2) |
    // |      2 | [2, 1+1*2^1 = 3) |
    // |      3 | [3, 1+1*2^2 = 5) |
    // |      4 | [5, 1+1*2^3 = 9) |
    // |      5 | [9, +inf)        |
    #[test_case(
        ExponentialHistogram {
            size: 6,
            floor: 1.0,
            initial_step: 1.0,
            step_multiplier: 2.0,
            counts: vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0],
            indexes: None,
        } ;
        "f64_dense"
    )]
    #[test_case(
        ExponentialHistogram {
            size: 6,
            floor: 1i64,
            initial_step: 1i64,
            step_multiplier: 2i64,
            counts: vec![1i64, 2i64, 3i64, 4i64, 5i64, 6i64],
            indexes: None,
        } ;
        "i64_dense"
    )]
    #[test_case(
        ExponentialHistogram {
            size: 6,
            floor: 1u64,
            initial_step: 1u64,
            step_multiplier: 2u64,
            counts: vec![1u64, 2u64, 3u64, 4u64, 5u64, 6u64],
            indexes: None,
        } ;
        "u64_dense"
    )]
    #[test_case(
        ExponentialHistogram {
            size: 10,
            floor: 1.0,
            initial_step: 1.0,
            step_multiplier: 2.0,
            counts: vec![5.0],
            indexes: Some(vec![5]),
        } ;
        "f64_sparse"
    )]
    #[test_case(
        ExponentialHistogram {
            size: 10,
            floor: 1i64,
            initial_step: 1i64,
            step_multiplier: 2i64,
            counts: vec![5i64],
            indexes: Some(vec![5]),
        } ;
        "i64_sparse"
    )]
    #[test_case(
        ExponentialHistogram {
            size: 10,
            floor: 1u64,
            initial_step: 1u64,
            step_multiplier: 2u64,
            counts: vec![5u64],
            indexes: Some(vec![5]),
        } ;
        "u64_sparse"
    )]
    // | Bucket | Range        |
    // | ------ | ------------ |
    // |      0 | (-inf, MIN)  |
    // |      1 | [MIN, 0)     |
    // |      2 | [0, MAX)     |
    // |      3 | [MAX, +inf)  |
    //
    #[test_case(
        ExponentialHistogram {
            size: 4,
            floor: f64::MIN,
            initial_step: f64::MAX,
            step_multiplier: 2.0,
            counts: vec![1.0, 2.0, 3.0, 4.0],
            indexes: None,
        } ;
        "f64_bounds"
    )]
    // | Bucket | Range         |
    // | ------ | ------------- |
    // |      0 | (-inf, MIN+1) |
    // |      1 | [MIN+1, 0)    |
    // |      2 | [0, MAX)      |
    // |      3 | [MAX, +inf)   |
    #[test_case(
        ExponentialHistogram {
            size: 4,
            // MIN is 1 less than MAX due to two's complement. Modify this such
            // that bucket ends with MAX.
            floor: i64::MIN + 1,
            initial_step: i64::MAX,
            step_multiplier: 2i64,
            counts: vec![1i64, 2i64, 3i64, 4i64],
            indexes: None,
        } ;
        "i64_bounds"
    )]
    // | Bucket | Range        |
    // | ------ | ------------ |
    // |      0 | (-inf, 0)    |
    // |      1 | [0, MAX)     |
    // |      2 | [MAX, +inf)  |
    #[test_case(
        ExponentialHistogram {
            size: 3,
            floor: u64::MIN,
            initial_step: u64::MAX,
            step_multiplier: 2u64,
            // Not possible to go below u64::MIN.
            counts: vec![0u64, 1u64, 2u64],
            indexes: None,
        } ;
        "u64_bounds"
    )]
    #[fuchsia::test]
    async fn record_exponential_histogram<'a, T>(histogram: ExponentialHistogram<T>)
    where
        ArrayContent<T>: InspectData<'a>,
        ExponentialHistogram<T>: PropertyAssertion + Clone,
        T: 'static,
    {
        let inspector = Inspector::default();
        ArrayContent::ExponentialHistogram(histogram.clone()).record(inspector.root(), "child");
        assert_data_tree!(
            inspector,
            root: {
                child: histogram,
            }
        );
    }
}
