// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fuchsia_criterion::FuchsiaCriterion;
use fuchsia_criterion::criterion::Criterion;
use futures::executor::block_on;

use fxfs::lsm_tree::cache::NullCache;
use fxfs::lsm_tree::merge::{MergeLayerIterator, MergeResult};
use fxfs::lsm_tree::types::{Item, LayerIterator};
use fxfs::lsm_tree::{LSMTree, Query, compact_with_iterator, layers_from_handles};
use fxfs::object_handle::ObjectHandle;
use fxfs::object_store::ExtentKey;
use fxfs::object_store::journal::CompactionYielder;
use fxfs::object_store::object_record::{AttributeKey, ObjectKey, ObjectKeyData, ObjectValue};
use fxfs::testing::fake_object::{FakeObject, FakeObjectHandle};
use fxfs::testing::writer::Writer;
use std::sync::Arc;

fn emit_left_merge_fn(
    _left: &MergeLayerIterator<'_, ObjectKey, ObjectValue>,
    _right: &MergeLayerIterator<'_, ObjectKey, ObjectValue>,
) -> MergeResult<ObjectKey, ObjectValue> {
    MergeResult::EmitLeft
}

fn create_tree_generic<F>(
    depth: u64,
    size: u64,
    mut populate_layer: F,
) -> LSMTree<ObjectKey, ObjectValue>
where
    F: FnMut(&LSMTree<ObjectKey, ObjectValue>, u64, u64),
{
    let items_per_layer = size / depth;
    let mut handles = Vec::new();

    for layer_idx in 0..depth {
        let layer_tree = LSMTree::new(emit_left_merge_fn, Box::new(NullCache {}));
        populate_layer(&layer_tree, layer_idx, items_per_layer);
        layer_tree.seal();

        let object = Arc::new(FakeObject::new());
        let handle = FakeObjectHandle::new(object.clone());

        block_on(async {
            let layer_set = layer_tree.layer_set();
            let mut merger = layer_set.merger();
            let iter = merger.query(Query::FullScan).await.unwrap();
            compact_with_iterator(
                iter,
                items_per_layer as usize,
                Writer::new(&handle).await,
                handle.block_size(),
                None::<CompactionYielder<'static>>,
            )
            .await
            .unwrap();
        });

        handles.push(FakeObjectHandle::new(object));
    }

    let layers = block_on(async { layers_from_handles(handles).await.unwrap() });

    let tree = LSMTree::new(emit_left_merge_fn, Box::new(NullCache {}));
    tree.set_layers(layers);
    tree
}

fn create_tree(depth: u64, size: u64) -> LSMTree<ObjectKey, ObjectValue> {
    create_tree_generic(depth, size, |tree, layer_idx, items_per_layer| {
        for i in 0..items_per_layer {
            let key_id = i * depth + layer_idx;
            let key = ObjectKey::object(key_id);
            let value = ObjectValue::Some;
            tree.insert(Item::new(key, value)).unwrap();
        }
    })
}

fn create_long_tree(depth: u64, size: u64) -> LSMTree<ObjectKey, ObjectValue> {
    create_tree_generic(depth, size, |tree, layer_idx, items_per_layer| {
        for i in 0..items_per_layer {
            let key_id = i * depth + layer_idx;
            let name = "a".repeat(300);
            let key = ObjectKey { object_id: key_id, data: ObjectKeyData::Child { name } };
            let value = ObjectValue::Some;
            tree.insert(Item::new(key, value)).unwrap();
        }
    })
}

fn create_extent_tree(depth: u64, size: u64) -> LSMTree<ObjectKey, ObjectValue> {
    create_tree_generic(depth, size, |tree, layer_idx, items_per_layer| {
        let mut offset = layer_idx * 1024;
        for _ in 0..items_per_layer {
            let key = ObjectKey {
                object_id: 1,
                data: ObjectKeyData::Attribute(
                    0,
                    AttributeKey::Extent(ExtentKey::new(offset..offset + 1024)),
                ),
            };
            let value = ObjectValue::Some;
            tree.insert(Item::new(key, value)).unwrap();
            offset += depth * 1024;
        }
    })
}

