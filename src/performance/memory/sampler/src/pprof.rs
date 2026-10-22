// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! # `pprof`-related utilities
//!
//! This crate provides a `build_profile` function that can be used to turn
//! profiling information gathered by `memory_sampler` into a
//! `pprof`-compatible profile.
pub use profile_rust_proto::perfetto::third_party::perftools::profiles as pproto;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Bound::{Excluded, Unbounded};
use std::rc::Rc;

use fidl_fuchsia_memory_sampler::ModuleMap;
use itertools::Itertools;

use crate::profile_builder::{
    DeadAllocationCounter, DeallocationCounter, LiveAllocation, StackTrace,
};

/// Pprof interns most strings: it keeps a table of known strings and replaces
/// most occurrences of strings in its data by their index. This structure
/// implements that behavior.
struct StringTable {
    strings: HashMap<String, i64>,
    table: Vec<String>,
}

impl StringTable {
    pub fn new() -> StringTable {
        let mut st = StringTable { strings: HashMap::new(), table: vec![] };
        st.intern("");
        st
    }
    /// If a string is already known to the table, return its index. Otherwise,
    /// insert it and return its newly created index.
    pub fn intern(&mut self, string: &str) -> i64 {
        *self.strings.entry(string.to_string()).or_insert_with(|| {
            let index = self.table.len();
            self.table.push(string.to_string());
            index as i64
        })
    }
    fn finalize(self) -> Vec<String> {
        self.table
    }
}

/// Helper structure to match an address to a memory range.
struct Mapping {
    ranges: BTreeMap<u64, (u64, u64)>, // memory_limit -> (memory_start, mapping_id)
}

impl Mapping {
    /// Produce a collection of `pprof`'s `Mapping` for use in a
    /// profile, and an index to map an address to the corresponding
    /// mapping.
    fn build<'a>(
        modules: impl Iterator<Item = &'a ModuleMap>,
        st: &mut StringTable,
    ) -> (Mapping, Vec<pproto::Mapping>) {
        let (ranges, table) = modules
            // Distribute the build_id on each segment.
            .flat_map(|ModuleMap { executable_segments, build_id, .. }| {
                let build_id = build_id.as_ref().map_or(0, |id| st.intern(&hex::encode(id)));
                executable_segments.iter().flatten().zip(std::iter::repeat(build_id))
            })
            // Use the index of the collection as a unique id.
            .enumerate()
            .filter_map(|(id, (segment, build_id))| {
                // ID 0 is reserved and cannot be used in the mapping.
                let id = id as u64 + 1;
                let memory_start = segment.start_address?;
                let memory_limit = memory_start + segment.size?;
                let file_offset = segment.relative_address?;
                let range = (memory_limit, (memory_start, id));
                let mapping = pproto::Mapping {
                    id,
                    memory_start,
                    memory_limit,
                    file_offset,
                    build_id,
                    ..Default::default()
                };
                Some((range, mapping))
            })
            .unzip();
        (Mapping { ranges }, table)
    }
    /// Resolve an address to the corresponding mapping_id, or return
    /// 0 if not found.
    fn resolve(&self, address: u64) -> u64 {
        if let Some((memory_limit, (memory_start, mapping_id))) =
            self.ranges.range((Excluded(address), Unbounded)).next()
        {
            assert!(address < *memory_limit);
            if *memory_start <= address {
                return *mapping_id;
            }
        }
        0 // Not found
    }
}

