// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Result};
use ffx_config::EnvironmentContext;
use heapdump_snapshot::{ExecutableRegion, Snapshot};
use itertools::Itertools;
use prost::Message;
use std::collections::HashSet;
use std::collections::hash_map::{Entry, HashMap};
use std::io::Write;

// Tries to instantiate a symbolizer instance to resolve addresses in the given address space.
//
// Fails gracefully returning None if (at least) one of the provided ExecutableRegion entries
// does not have the necessary information. This can happen with old heapdump collector builds
// from before the `name` and `vaddr` fields were added to the FIDL table.
fn instantiate_symbolizer(
    context: &EnvironmentContext,
    executable_regions: &HashMap<u64, ExecutableRegion>,
) -> Result<ffx_symbolize::Symbolizer> {
    let mut symbolizer = ffx_symbolize::Symbolizer::with_context(context)?;
    let mut symbolizer_module_ids = HashMap::new();
    for (address, info) in executable_regions {
        let module_id = symbolizer_module_ids
            .entry(info.build_id.clone())
            .or_insert_with(|| symbolizer.add_module(&info.name, &info.build_id));
        symbolizer.add_mapping(
            *module_id,
            ffx_symbolize::MappingDetails {
                start_addr: *address,
                size: info.size,
                vaddr: info.vaddr.context("missing vaddr")?,
                flags: ffx_symbolize::MappingFlags::EXECUTE,
            },
        )?;
    }
    Ok(symbolizer)
}