fn bench_lsm_tree(c: &mut Criterion) {
    *c = std::mem::take(c).sample_size(10).warm_up_time(std::time::Duration::from_secs(1));

    for depth in [1] {
        for size in [10_000] {
            // --- Object Record Benchmarks ---
            let tree = Arc::new(create_tree(depth, size));

            let tree_hit = tree.clone();
            c.bench_function(&format!("find_hit_{}_depth_{}", size, depth), move |b| {
                b.iter(|| {
                    let key = ObjectKey::object(size / 2);
                    block_on(async {
                        for _ in 0..10 {
                            let _ = tree_hit.find(&key).await.unwrap();
                        }
                    });
                })
            });

            let tree_scan = tree.clone();
            let layer_set = tree_scan.layer_set();
            c.bench_function(&format!("full_scan_{}_depth_{}", size, depth), move |b| {
                b.iter(|| {
                    block_on(async {
                        for _ in 0..1 {
                            let mut merger = layer_set.merger();
                            let mut iter = merger.query(Query::FullScan).await.unwrap();
                            while iter.get().is_some() {
                                iter.advance().await.unwrap();
                            }
                        }
                    });
                })
            });

            // --- Long Key Benchmarks ---
            let long_tree = Arc::new(create_long_tree(depth, size));

            let tree_long_hit = long_tree.clone();
            c.bench_function(&format!("find_long_hit_{}_depth_{}", size, depth), move |b| {
                b.iter(|| {
                    let key = ObjectKey {
                        object_id: size / 2,
                        data: ObjectKeyData::Child { name: "a".repeat(300) },
                    };
                    block_on(async {
                        for _ in 0..10 {
                            let _ = tree_long_hit.find(&key).await.unwrap();
                        }
                    });
                })
            });

            let tree_long_scan = long_tree.clone();
            let long_layer_set = tree_long_scan.layer_set();
            c.bench_function(&format!("full_long_scan_{}_depth_{}", size, depth), move |b| {
                b.iter(|| {
                    block_on(async {
                        for _ in 0..1 {
                            let mut merger = long_layer_set.merger();
                            let mut iter = merger.query(Query::FullScan).await.unwrap();
                            while iter.get().is_some() {
                                iter.advance().await.unwrap();
                            }
                        }
                    });
                })
            });

            // --- Extent Record Benchmarks ---
            let extent_tree = Arc::new(create_extent_tree(depth, size));

            let tree_extent_scan = extent_tree.clone();
            let extent_layer_set = tree_extent_scan.layer_set();
            c.bench_function(&format!("extent_scan_{}_depth_{}", size, depth), move |b| {
                b.iter(|| {
                    block_on(async {
                        for _ in 0..1 {
                            let mut merger = extent_layer_set.merger();
                            let mut iter = merger.query(Query::FullScan).await.unwrap();
                            while iter.get().is_some() {
                                iter.advance().await.unwrap();
                            }
                        }
                    });
                })
            });

            let tree_extent_range = extent_tree.clone();
            let extent_layer_set_range = tree_extent_range.layer_set();
            c.bench_function(&format!("extent_range_{}_depth_{}", size, depth), move |b| {
                b.iter(|| {
                    let key = ObjectKey {
                        object_id: 1,
                        data: ObjectKeyData::Attribute(
                            0,
                            AttributeKey::Extent(ExtentKey::new(
                                (size / 2 * 1024)..(size / 2 * 1024 + 1024),
                            )),
                        ),
                    };
                    block_on(async {
                        for _ in 0..10 {
                            let mut merger = extent_layer_set_range.merger();
                            let mut iter = merger.query(Query::LimitedRange(&key)).await.unwrap();
                            let query_end = size / 2 * 1024 + 1024;
                            while let Some(item) = iter.get() {
                                if let ObjectKeyData::Attribute(
                                    _,
                                    AttributeKey::Extent(extent_key),
                                ) = &item.key.data
                                {
                                    if extent_key.range.start >= query_end {
                                        break;
                                    }
                                }
                                iter.advance().await.unwrap();
                            }
                        }
                    });
                })
            });
        }
    }
}

fn main() {
    let mut c = FuchsiaCriterion::default();
    bench_lsm_tree(&mut c);
}