fn init_profile(st: &mut StringTable, comments: Vec<i64>) -> pproto::Profile {
    pproto::Profile {
        // This table describes the types of values contained in each sample.
        sample_type: vec![
            pproto::ValueType {
                r#type: st.intern("residual_allocated_objects"),
                unit: st.intern("count"),
            },
            pproto::ValueType {
                r#type: st.intern("residual_allocated_space"),
                unit: st.intern("bytes"),
            },
            pproto::ValueType { r#type: st.intern("allocated_objects"), unit: st.intern("count") },
            pproto::ValueType { r#type: st.intern("allocated_space"), unit: st.intern("bytes") },
            pproto::ValueType {
                r#type: st.intern("deallocated_objects"),
                unit: st.intern("count"),
            },
            pproto::ValueType { r#type: st.intern("deallocated_space"), unit: st.intern("bytes") },
        ],
        default_sample_type: 3, // Clients should default to showing the allocated space.
        period_type: Some(pproto::ValueType {
            r#type: st.intern("space"),
            unit: st.intern("bytes"),
        }),
        period: 1,
        comment: comments,
        ..pproto::Profile::default()
    }
}

/// Build a `pprof`-compatible profile from a program address mapping
/// and its allocation/deallocation information.
pub fn build_profile<'a>(
    modules: impl Iterator<Item = &'a ModuleMap>,
    live_allocations: impl Iterator<Item = &'a LiveAllocation>,
    dead_allocations: HashMap<Rc<StackTrace>, DeadAllocationCounter>,
    deallocations: HashMap<Rc<StackTrace>, DeallocationCounter>,
    stack_traces: &'a HashSet<Rc<StackTrace>>,
    is_lossy: bool,
) -> pproto::Profile {
    let mut st = StringTable::new();
    let mut comments = Vec::new();
    if is_lossy {
        comments.push(st.intern(
            "WARNING: Socket buffer saturated. Dropped allocation and deallocation datagrams. Residual memory may exhibit phantom leaks, and cumulative allocation volumes are under-reported and skewed.",
        ));
    }
    let mut pprof = init_profile(&mut st, comments);

    {
        let (mp, mappings) = Mapping::build(modules, &mut st);
        pprof.mapping = mappings;

        pprof.location = stack_traces
            .iter()
            .flat_map(|st| st.iter())
            .unique()
            .map(|&address| pproto::Location {
                id: address,
                mapping_id: mp.resolve(address),
                address,
                ..Default::default()
            })
            .collect();
    }

    // Live allocations
    {
        let samples = live_allocations.map(|LiveAllocation { size, scale_factor, stack_trace }| {
            let scaled_size = ((*size as f64) * scale_factor).round() as i64;
            let scaled_count = scale_factor.round() as i64;
            pproto::Sample {
                value: vec![scaled_count, scaled_size, scaled_count, scaled_size, 0, 0],
                location_id: stack_trace.to_vec(),
                ..Default::default()
            }
        });
        pprof.sample.extend(samples);
    }
    // Dead allocations
    {
        let samples = dead_allocations.into_iter().map(
            |(stack_trace, DeadAllocationCounter { count, total_size })| pproto::Sample {
                value: vec![0, 0, count.round() as i64, total_size.round() as i64, 0, 0],
                location_id: stack_trace.to_vec(),
                ..Default::default()
            },
        );
        pprof.sample.extend(samples);
    }

    // DeallocationCounters
    {
        let samples = deallocations.into_iter().map(
            |(stack_trace, DeallocationCounter { count, total_size })| pproto::Sample {
                value: vec![0, 0, 0, 0, count.round() as i64, total_size.round() as i64],
                location_id: stack_trace.to_vec(),
                ..Default::default()
            },
        );
        pprof.sample.extend(samples);
    }
    pprof.string_table = st.finalize();
    pprof
}

/// Merge an older partial `Profile` with a newer (partial or final) `Profile`
/// from the same process.
///
/// Because `ProfileBuilder::build_partial_profile` retains `live_allocations`
/// across partial profiles, every live allocation in `older` (`value[0] > 0`)
/// is guaranteed to appear again in `newer` (either still in `live_allocations`
/// if un-freed, or moved to `dead_allocations` if freed in the interim).
/// Therefore, `older`'s live-allocation samples are discarded, while `older`'s
/// dead-allocation and deallocation samples (`value[0] == 0 && value[1] == 0`)
/// are summed with `newer`'s samples by stack trace.
pub fn merge_same_process_profiles(
    older: pproto::Profile,
    newer: pproto::Profile,
) -> pproto::Profile {
    let mut st = StringTable::new();
    let comments = older
        .comment
        .iter()
        .filter_map(|&idx| older.string_table.get(idx as usize))
        .chain(newer.comment.iter().filter_map(|&idx| newer.string_table.get(idx as usize)))
        .filter(|s| !s.is_empty())
        .unique()
        .map(|s| st.intern(s))
        .collect();
    let mut pprof = init_profile(&mut st, comments);

    // Deduplicate mappings across `older` and `newer` and record old_id -> new_id.
    let mut mapping_dedup: HashMap<(u64, u64, u64, String, String), u64> = HashMap::new();
    let mut older_map_ids: HashMap<u64, u64> = HashMap::new();
    let mut newer_map_ids: HashMap<u64, u64> = HashMap::new();

    for (mappings, src_st, id_map) in [
        (&older.mapping, &older.string_table, &mut older_map_ids),
        (&newer.mapping, &newer.string_table, &mut newer_map_ids),
    ] {
        for m in mappings {
            let build_id_str = src_st.get(m.build_id as usize).cloned().unwrap_or_default();
            let filename_str = src_st.get(m.filename as usize).cloned().unwrap_or_default();
            let key = (
                m.memory_start,
                m.memory_limit,
                m.file_offset,
                build_id_str.clone(),
                filename_str.clone(),
            );
            let new_id = *mapping_dedup.entry(key).or_insert_with(|| {
                let id = pprof.mapping.len() as u64 + 1;
                let build_id = if build_id_str.is_empty() { 0 } else { st.intern(&build_id_str) };
                let filename = if filename_str.is_empty() { 0 } else { st.intern(&filename_str) };
                pprof.mapping.push(pproto::Mapping {
                    id,
                    memory_start: m.memory_start,
                    memory_limit: m.memory_limit,
                    file_offset: m.file_offset,
                    filename,
                    build_id,
                    ..Default::default()
                });
                id
            });
            id_map.insert(m.id, new_id);
        }
    }

    let mut older_loc_to_addr: HashMap<u64, u64> = HashMap::with_capacity(older.location.len());
    let mut newer_loc_to_addr: HashMap<u64, u64> = HashMap::with_capacity(newer.location.len());
    let mut addr_to_mapping_id: HashMap<u64, u64> = HashMap::new();

    for loc in &older.location {
        older_loc_to_addr.insert(loc.id, loc.address);
        let mapped_id = older_map_ids.get(&loc.mapping_id).copied().unwrap_or(0);
        let entry = addr_to_mapping_id.entry(loc.address).or_insert(0);
        if *entry == 0 && mapped_id != 0 {
            *entry = mapped_id;
        }
    }
    for loc in &newer.location {
        newer_loc_to_addr.insert(loc.id, loc.address);
        let mapped_id = newer_map_ids.get(&loc.mapping_id).copied().unwrap_or(0);
        let entry = addr_to_mapping_id.entry(loc.address).or_insert(0);
        if mapped_id != 0 {
            *entry = mapped_id;
        }
    }

    let mut live_samples: Vec<pproto::Sample> = Vec::new();
    let mut dead_allocations: HashMap<Vec<u64>, (i64, i64)> = HashMap::new();
    let mut deallocations: HashMap<Vec<u64>, (i64, i64)> = HashMap::new();

    let mut ingest_sample =
        |sample: pproto::Sample, loc_to_addr: &HashMap<u64, u64>, keep_live: bool| {
            if sample.value.len() < 6 {
                return;
            }
            let is_live = sample.value[0] > 0 || sample.value[1] > 0;
            if is_live && !keep_live {
                return;
            }
            let stack: Vec<u64> = sample
                .location_id
                .into_iter()
                .map(|id| loc_to_addr.get(&id).copied().unwrap_or(id))
                .collect();
            if is_live {
                live_samples.push(pproto::Sample {
                    value: sample.value,
                    location_id: stack,
                    ..Default::default()
                });
            } else {
                if sample.value[2] > 0 || sample.value[3] > 0 {
                    let (count, total_size) = dead_allocations.entry(stack.clone()).or_default();
                    *count += sample.value[2];
                    *total_size += sample.value[3];
                }
                if sample.value[4] > 0 || sample.value[5] > 0 {
                    let (count, total_size) = deallocations.entry(stack).or_default();
                    *count += sample.value[4];
                    *total_size += sample.value[5];
                }
            }
        };

    for sample in older.sample {
        ingest_sample(sample, &older_loc_to_addr, false);
    }
    for sample in newer.sample {
        ingest_sample(sample, &newer_loc_to_addr, true);
    }

    pprof.sample.extend(live_samples);
    pprof.sample.extend(dead_allocations.into_iter().map(|(stack, (count, total_size))| {
        pproto::Sample {
            value: vec![0, 0, count, total_size, 0, 0],
            location_id: stack,
            ..Default::default()
        }
    }));
    pprof.sample.extend(deallocations.into_iter().map(|(stack, (count, total_size))| {
        pproto::Sample {
            value: vec![0, 0, 0, 0, count, total_size],
            location_id: stack,
            ..Default::default()
        }
    }));

    pprof.location = pprof
        .sample
        .iter()
        .flat_map(|s| s.location_id.iter())
        .unique()
        .map(|&address| pproto::Location {
            id: address,
            mapping_id: addr_to_mapping_id.get(&address).copied().unwrap_or(0),
            address,
            ..Default::default()
        })
        .collect();
    pprof.string_table = st.finalize();
    pprof
}

#[cfg(test)]
mod test {
    use super::{build_profile, merge_same_process_profiles};
    use crate::profile_builder::{DeadAllocationCounter, DeallocationCounter, LiveAllocation};
    use fidl_fuchsia_memory_sampler::{ExecutableSegment, ModuleMap};
    use std::collections::{HashMap, HashSet};
    use std::rc::Rc;

    mod string_table {
        use crate::pprof::StringTable;
        use std::iter::zip;

        /// `pprof` mandates that the string table starts with the empty
        /// string.
        #[fuchsia::test]
        fn test_string_table_starts_with_empty_string() {
            let result = StringTable::new().finalize();

            assert_eq!(vec![""], result);
        }

        /// Ensure that the string table knows to resolve the empty
        /// string.
        #[fuchsia::test]
        fn test_string_table_knows_empty_string() {
            let mut st = StringTable::new();

            assert_ne!(0, st.intern("word"));
            assert_eq!(0, st.intern(""));
        }

        /// Ensure the string table can intern unique words.
        #[fuchsia::test]
        fn test_string_table_interns_unique_words() {
            let mut st = StringTable::new();
            let words = vec!["Those", "are", "unique", "words"];
            let indices: Vec<i64> = words.iter().map(|w| st.intern(w)).collect();
            let result = st.finalize();

            for (index, word) in zip(indices, words.iter()) {
                assert_eq!(*word, result[index as usize]);
            }
        }

        /// Ensure the string table can intern repeated words, and reuses
        /// the same entries.
        #[fuchsia::test]
        fn test_string_table_interns_repeated_words() {
            let mut st = StringTable::new();
            let words = vec!["word", "word", "word", "word"];
            let indices: Vec<i64> = words.iter().map(|w| st.intern(w)).collect();
            let result = st.finalize();

            let repeated = indices[0];
            for (index, word) in zip(indices, words.iter()) {
                assert_eq!(*word, result[index as usize]);
                assert_eq!(repeated, index);
            }
        }
    }

    mod mapping {
        use crate::pprof::{Mapping, StringTable};
        use fidl_fuchsia_memory_sampler::{ExecutableSegment, ModuleMap};

        #[fuchsia::test]
        fn test_mapping_can_resolve() {
            let mut st = StringTable::new();
            let modules = vec![
                // Describes a module that lives between addresses 0 and 100.
                ModuleMap {
                    executable_segments: Some(vec![ExecutableSegment {
                        start_address: Some(0),
                        size: Some(100),
                        relative_address: Some(0),
                        ..Default::default()
                    }]),
                    build_id: Some(vec![0, 0, 0, 0]),
                    ..Default::default()
                },
                // Describes a module that lives between addresses 1000 and 2000.
                ModuleMap {
                    executable_segments: Some(vec![
                        ExecutableSegment {
                            start_address: Some(1000),
                            size: Some(500),
                            relative_address: Some(0),
                            ..Default::default()
                        },
                        ExecutableSegment {
                            start_address: Some(1500),
                            size: Some(500),
                            relative_address: Some(500),
                            ..Default::default()
                        },
                    ]),
                    build_id: Some(vec![1, 0, 0, 0]),
                    ..Default::default()
                },
            ];

            let (mapping, table) = Mapping::build(modules.iter(), &mut st);
            let strings = st.finalize();
            let id_to_mapping = {
                let mut im = vec![0; table.len() + 1];
                for (i, crate::pprof::pproto::Mapping { id, .. }) in table.iter().enumerate() {
                    im[*id as usize] = i;
                }
                im
            };
            let lookup_by_address =
                |address| &table[id_to_mapping[mapping.resolve(address) as usize]];

            {
                // Address 1 resolves to the first segment of the first module.
                let address = 1;
                let expected_module = &modules[0];
                let expected_segment = &expected_module.executable_segments.as_ref().unwrap()[0];

                let segment = lookup_by_address(address);
                {
                    let expected_build_id =
                        hex::encode(&expected_module.build_id.as_ref().unwrap()[..]);
                    let build_id = &strings[segment.build_id as usize];
                    assert_eq!(expected_build_id, *build_id);
                }
                {
                    let expected_start = expected_segment.start_address.unwrap();
                    let expected_end = expected_start + expected_segment.size.unwrap();
                    assert_eq!(expected_start, segment.memory_start);
                    assert_eq!(expected_end, segment.memory_limit);
                    assert_eq!(expected_segment.relative_address.unwrap(), segment.file_offset);
                }
            }

            {
                // Address 1000 resolves to the first segment of the second module
                let address = 1000;
                let expected_module = &modules[1];
                let expected_segment = &expected_module.executable_segments.as_ref().unwrap()[0];

                let segment = lookup_by_address(address);
                {
                    let expected_build_id =
                        hex::encode(&expected_module.build_id.as_ref().unwrap()[..]);
                    let build_id = &strings[segment.build_id as usize];
                    assert_eq!(expected_build_id, *build_id);
                }
                {
                    let expected_start = expected_segment.start_address.unwrap();
                    let expected_end = expected_start + expected_segment.size.unwrap();
                    assert_eq!(expected_start, segment.memory_start);
                    assert_eq!(expected_end, segment.memory_limit);
                    assert_eq!(expected_segment.relative_address.unwrap(), segment.file_offset);
                }
            }

            {
                // Address 1700 resolves to the second module
                let address = 1700;
                let expected_module = &modules[1];
                let expected_segment = &expected_module.executable_segments.as_ref().unwrap()[1];

                let segment = lookup_by_address(address);
                {
                    let expected_build_id =
                        hex::encode(&expected_module.build_id.as_ref().unwrap()[..]);
                    let build_id = &strings[segment.build_id as usize];
                    assert_eq!(expected_build_id, *build_id);
                }

                {
                    let expected_start = expected_segment.start_address.unwrap();
                    let expected_end = expected_start + expected_segment.size.unwrap();
                    assert_eq!(expected_start, segment.memory_start);
                    assert_eq!(expected_end, segment.memory_limit);
                    assert_eq!(expected_segment.relative_address.unwrap(), segment.file_offset);
                }
            }

            {
                // Address 500 is not found.
                let address_not_found = 500;
                assert_eq!(0, mapping.resolve(address_not_found));
            }
        }
    }

    #[fuchsia::test]
    fn test_build_profile() {
        let modules = vec![
            // Describes a module that lives between addresses 0 and 100.
            ModuleMap {
                executable_segments: Some(vec![ExecutableSegment {
                    start_address: Some(0),
                    size: Some(100),
                    relative_address: Some(0),
                    ..Default::default()
                }]),
                build_id: Some(vec![0, 0, 0, 0]),
                ..Default::default()
            },
            // Describes a module that lives between addresses 1000 and 2000.
            ModuleMap {
                executable_segments: Some(vec![
                    ExecutableSegment {
                        start_address: Some(1000),
                        size: Some(500),
                        relative_address: Some(0),
                        ..Default::default()
                    },
                    ExecutableSegment {
                        start_address: Some(1500),
                        size: Some(500),
                        relative_address: Some(500),
                        ..Default::default()
                    },
                ]),
                build_id: Some(vec![1, 0, 0, 0]),
                ..Default::default()
            },
        ];

        let live_allocations =
            vec![LiveAllocation { size: 10, scale_factor: 2.5, stack_trace: Default::default() }];
        let mut dead_allocations = HashMap::new();
        dead_allocations
            .insert(Default::default(), DeadAllocationCounter { total_size: 20.8, count: 5.4 });
        let mut deallocations = HashMap::new();
        deallocations
            .insert(Default::default(), DeallocationCounter { total_size: 20.2, count: 12.6 });

        let profile = build_profile(
            modules.iter(),
            live_allocations.iter(),
            dead_allocations.clone(),
            deallocations.clone(),
            &Default::default(),
            false,
        );

        // Check that the profile contains one mapping per segment.
        let segment_count: usize = modules
            .iter()
            .map(|ModuleMap { executable_segments, .. }| {
                executable_segments.as_ref().unwrap().len()
            })
            .sum();
        assert_eq!(segment_count, profile.mapping.len(), "Unexpected number of mappings.");

        // Check that the profile contains one sample per allocation
        // and one sample per deallocation. There is one allocation
        // per `live_allocations` and per `dead_allocations`, and one
        // deallocation per `dead_allocations`.
        let allocation_count =
            live_allocations.len() + dead_allocations.values().len() + deallocations.values().len();
        assert_eq!(allocation_count, profile.sample.len(), "Unexpected number of samples.");

        // Check that the number of sample value per sample matches
        // the sample_type.
        let sample_type_count = profile.sample_type.len();
        assert!(
            profile.sample.iter().all(|sample| sample.value.len() == sample_type_count),
            "Unexpected sample value count."
        );

        // Verify the scaled and rounded sample values.
        assert_eq!(profile.sample[0].value, vec![3, 25, 3, 25, 0, 0]);
        assert_eq!(profile.sample[1].value, vec![0, 0, 5, 21, 0, 0]);
        assert_eq!(profile.sample[2].value, vec![0, 0, 0, 0, 13, 20]);
    }

    #[fuchsia::test]
    fn test_build_profile_lossy_comment() {
        let profile = build_profile(
            [].iter(),
            [].iter(),
            HashMap::new(),
            HashMap::new(),
            &Default::default(),
            true,
        );
        assert_eq!(profile.comment.len(), 1);
        let comment_str = &profile.string_table[profile.comment[0] as usize];
        assert!(comment_str.contains("WARNING: Socket buffer saturated"));
    }

    #[fuchsia::test]
    fn test_merge_same_process_profiles() {
        let module_a = ModuleMap {
            executable_segments: Some(vec![ExecutableSegment {
                start_address: Some(1000),
                size: Some(500),
                relative_address: Some(0),
                ..Default::default()
            }]),
            build_id: Some(vec![1, 2, 3, 4]),
            ..Default::default()
        };
        let module_b = ModuleMap {
            executable_segments: Some(vec![ExecutableSegment {
                start_address: Some(2000),
                size: Some(500),
                relative_address: Some(0),
                ..Default::default()
            }]),
            build_id: Some(vec![5, 6, 7, 8]),
            ..Default::default()
        };

        let st_dead = Rc::new(vec![1100, 1200]);
        let st_dealloc = Rc::new(vec![1300]);
        let st_live_new = Rc::new(vec![2100]);

        let mut older_dead = HashMap::new();
        older_dead.insert(st_dead.clone(), DeadAllocationCounter { count: 2.0, total_size: 200.0 });
        let mut older_dealloc = HashMap::new();
        older_dealloc
            .insert(st_dealloc.clone(), DeallocationCounter { count: 2.0, total_size: 200.0 });
        let older_live =
            vec![LiveAllocation { size: 100, scale_factor: 1.0, stack_trace: st_dead.clone() }];
        let older_traces = HashSet::from([st_dead.clone(), st_dealloc.clone()]);
        let older = build_profile(
            std::slice::from_ref(&module_a).iter(),
            older_live.iter(),
            older_dead,
            older_dealloc,
            &older_traces,
            true,
        );

        let mut newer_dead = HashMap::new();
        newer_dead.insert(st_dead.clone(), DeadAllocationCounter { count: 3.0, total_size: 300.0 });
        let mut newer_dealloc = HashMap::new();
        newer_dealloc
            .insert(st_dealloc.clone(), DeallocationCounter { count: 1.0, total_size: 100.0 });
        let newer_live =
            vec![LiveAllocation { size: 500, scale_factor: 1.0, stack_trace: st_live_new.clone() }];
        let newer_traces =
            HashSet::from([st_dead.clone(), st_dealloc.clone(), st_live_new.clone()]);
        let newer = build_profile(
            [module_a, module_b].iter(),
            newer_live.iter(),
            newer_dead,
            newer_dealloc,
            &newer_traces,
            true,
        );

        let merged = merge_same_process_profiles(older, newer);

        // Duplicate mappings from `module_a` are deduplicated; `module_b` is added.
        assert_eq!(2, merged.mapping.len());
        // Lossy comments from both profiles are deduplicated.
        assert_eq!(1, merged.comment.len());
        // All 4 unique frame addresses are present and mapped to their segments.
        assert_eq!(4, merged.location.len());
        for loc in &merged.location {
            assert_ne!(0, loc.mapping_id, "Location {} should resolve to a mapping", loc.address);
        }

        // `older`'s live sample is dropped; `newer`'s live sample is kept, and
        // dead/deallocated counters are summed across both profiles.
        assert_eq!(3, merged.sample.len());
        let samples_by_trace: HashMap<Vec<u64>, Vec<i64>> =
            merged.sample.into_iter().map(|s| (s.location_id, s.value)).collect();
        assert_eq!(samples_by_trace.get(&vec![2100]), Some(&vec![1, 500, 1, 500, 0, 0]));
        assert_eq!(samples_by_trace.get(&vec![1100, 1200]), Some(&vec![0, 0, 5, 500, 0, 0]));
        assert_eq!(samples_by_trace.get(&vec![1300]), Some(&vec![0, 0, 0, 0, 3, 300]));
    }
}