fn build_profile(
    context: &EnvironmentContext,
    snapshot: &Snapshot,
    with_tags: bool,
    symbolize: bool,
) -> Result<pprof::Profile> {
    let mut st = pprof::StringTableBuilder::default();

    let mut pprof = pprof::Profile {
        sample_type: vec![
            pprof::ValueType { r#type: st.intern("objects"), unit: st.intern("count") },
            pprof::ValueType { r#type: st.intern("allocated"), unit: st.intern("bytes") },
        ],
        ..Default::default()
    };

    // Build the Mappings with all the executable regions listed in the snapshot and obtain an
    // object that resolves arbitrary program addresses into the corresponding module IDs.
    let module_map = {
        let mut builder = pprof::ModuleMapBuilder::default();
        for (address, info) in &snapshot.executable_regions {
            let limit = *address + info.size;
            let filename_string_index = st.intern(&info.name);
            let build_id_string_index = st.intern_build_id(&info.build_id);
            builder.add_mapping(
                *address..limit,
                info.file_offset,
                filename_string_index,
                build_id_string_index,
            )?;
        }
        let (mappings, resolver) = builder.build();
        pprof.mapping = mappings;
        resolver
    };

    // If symbolization was requested, populate its own view of the mappings too.
    let symbolizer = if symbolize {
        if let Ok(symbolizer) = instantiate_symbolizer(context, &snapshot.executable_regions) {
            Some(symbolizer)
        } else {
            eprintln!(
                "WARNING: Automatic symbolization could not be performed, likely due to an \
                 incompatible version of the Heapdump collector running on the device. Please run \
                 \"fx pprof ...\" manually on the generated file."
            );
            None
        }
    } else {
        None
    };

    // Helper function that interns data on a function.
    let mut interned_functions = HashMap::new();
    let mut intern_function = |function_name: &str, file_name: &str| -> u64 {
        let function_name = st.intern(function_name);
        let file_name = st.intern(file_name);
        let next_id = (interned_functions.len() + 1) as u64;
        *interned_functions.entry((function_name, file_name)).or_insert_with(|| {
            pprof.function.push(pprof::Function {
                id: next_id,
                name: function_name,
                filename: file_name,
                ..Default::default()
            });
            next_id
        })
    };

    // Helper function that translates an address into symbolized stack frames. While doing it, also
    // keep track of resolved and unresolved mappings.
    let mut resolved_mapping_ids = HashSet::new();
    let mut unresolved_mapping_ids = HashSet::new();
    let mut address_to_symbolized_lines =
        |mapping_id: u64, program_address: u64| -> Option<Vec<pprof::Line>> {
            if let Some(symbolizer) = &symbolizer {
                match symbolizer.resolve_addr(program_address) {
                    Ok(resolved_locations) => {
                        let mut result = Vec::new();
                        for resolved_location in resolved_locations {
                            let function_name = &resolved_location.function;
                            let (file_name, line) = match resolved_location.file_and_line.as_ref() {
                                Some((file_name, line)) => {
                                    (file_name.as_str(), i64::try_from(*line).unwrap())
                                }
                                None => ("", 0),
                            };
                            let function_id = intern_function(function_name, &file_name);
                            result.push(pprof::Line { function_id, line });
                        }

                        // We managed to symbolize an address from this mapping, which proves that
                        // it was resolved.
                        resolved_mapping_ids.insert(mapping_id);

                        return Some(result);
                    }
                    Err(ffx_symbolize::ResolveError::SymbolNotFound) => {
                        // Even if this specific address could not be symbolized, the mapping as a
                        // whole was resolved.
                        resolved_mapping_ids.insert(mapping_id);
                    }
                    Err(ffx_symbolize::ResolveError::NoOverlappingModule) => {
                        unreachable!("the address belongs to mapping {}", mapping_id)
                    }
                    Err(ffx_symbolize::ResolveError::SymbolFileUnavailable) => {
                        unresolved_mapping_ids.insert(mapping_id);
                    }
                }
            }

            None
        };

    // Fill the Locations with all the program addresses referenced in the snapshot and store their
    // assigned IDs.
    let mut address_to_location_id = HashMap::new();
    for info in &snapshot.allocations {
        for address in &info.stack_trace.program_addresses {
            let next_id = address_to_location_id.len() as u64 + 1;
            if let Entry::Vacant(e) = address_to_location_id.entry(*address) {
                e.insert(next_id);

                let mapping_id = module_map.resolve(*address);
                let symbolized_lines = if mapping_id != 0 {
                    address_to_symbolized_lines(mapping_id, *address)
                } else {
                    None
                };

                pprof.location.push(pprof::Location {
                    id: next_id,
                    mapping_id,
                    address: *address,
                    line: symbolized_lines.unwrap_or(vec![]),
                    ..Default::default()
                });
            }
        }
    }

    // Mark resolved mappings.
    for mapping in &mut pprof.mapping {
        if resolved_mapping_ids.contains(&mapping.id) {
            mapping.has_functions = true;
            mapping.has_filenames = true;
            mapping.has_line_numbers = true;
            mapping.has_inline_frames = true;
        }
    }

    // Helper function that translates program addresses to location IDs.
    let addresses_to_location_ids = |program_addresses: &[u64]| -> Vec<u64> {
        program_addresses.iter().map(|addr| address_to_location_id[addr]).collect()
    };

    // Fill the Samples.
    if with_tags {
        for info in &snapshot.allocations {
            // Cast into a pprof-friendly type.
            let size = info.size as i64;

            let location_ids = addresses_to_location_ids(&info.stack_trace.program_addresses);
            let mut label = vec![];
            if let Some(address) = info.address {
                label.push(pprof::Label {
                    key: st.intern("address"),
                    str: st.intern(&format!("0x{:x}", address)),
                    ..Default::default()
                })
            }
            label.push(pprof::Label {
                key: st.intern("bytes"),
                num: size,
                num_unit: st.intern("bytes"),
                ..Default::default()
            });
            if let Some(timestamp) = &info.timestamp {
                label.push(pprof::Label {
                    key: st.intern("timestamp"),
                    num: timestamp.into_nanos(),
                    num_unit: st.intern("nanoseconds"),
                    ..Default::default()
                });
            }
            if let Some(thread_info) = &info.thread_info {
                label.push(pprof::Label {
                    key: st.intern("thread"),
                    str: st.intern(&format!("{}[{}]", thread_info.name, thread_info.koid)),
                    ..Default::default()
                });
            }

            pprof.sample.push(pprof::Sample {
                location_id: location_ids,
                value: vec![info.count as i64, size],
                label,
                ..Default::default()
            });
        }
    } else {
        // Group allocations with the same stack trace (to make the resulting profile smaller).
        let grouped_allocations =
            snapshot.allocations.iter().into_group_map_by(|allocation_info| {
                addresses_to_location_ids(&allocation_info.stack_trace.program_addresses)
            });

        for (location_ids, allocations_info) in grouped_allocations {
            // Compute totals and cast into pprof-friendly types.
            let size = allocations_info.iter().map(|alloc| alloc.size).sum::<u64>() as i64;
            let count = allocations_info.iter().map(|alloc| alloc.count).sum::<u64>() as i64;

            pprof.sample.push(pprof::Sample {
                location_id: location_ids,
                value: vec![count, size],
                ..Default::default()
            });
        }
    }

    pprof.string_table = st.build();

    // Print warnings about unresolved mappings.
    for mapping in &pprof.mapping {
        if unresolved_mapping_ids.contains(&mapping.id) {
            let build_id = &pprof.string_table[mapping.build_id as usize];
            eprintln!("WARNING: Unresolved build ID \"{build_id}\".");
        }
    }

    Ok(pprof)
}

pub fn export_to_pprof(
    context: &EnvironmentContext,
    snapshot: &Snapshot,
    dest: &mut std::fs::File,
    with_tags: bool,
    symbolize: bool,
) -> Result<()> {
    let buf = build_profile(context, snapshot, with_tags, symbolize)?.encode_to_vec();
    dest.write_all(&buf)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use heapdump_snapshot::{Allocation, ExecutableRegion, StackTrace, ThreadInfo};
    use itertools::MinMaxResult::MinMax;
    use maplit::hashmap;
    use std::io::{Read, Seek};
    use std::rc::Rc;
    use test_case::test_case;

    // Placeholder mappings for the fake profile:
    const MAP_1_ADDRESS: u64 = 0x2000;
    const MAP_1_NAME: &str = "map-1";
    const MAP_1_SIZE: u64 = 0x1000;
    const MAP_1_FILE_OFFSET: u64 = 0x1000;
    const MAP_1_VADDR: u64 = 0x2000;
    const MAP_1_BUILD_ID: &str = "112233441122334411223344";
    const MAP_2_ADDRESS: u64 = 0x8000;
    const MAP_2_NAME: &str = "map-2";
    const MAP_2_SIZE: u64 = 0x2000;
    const MAP_2_FILE_OFFSET: u64 = 0;
    const MAP_2_VADDR: u64 = 0x3000;
    const MAP_2_BUILD_ID: &str = "556677885566778855667788";

    // Placeholder code locations (program addresses) for the fake profile:
    const LOC_1_ADDRESS: u64 = 0x2500; // within mapping 1
    const LOC_2_ADDRESS: u64 = 0x8900; // within mapping 2
    const LOC_3_ADDRESS: u64 = 0x9100; // within mapping 2
    const LOC_4_ADDRESS: u64 = 0; // outside of any known mapping
    const LOC_5_ADDRESS: u64 = 0x5123; // outside of any known mapping

    // Placeholder stack traces for the fake profile:
    const STACK_TRACE_A: &[u64] = &[LOC_1_ADDRESS, LOC_2_ADDRESS, LOC_3_ADDRESS];
    const STACK_TRACE_B: &[u64] = &[LOC_1_ADDRESS, LOC_4_ADDRESS, LOC_5_ADDRESS];

    // Placeholder allocations for the fake profile:
    const ALLOC_1_ADDRESS: u64 = 0x611000;
    const ALLOC_1_SIZE: i64 = 0x1800;
    const ALLOC_1_COUNT: u64 = 1;
    const ALLOC_1_THREAD_KOID: u64 = 1234;
    const ALLOC_1_THREAD_NAME: &str = "thread-1";
    const ALLOC_1_TIMESTAMP: fidl::MonotonicInstant =
        fidl::MonotonicInstant::from_nanos(8777777777778);
    const ALLOC_2_ADDRESS: u64 = 0x624000;
    const ALLOC_2_SIZE: i64 = 0x30;
    const ALLOC_2_COUNT: u64 = 1;
    const ALLOC_2_THREAD_KOID: u64 = 5678;
    const ALLOC_2_THREAD_NAME: &str = "thread-2";
    const ALLOC_2_TIMESTAMP: fidl::MonotonicInstant =
        fidl::MonotonicInstant::from_nanos(9333333333333);
    const ALLOC_3_SIZE: i64 = 0xC000;
    const ALLOC_3_COUNT: u64 = 3;
    const ALLOC_3_THREAD_KOID: u64 = 9999;
    const ALLOC_3_THREAD_NAME: &str = "thread-3";
    const ALLOC_3_TIMESTAMP: fidl::MonotonicInstant =
        fidl::MonotonicInstant::from_nanos(9876543211111);

    fn generate_fake_snapshot() -> Snapshot {
        let stack_trace_a = Rc::new(StackTrace { program_addresses: STACK_TRACE_A.to_vec() });
        let stack_trace_b = Rc::new(StackTrace { program_addresses: STACK_TRACE_B.to_vec() });

        Snapshot {
            allocations: vec![
                Allocation {
                    address: Some(ALLOC_1_ADDRESS),
                    size: ALLOC_1_SIZE.try_into().unwrap(),
                    count: ALLOC_1_COUNT,
                    thread_info: Some(Rc::new(ThreadInfo {
                        koid: ALLOC_1_THREAD_KOID,
                        name: ALLOC_1_THREAD_NAME.to_string(),
                    })),
                    stack_trace: stack_trace_a.clone(),
                    timestamp: Some(ALLOC_1_TIMESTAMP),
                    contents: None,
                },
                Allocation {
                    address: Some(ALLOC_2_ADDRESS),
                    size: ALLOC_2_SIZE.try_into().unwrap(),
                    count: ALLOC_2_COUNT,
                    thread_info: Some(Rc::new(ThreadInfo {
                        koid: ALLOC_2_THREAD_KOID,
                        name: ALLOC_2_THREAD_NAME.to_string(),
                    })),
                    stack_trace: stack_trace_b.clone(),
                    timestamp: Some(ALLOC_2_TIMESTAMP),
                    contents: None,
                },
                Allocation {
                    address: None,
                    size: ALLOC_3_SIZE.try_into().unwrap(),
                    count: ALLOC_3_COUNT,
                    thread_info: Some(Rc::new(ThreadInfo {
                        koid: ALLOC_3_THREAD_KOID,
                        name: ALLOC_3_THREAD_NAME.to_string(),
                    })),
                    stack_trace: stack_trace_b.clone(),
                    timestamp: Some(ALLOC_3_TIMESTAMP),
                    contents: None,
                },
            ],
            executable_regions: hashmap![
                MAP_1_ADDRESS => ExecutableRegion {
                    name: MAP_1_NAME.to_string(),
                    size: MAP_1_SIZE,
                    file_offset: MAP_1_FILE_OFFSET,
                    vaddr: Some(MAP_1_VADDR),
                    build_id: hex::decode(MAP_1_BUILD_ID).unwrap(),
                },
                MAP_2_ADDRESS => ExecutableRegion {
                    name: MAP_2_NAME.to_string(),
                    size: MAP_2_SIZE,
                    file_offset: MAP_2_FILE_OFFSET,
                    vaddr: Some(MAP_2_VADDR),
                    build_id: hex::decode(MAP_2_BUILD_ID).unwrap(),
                },
            ],
        }
    }

    struct ProfileHelper<'a> {
        profile: &'a pprof::Profile,
        address_idx: Option<i64>,
        bytes_idx: Option<i64>,
        timestamp_idx: Option<i64>,
        thread_idx: Option<i64>,
    }
    impl<'a> ProfileHelper<'a> {
        fn new(profile: &'a pprof::Profile) -> ProfileHelper<'a> {
            let lookup = |text: &str| {
                (&profile.string_table).iter().position(|s| s == text).map(|i| i as i64)
            };
            ProfileHelper {
                profile,
                address_idx: lookup("address"),
                bytes_idx: lookup("bytes"),
                timestamp_idx: lookup("timestamp"),
                thread_idx: lookup("thread"),
            }
        }
        fn address(&self, sample: &'a pprof::Sample) -> Option<&'a String> {
            sample
                .label
                .iter()
                .find(|l| l.key == self.address_idx.unwrap())
                .map(|l| &self.profile.string_table[usize::try_from(l.str).unwrap()])
        }
        fn bytes(&self, sample: &'a pprof::Sample) -> Option<i64> {
            self.bytes_idx.and_then(|bytes_idx| {
                sample.label.iter().find(|l| l.key == bytes_idx).map(|l| l.num)
            })
        }
        fn timestamp(&self, sample: &'a pprof::Sample) -> Option<i64> {
            self.timestamp_idx.and_then(|timestamp_idx| {
                sample.label.iter().find(|l| l.key == timestamp_idx).map(|l| l.num)
            })
        }
        fn thread(&self, sample: &'a pprof::Sample) -> Option<&'a String> {
            self.thread_idx.and_then(|thread_idx| {
                sample
                    .label
                    .iter()
                    .find(|l| l.key == thread_idx)
                    .map(|l| &self.profile.string_table[usize::try_from(l.str).unwrap()])
            })
        }
    }

    fn assert_profile_matches_fake_snapshot(profile: &pprof::Profile, with_tags: bool) {
        // Helper function to access the string table.
        let st = |index: i64| profile.string_table[usize::try_from(index).unwrap()].as_str();
        let h = ProfileHelper::new(profile);

        // Helper function to resolce location IDs.
        let loc = |location_id: u64| profile.location.iter().find(|e| e.id == location_id).unwrap();

        // Verify the string table.
        assert_eq!(st(0), "", "The first entry in the string table should always be empty");

        // Verify that samples' data format.
        assert_eq!(profile.sample_type.len(), 2);
        assert_eq!(st(profile.sample_type[0].r#type), "objects");
        assert_eq!(st(profile.sample_type[0].unit), "count");
        assert_eq!(st(profile.sample_type[1].r#type), "allocated");
        assert_eq!(st(profile.sample_type[1].unit), "bytes");

        if with_tags {
            // Verify the tags' labels.
            for sample in &profile.sample {
                for label in &sample.label {
                    match st(label.key) {
                        "address" => {}
                        "bytes" => assert_eq!(st(label.num_unit), "bytes"),
                        "timestamp" => assert_eq!(st(label.num_unit), "nanoseconds"),
                        "thread" => {}
                        _ => unreachable!(),
                    }
                }
            }

            // Identify the three allocations from their sizes (which are unique in our sample snapshot)
            // and verify them.
            assert_eq!(profile.sample.len(), 3);
            let allocation1 =
                profile.sample.iter().find(|s| h.bytes(s) == Some(ALLOC_1_SIZE)).unwrap();
            assert_eq!(allocation1.value[0], ALLOC_1_COUNT as i64);
            assert_eq!(allocation1.value[1], ALLOC_1_SIZE);
            assert_eq!(h.address(allocation1), Some(&format!("0x{:x}", ALLOC_1_ADDRESS)));
            assert_eq!(allocation1.location_id.len(), STACK_TRACE_A.len());
            assert_eq!(loc(allocation1.location_id[0]).address, STACK_TRACE_A[0]);
            assert_eq!(loc(allocation1.location_id[1]).address, STACK_TRACE_A[1]);
            assert_eq!(loc(allocation1.location_id[2]).address, STACK_TRACE_A[2]);
            assert_eq!(h.timestamp(allocation1), Some(ALLOC_1_TIMESTAMP.into_nanos()));
            assert_eq!(
                h.thread(allocation1),
                Some(&format!("{}[{}]", ALLOC_1_THREAD_NAME, ALLOC_1_THREAD_KOID))
            );
            let allocation2 =
                profile.sample.iter().find(|s| h.bytes(s) == Some(ALLOC_2_SIZE)).unwrap();
            assert_eq!(allocation2.value[0], ALLOC_2_COUNT as i64);
            assert_eq!(allocation2.value[1], ALLOC_2_SIZE);
            assert_eq!(h.address(allocation2), Some(&format!("0x{:x}", ALLOC_2_ADDRESS)));
            assert_eq!(allocation2.location_id.len(), STACK_TRACE_B.len());
            assert_eq!(loc(allocation2.location_id[0]).address, STACK_TRACE_B[0]);
            assert_eq!(loc(allocation2.location_id[1]).address, STACK_TRACE_B[1]);
            assert_eq!(h.timestamp(allocation2), Some(ALLOC_2_TIMESTAMP.into_nanos()));
            assert_eq!(
                h.thread(allocation2),
                Some(&format!("{}[{}]", ALLOC_2_THREAD_NAME, ALLOC_2_THREAD_KOID))
            );
            let allocation3 =
                profile.sample.iter().find(|s| h.bytes(s) == Some(ALLOC_3_SIZE)).unwrap();
            assert_eq!(allocation3.value[0], ALLOC_3_COUNT as i64);
            assert_eq!(allocation3.value[1], ALLOC_3_SIZE);
            assert_eq!(h.address(allocation3), None);
            assert_eq!(allocation3.location_id.len(), STACK_TRACE_B.len());
            assert_eq!(loc(allocation3.location_id[0]).address, STACK_TRACE_B[0]);
            assert_eq!(loc(allocation3.location_id[1]).address, STACK_TRACE_B[1]);
            assert_eq!(h.timestamp(allocation3), Some(ALLOC_3_TIMESTAMP.into_nanos()));
            assert_eq!(
                h.thread(allocation3),
                Some(&format!("{}[{}]", ALLOC_3_THREAD_NAME, ALLOC_3_THREAD_KOID))
            );
        } else {
            // Verify that the samples were aggregated by stack trace correctly.
            assert_eq!(profile.sample.len(), 2);
            let MinMax(group1, group2) = profile.sample.iter().minmax_by_key(|e| e.value[0]) else {
                unreachable!();
            };
            assert_eq!(group1.value[0], ALLOC_1_COUNT as i64);
            assert_eq!(group1.value[1], ALLOC_1_SIZE);
            assert_eq!(group1.location_id.len(), STACK_TRACE_A.len());
            assert_eq!(loc(group1.location_id[0]).address, STACK_TRACE_A[0]);
            assert_eq!(loc(group1.location_id[1]).address, STACK_TRACE_A[1]);
            assert_eq!(loc(group1.location_id[2]).address, STACK_TRACE_A[2]);
            assert_eq!(group2.value[0], (ALLOC_2_COUNT + ALLOC_3_COUNT) as i64);
            assert_eq!(group2.value[1], ALLOC_2_SIZE + ALLOC_3_SIZE);
            assert_eq!(group2.location_id.len(), STACK_TRACE_B.len());
            assert_eq!(loc(group2.location_id[0]).address, STACK_TRACE_B[0]);
            assert_eq!(loc(group2.location_id[1]).address, STACK_TRACE_B[1]);
        }

        // Identify the mappings from their addresses and verify them.
        assert_eq!(profile.mapping.len(), 2);
        assert_eq!(profile.mapping.iter().filter(|m| m.id == 0).next(), None, "ID 0 is reserved");
        let mapping1 = profile.mapping.iter().find(|m| m.memory_start == MAP_1_ADDRESS).unwrap();
        assert_eq!(mapping1.memory_limit, MAP_1_ADDRESS + MAP_1_SIZE);
        assert_eq!(mapping1.file_offset, MAP_1_FILE_OFFSET);
        assert_eq!(st(mapping1.filename), MAP_1_NAME);
        assert_eq!(st(mapping1.build_id), MAP_1_BUILD_ID);
        let mapping2 = profile.mapping.iter().find(|m| m.memory_start == MAP_2_ADDRESS).unwrap();
        assert_eq!(mapping2.memory_limit, MAP_2_ADDRESS + MAP_2_SIZE);
        assert_eq!(mapping2.file_offset, MAP_2_FILE_OFFSET);
        assert_eq!(st(mapping2.filename), MAP_2_NAME);
        assert_eq!(st(mapping2.build_id), MAP_2_BUILD_ID);

        // Identify the locations from their addresses and verify them.
        assert_eq!(profile.location.len(), 5);
        assert_eq!(profile.location.iter().filter(|l| l.id == 0).next(), None, "ID 0 is reserved");
        let loc1 = profile.location.iter().find(|l| l.address == LOC_1_ADDRESS).unwrap();
        assert_eq!(loc1.mapping_id, mapping1.id, "LOC_1_ADDRESS belongs to mapping 1");
        let loc2 = profile.location.iter().find(|l| l.address == LOC_2_ADDRESS).unwrap();
        assert_eq!(loc2.mapping_id, mapping2.id, "LOC_2_ADDRESS belongs to mapping 2");
        let loc3 = profile.location.iter().find(|l| l.address == LOC_3_ADDRESS).unwrap();
        assert_eq!(loc3.mapping_id, mapping2.id, "LOC_3_ADDRESS belongs to mapping 2");
        let loc4 = profile.location.iter().find(|l| l.address == LOC_4_ADDRESS).unwrap();
        assert_eq!(loc4.mapping_id, 0, "LOC_4_ADDRESS does not belong to any mapping");
        let loc5 = profile.location.iter().find(|l| l.address == LOC_5_ADDRESS).unwrap();
        assert_eq!(loc5.mapping_id, 0, "LOC_5_ADDRESS does not belong to any mapping");
    }

    /// Verifies that the protobuf message generated by `build_profile` contains correct data.
    #[test_case(true ; "with tags")]
    #[test_case(false ; "aggregated")]
    fn test_build_profile(with_tags: bool) {
        let env = ffx_config::test_env().build().expect("Test Env Init");
        let snapshot = generate_fake_snapshot();
        let profile = build_profile(&env.context, &snapshot, with_tags, false).unwrap();
        assert_profile_matches_fake_snapshot(&profile, with_tags);
    }

    /// Verifies that the file written by `export_to_pprof` can be read back.
    #[test_case(true ; "with tags")]
    #[test_case(false ; "aggregated")]
    fn test_export_to_pprof(with_tags: bool) {
        let env = ffx_config::test_env().build().expect("Test Env Init");
        // Create a temporary file.
        let mut tempfile = tempfile::tempfile().unwrap();

        // Write a snapshot to it.
        let snapshot = generate_fake_snapshot();
        export_to_pprof(&env.context, &snapshot, &mut tempfile, with_tags, false).unwrap();

        // Read it back.
        let mut buf = Vec::new();
        tempfile.rewind().unwrap();
        tempfile.read_to_end(&mut buf).unwrap();

        // Verify that it can be decoded correctly.
        let profile = pprof::Profile::decode(&buf[..]).unwrap();
        assert_profile_matches_fake_snapshot(&profile, with_tags);
    }

    fn generate_fake_aggregated_snapshot() -> Snapshot {
        let stack_trace_a = Rc::new(StackTrace { program_addresses: STACK_TRACE_A.to_vec() });
        let stack_trace_b = Rc::new(StackTrace { program_addresses: STACK_TRACE_B.to_vec() });

        Snapshot {
            allocations: vec![
                Allocation {
                    address: None,
                    size: ALLOC_1_SIZE.try_into().unwrap(),
                    count: ALLOC_1_COUNT,
                    thread_info: None,
                    stack_trace: stack_trace_a.clone(),
                    timestamp: None,
                    contents: None,
                },
                Allocation {
                    address: None,
                    size: ALLOC_2_SIZE.try_into().unwrap(),
                    count: ALLOC_2_COUNT,
                    thread_info: None,
                    stack_trace: stack_trace_b.clone(),
                    timestamp: None,
                    contents: None,
                },
                Allocation {
                    address: None,
                    size: ALLOC_3_SIZE.try_into().unwrap(),
                    count: ALLOC_3_COUNT,
                    thread_info: None,
                    stack_trace: stack_trace_b.clone(),
                    timestamp: None,
                    contents: None,
                },
            ],
            executable_regions: hashmap![
                MAP_1_ADDRESS => ExecutableRegion {
                    name: MAP_1_NAME.to_string(),
                    size: MAP_1_SIZE,
                    file_offset: MAP_1_FILE_OFFSET,
                    vaddr: Some(MAP_1_VADDR),
                    build_id: hex::decode(MAP_1_BUILD_ID).unwrap(),
                },
                MAP_2_ADDRESS => ExecutableRegion {
                    name: MAP_2_NAME.to_string(),
                    size: MAP_2_SIZE,
                    file_offset: MAP_2_FILE_OFFSET,
                    vaddr: Some(MAP_2_VADDR),
                    build_id: hex::decode(MAP_2_BUILD_ID).unwrap(),
                },
            ],
        }
    }

    fn assert_profile_matches_fake_aggregated_snapshot(profile: &pprof::Profile, with_tags: bool) {
        // Helper function to access the string table.
        let st = |index: i64| profile.string_table[usize::try_from(index).unwrap()].as_str();

        // Helper function to resolce location IDs.
        let loc = |location_id: u64| profile.location.iter().find(|e| e.id == location_id).unwrap();

        // Verify the string table.
        assert_eq!(st(0), "", "The first entry in the string table should always be empty");

        // Verify that samples' data format.
        assert_eq!(profile.sample_type.len(), 2);
        assert_eq!(st(profile.sample_type[0].r#type), "objects");
        assert_eq!(st(profile.sample_type[0].unit), "count");
        assert_eq!(st(profile.sample_type[1].r#type), "allocated");
        assert_eq!(st(profile.sample_type[1].unit), "bytes");

        if with_tags {
            // Verify the tags' labels.

            for sample in &profile.sample {
                assert_eq!(sample.value.len(), profile.sample_type.len());
                assert_eq!(sample.label.len(), 1);
                assert_eq!(st(sample.label[0].key), "bytes");
                assert_eq!(st(sample.label[0].num_unit), "bytes");
            }

            // Identify the three allocations from their sizes (which are unique in our sample snapshot)
            // and verify them.
            assert_eq!(profile.sample.len(), 3);
            let allocation1 =
                profile.sample.iter().find(|s| s.label[0].num == ALLOC_1_SIZE).unwrap();
            assert_eq!(allocation1.value[0], ALLOC_1_COUNT as i64);
            assert_eq!(allocation1.value[1], ALLOC_1_SIZE);
            assert_eq!(allocation1.location_id.len(), STACK_TRACE_A.len());
            assert_eq!(loc(allocation1.location_id[0]).address, STACK_TRACE_A[0]);
            assert_eq!(loc(allocation1.location_id[1]).address, STACK_TRACE_A[1]);
            assert_eq!(loc(allocation1.location_id[2]).address, STACK_TRACE_A[2]);

            let allocation2 =
                profile.sample.iter().find(|s| s.label[0].num == ALLOC_2_SIZE).unwrap();
            assert_eq!(allocation2.value[0], ALLOC_2_COUNT as i64);
            assert_eq!(allocation2.value[1], ALLOC_2_SIZE);
            assert_eq!(allocation2.location_id.len(), STACK_TRACE_B.len());
            assert_eq!(loc(allocation2.location_id[0]).address, STACK_TRACE_B[0]);
            assert_eq!(loc(allocation2.location_id[1]).address, STACK_TRACE_B[1]);

            let allocation3 =
                profile.sample.iter().find(|s| s.label[0].num == ALLOC_3_SIZE).unwrap();
            assert_eq!(allocation3.value[0], ALLOC_3_COUNT as i64);
            assert_eq!(allocation3.value[1], ALLOC_3_SIZE);
            assert_eq!(allocation3.location_id.len(), STACK_TRACE_B.len());
            assert_eq!(loc(allocation3.location_id[0]).address, STACK_TRACE_B[0]);
            assert_eq!(loc(allocation3.location_id[1]).address, STACK_TRACE_B[1]);
        } else {
            // Verify that the samples were aggregated by stack trace correctly.
            assert_eq!(profile.sample.len(), 2);
            let MinMax(group1, group2) = profile.sample.iter().minmax_by_key(|e| e.value[0]) else {
                unreachable!();
            };
            assert_eq!(group1.value[0], ALLOC_1_COUNT as i64);
            assert_eq!(group1.value[1], ALLOC_1_SIZE);
            assert_eq!(group1.location_id.len(), STACK_TRACE_A.len());
            assert_eq!(loc(group1.location_id[0]).address, STACK_TRACE_A[0]);
            assert_eq!(loc(group1.location_id[1]).address, STACK_TRACE_A[1]);
            assert_eq!(loc(group1.location_id[2]).address, STACK_TRACE_A[2]);
            assert_eq!(group2.value[0], (ALLOC_2_COUNT + ALLOC_3_COUNT) as i64);
            assert_eq!(group2.value[1], ALLOC_2_SIZE + ALLOC_3_SIZE);
            assert_eq!(group2.location_id.len(), STACK_TRACE_B.len());
            assert_eq!(loc(group2.location_id[0]).address, STACK_TRACE_B[0]);
            assert_eq!(loc(group2.location_id[1]).address, STACK_TRACE_B[1]);
        }

        // Identify the mappings from their addresses and verify them.
        assert_eq!(profile.mapping.len(), 2);
        assert_eq!(profile.mapping.iter().filter(|m| m.id == 0).next(), None, "ID 0 is reserved");
        let mapping1 = profile.mapping.iter().find(|m| m.memory_start == MAP_1_ADDRESS).unwrap();
        assert_eq!(mapping1.memory_limit, MAP_1_ADDRESS + MAP_1_SIZE);
        assert_eq!(mapping1.file_offset, MAP_1_FILE_OFFSET);
        assert_eq!(st(mapping1.filename), MAP_1_NAME);
        assert_eq!(st(mapping1.build_id), MAP_1_BUILD_ID);
        let mapping2 = profile.mapping.iter().find(|m| m.memory_start == MAP_2_ADDRESS).unwrap();
        assert_eq!(mapping2.memory_limit, MAP_2_ADDRESS + MAP_2_SIZE);
        assert_eq!(mapping2.file_offset, MAP_2_FILE_OFFSET);
        assert_eq!(st(mapping2.filename), MAP_2_NAME);
        assert_eq!(st(mapping2.build_id), MAP_2_BUILD_ID);

        // Identify the locations from their addresses and verify them.
        assert_eq!(profile.location.len(), 5);
        assert_eq!(profile.location.iter().filter(|l| l.id == 0).next(), None, "ID 0 is reserved");
        let loc1 = profile.location.iter().find(|l| l.address == LOC_1_ADDRESS).unwrap();
        assert_eq!(loc1.mapping_id, mapping1.id, "LOC_1_ADDRESS belongs to mapping 1");
        let loc2 = profile.location.iter().find(|l| l.address == LOC_2_ADDRESS).unwrap();
        assert_eq!(loc2.mapping_id, mapping2.id, "LOC_2_ADDRESS belongs to mapping 2");
        let loc3 = profile.location.iter().find(|l| l.address == LOC_3_ADDRESS).unwrap();
        assert_eq!(loc3.mapping_id, mapping2.id, "LOC_3_ADDRESS belongs to mapping 2");
        let loc4 = profile.location.iter().find(|l| l.address == LOC_4_ADDRESS).unwrap();
        assert_eq!(loc4.mapping_id, 0, "LOC_4_ADDRESS does not belong to any mapping");
        let loc5 = profile.location.iter().find(|l| l.address == LOC_5_ADDRESS).unwrap();
        assert_eq!(loc5.mapping_id, 0, "LOC_5_ADDRESS does not belong to any mapping");
    }

    /// Verifies that the protobuf message generated by `build_profile` contains correct data.
    #[test_case(true ; "with tags")]
    #[test_case(false ; "aggregated")]
    fn test_build_profile_from_aggregated_snapshot(with_tags: bool) {
        let env = ffx_config::test_env().build().expect("Test Env Init");
        let snapshot = generate_fake_aggregated_snapshot();
        let profile = build_profile(&env.context, &snapshot, with_tags, false).unwrap();
        assert_profile_matches_fake_aggregated_snapshot(&profile, with_tags);
    }
}
