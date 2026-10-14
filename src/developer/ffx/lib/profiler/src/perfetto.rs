// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::parse::{ModuleWithMmapDetails, Pid, Tid};
use crate::symbolize::{ResolvedAddress, SymbolizedRecords};
use perfetto_trace_rust_proto::perfetto::protos::{
    Callstack, Frame, InternedData, InternedString, Mapping, PerfSample, ProcessTree, Trace,
    TracePacket, process_tree, profiling, trace_packet,
};
use prost::Message;
use std::collections::{HashMap, HashSet};

/// Fixed, distinctive sequence ID ("FXTP" in ASCII) for `ffx profiler` Perfetto packets.
/// Chosen in a high range to avoid colliding with `trace_manager` or `perfetto-bridge`
/// packet sequence IDs when concatenated into an `ffx trace` `.fxt` file.
const PERFETTO_SEQUENCE_ID: u32 = 0x4658_5450;

const UNMAPPED_PATH: &str = "[unmapped]";

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum MappingKey {
    Module { pid: u64, mod_idx: usize },
    Unmapped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct FrameKey {
    mapping_iid: u64,
    rel_pc: u64,
    function_name_iid: u64,
    source_path_iid: u64,
    line_number: u32,
}

#[derive(Default)]
struct SequenceInterner {
    build_ids: HashMap<Vec<u8>, u64>,
    mapping_paths: HashMap<Vec<u8>, u64>,
    function_names: HashMap<Vec<u8>, u64>,
    source_paths: HashMap<Vec<u8>, u64>,
    mappings: HashMap<MappingKey, u64>,
    frames: HashMap<FrameKey, u64>,
    callstacks: HashMap<Vec<u64>, u64>,
}

impl SequenceInterner {
    fn intern_build_id(&mut self, bytes: &[u8], pending: &mut InternedData) -> u64 {
        if let Some(&iid) = self.build_ids.get(bytes) {
            return iid;
        }
        let iid = (self.build_ids.len() + 1) as u64;
        let owned = bytes.to_vec();
        self.build_ids.insert(owned.clone(), iid);
        pending.build_ids.push(InternedString { iid: Some(iid), str: Some(owned) });
        iid
    }

    fn intern_mapping_path(&mut self, path: &[u8], pending: &mut InternedData) -> u64 {
        if let Some(&iid) = self.mapping_paths.get(path) {
            return iid;
        }
        let iid = (self.mapping_paths.len() + 1) as u64;
        let owned = path.to_vec();
        self.mapping_paths.insert(owned.clone(), iid);
        pending.mapping_paths.push(InternedString { iid: Some(iid), str: Some(owned) });
        iid
    }

    fn intern_function_name(&mut self, name: &[u8], pending: &mut InternedData) -> u64 {
        if let Some(&iid) = self.function_names.get(name) {
            return iid;
        }
        let iid = (self.function_names.len() + 1) as u64;
        let owned = name.to_vec();
        self.function_names.insert(owned.clone(), iid);
        pending.function_names.push(InternedString { iid: Some(iid), str: Some(owned) });
        iid
    }

    fn intern_source_path(&mut self, path: &[u8], pending: &mut InternedData) -> u64 {
        if let Some(&iid) = self.source_paths.get(path) {
            return iid;
        }
        let iid = (self.source_paths.len() + 1) as u64;
        let owned = path.to_vec();
        self.source_paths.insert(owned.clone(), iid);
        pending.source_paths.push(InternedString { iid: Some(iid), str: Some(owned) });
        iid
    }

    fn intern_module_mapping(
        &mut self,
        pid: u64,
        mod_idx: usize,
        details: &ModuleWithMmapDetails,
        pending: &mut InternedData,
    ) -> u64 {
        let key = MappingKey::Module { pid, mod_idx };
        if let Some(&iid) = self.mappings.get(&key) {
            return iid;
        }
        let path_name = if details.module.name.is_empty() {
            UNMAPPED_PATH.as_bytes()
        } else {
            details.module.name.as_bytes()
        };
        let path_iid = self.intern_mapping_path(path_name, pending);
        let build_id_iid = if !details.module.build_id.is_empty() {
            match hex::decode(&details.module.build_id) {
                Ok(raw) if !raw.is_empty() => Some(self.intern_build_id(&raw, pending)),
                _ => None,
            }
        } else {
            None
        };
        let first_mmap = details.mmaps.iter().min_by_key(|m| m.start_addr);
        let start = first_mmap.map_or(0, |m| m.start_addr);
        // Perfetto's `Mapping.load_bias` is `p_vaddr - p_offset` (`vaddr` of the first segment
        // when `exact_offset` is 0), so `VirtualMemoryMapping::ToAddress(rel_pc)` computes
        // `rel_pc + start - exact_offset - load_bias` to reconstruct the original `pc`.
        let load_bias = first_mmap.map_or(0, |m| m.vaddr);
        let end =
            details.mmaps.iter().map(|m| m.start_addr.saturating_add(m.size)).max().unwrap_or(0);

        let iid = (self.mappings.len() + 1) as u64;
        self.mappings.insert(key, iid);
        pending.mappings.push(Mapping {
            iid: Some(iid),
            build_id: build_id_iid,
            exact_offset: None,
            start_offset: Some(0),
            start: Some(start),
            end: Some(end),
            load_bias: Some(load_bias),
            path_string_ids: vec![path_iid],
        });
        iid
    }

    fn intern_unmapped_mapping(&mut self, pending: &mut InternedData) -> u64 {
        if let Some(&iid) = self.mappings.get(&MappingKey::Unmapped) {
            return iid;
        }
        let path_iid = self.intern_mapping_path(UNMAPPED_PATH.as_bytes(), pending);
        let iid = (self.mappings.len() + 1) as u64;
        self.mappings.insert(MappingKey::Unmapped, iid);
        pending.mappings.push(Mapping {
            iid: Some(iid),
            build_id: None,
            exact_offset: None,
            start_offset: Some(0),
            start: Some(0),
            end: Some(0),
            load_bias: None,
            path_string_ids: vec![path_iid],
        });
        iid
    }

    fn intern_frame(
        &mut self,
        mapping_iid: u64,
        rel_pc: u64,
        function_name_iid: Option<u64>,
        source_path_iid: Option<u64>,
        line_number: Option<u32>,
        pending: &mut InternedData,
    ) -> u64 {
        let key = FrameKey {
            mapping_iid,
            rel_pc,
            function_name_iid: function_name_iid.unwrap_or(0),
            source_path_iid: source_path_iid.unwrap_or(0),
            line_number: line_number.unwrap_or(0),
        };
        if let Some(&iid) = self.frames.get(&key) {
            return iid;
        }
        let iid = (self.frames.len() + 1) as u64;
        self.frames.insert(key, iid);
        pending.frames.push(Frame {
            iid: Some(iid),
            function_name_id: function_name_iid,
            mapping_id: Some(mapping_iid),
            rel_pc: Some(rel_pc),
            source_path_iid,
            line_number,
            kind_field: None,
        });
        iid
    }

    fn intern_callstack(&mut self, root_first_frames: Vec<u64>, pending: &mut InternedData) -> u64 {
        if let Some(&iid) = self.callstacks.get(&root_first_frames) {
            return iid;
        }
        let iid = (self.callstacks.len() + 1) as u64;
        self.callstacks.insert(root_first_frames.clone(), iid);
        pending.callstacks.push(Callstack { iid: Some(iid), frame_ids: root_first_frames });
        iid
    }
}

fn is_valid_symbol_name(name: &str) -> bool {
    let trimmed = name.trim();
    if trimmed.is_empty()
        || trimmed.starts_with("??")
        || trimmed.starts_with("0x")
        || trimmed.starts_with("0X")
    {
        return false;
    }
    !matches!(trimmed, "<unknown>" | "[unknown]" | "<plt>" | "[vdso]" | "unknown")
}

fn find_mapping_for_pc(
    modules: &[ModuleWithMmapDetails],
    lookup_addr: u64,
) -> Option<(usize, u64)> {
    for (mod_idx, details) in modules.iter().enumerate() {
        for mmap in &details.mmaps {
            let end = mmap.start_addr.saturating_add(mmap.size);
            if lookup_addr >= mmap.start_addr && lookup_addr < end {
                let first_mmap = details.mmaps.iter().min_by_key(|m| m.start_addr);
                let elf_base = first_mmap.map_or(0, |m| m.start_addr.wrapping_sub(m.vaddr));
                return Some((mod_idx, elf_base));
            }
        }
    }
    None
}

/// Encodes symbolized profiler records into a Perfetto `Trace` protobuf byte stream.
///
/// Emits packets on a single sequence (`PERFETTO_SEQUENCE_ID`) in timestamp order:
/// 1. **Sequence reset:** Marks the start of the sequence (`SeqIncrementalStateCleared`,
///    `first_packet_on_sequence = true`) so Perfetto accepts the interned dictionaries that follow.
/// 2. **`ProcessTree`:** Maps PIDs and TIDs to process and thread names for the Perfetto UI.
/// 3. **`PerfSample` packets:** One packet per sample (`pid`, `tid`, `callstack_iid`), carrying
///    any newly seen mappings, function names, frames, and callstacks in `interned_data`.
pub fn encode_perf_samples(records: &SymbolizedRecords) -> Vec<u8> {
    let mut warned_pids: HashSet<u64> = HashSet::new();
    let mut warned_tids: HashSet<u64> = HashSet::new();
    let mut flat_samples: Vec<(u64, Pid, Tid, &[ResolvedAddress])> = Vec::new();

    for (pid, _proc_name, thread_records) in &records.records {
        if pid.0 > u32::MAX as u64 && warned_pids.insert(pid.0) {
            log::warn!("PID koid {} exceeds u32::MAX; truncating to 32 bits", pid.0);
        }
        for thread_record in thread_records {
            if thread_record.tid.0 > u32::MAX as u64 && warned_tids.insert(thread_record.tid.0) {
                log::warn!(
                    "TID koid {} exceeds u32::MAX; truncating to 32 bits",
                    thread_record.tid.0
                );
            }
            for sample in &thread_record.samples {
                flat_samples.push((
                    sample.timestamp,
                    *pid,
                    thread_record.tid,
                    sample.frames.as_slice(),
                ));
            }
        }
    }

    if flat_samples.is_empty() {
        return Vec::new();
    }

    // Stable sort by (ts_ns, pid.0, tid.0) so equal timestamps across processes/threads have a
    // deterministic order while same-thread samples at identical timestamps preserve input order.
    flat_samples.sort_by_key(|(ts_ns, pid, tid, _)| (*ts_ns, pid.0, tid.0));
    let first_ts_ns = flat_samples[0].0;

    let seq_id_oneof =
        Some(trace_packet::OptionalTrustedPacketSequenceId::TrustedPacketSequenceId(
            PERFETTO_SEQUENCE_ID,
        ));

    let mut packets = Vec::with_capacity(flat_samples.len() + 2);

    // 1. First packet: sequence state cleared at first sample timestamp.
    packets.push(TracePacket {
        timestamp: Some(first_ts_ns),
        optional_trusted_packet_sequence_id: seq_id_oneof,
        sequence_flags: Some(trace_packet::SequenceFlags::SeqIncrementalStateCleared as u32),
        first_packet_on_sequence: Some(true),
        ..Default::default()
    });

    // 2. ProcessTree packet at first sample timestamp for named processes and threads.
    let mut seen_pids: HashSet<u64> = HashSet::new();
    let mut seen_tids: HashSet<u64> = HashSet::new();
    let mut processes = Vec::new();
    let mut threads = Vec::new();
    // Perfetto's ProcessTree schema types pid/tid/tgid as int32 (whereas PerfSample uses uint32).
    for (pid, proc_name, thread_records) in &records.records {
        if let Some(name) = proc_name {
            if !name.is_empty() && seen_pids.insert(pid.0) {
                processes.push(process_tree::Process {
                    pid: Some(pid.0 as i32),
                    cmdline: vec![name.clone()],
                    ..Default::default()
                });
            }
        }
        for thread_record in thread_records {
            if let Some(tname) = &thread_record.thread_name {
                if !tname.is_empty() && seen_tids.insert(thread_record.tid.0) {
                    threads.push(process_tree::Thread {
                        tid: Some(thread_record.tid.0 as i32),
                        tgid: Some(pid.0 as i32),
                        name: Some(tname.clone()),
                        ..Default::default()
                    });
                }
            }
        }
    }

    processes.sort_by_key(|p| p.pid.unwrap_or(0) as u32);
    threads.sort_by_key(|t| (t.tgid.unwrap_or(0) as u32, t.tid.unwrap_or(0) as u32));

    packets.push(TracePacket {
        timestamp: Some(first_ts_ns),
        optional_trusted_packet_sequence_id: seq_id_oneof,
        sequence_flags: Some(trace_packet::SequenceFlags::SeqNeedsIncrementalState as u32),
        data: Some(trace_packet::Data::ProcessTree(ProcessTree {
            processes,
            threads,
            collection_end_timestamp: None,
        })),
        ..Default::default()
    });

    // 3. Per-sample packets with incremental InternedData.
    let mut interner = SequenceInterner::default();

    for (ts_ns, pid, tid, frames) in flat_samples {
        let mut pending = InternedData::default();
        let pid_modules = records.modules.get(&pid).map_or(&[][..], Vec::as_slice);

        let callstack_iid = if frames.is_empty() {
            None
        } else {
            let mut leaf_first_frame_iids = Vec::with_capacity(frames.len());
            for (idx, resolved_addr) in frames.iter().enumerate() {
                let pc = resolved_addr.addr;
                // Non-leaf frames (idx > 0) are return addresses and may point one byte past the
                // end of a tail-call's mapping, so look up the mapping at `pc - 1`.
                let lookup_addr = if idx > 0 && pc > 0 { pc - 1 } else { pc };

                let (mapping_iid, rel_pc) = if let Some((mod_idx, elf_base)) =
                    find_mapping_for_pc(pid_modules, lookup_addr)
                {
                    let map_iid = interner.intern_module_mapping(
                        pid.0,
                        mod_idx,
                        &pid_modules[mod_idx],
                        &mut pending,
                    );
                    (map_iid, pc.wrapping_sub(elf_base))
                } else {
                    let map_iid = interner.intern_unmapped_mapping(&mut pending);
                    (map_iid, pc)
                };

                // Expand inlined locations at this address (`locations[0]` is the innermost frame
                // and subsequent entries are inlined callers from inner to outer). When
                // `leaf_first_frame_iids` is reversed below into root-first `Callstack.frame_ids`,
                // outer callers precede their inlined callees.
                let mut emitted_any = false;
                for loc in &resolved_addr.locations {
                    let fn_iid = is_valid_symbol_name(&loc.function).then(|| {
                        interner.intern_function_name(loc.function.as_bytes(), &mut pending)
                    });
                    let (source_path_iid, line_number) = match &loc.file_and_line {
                        Some((file, line)) if !file.is_empty() => (
                            Some(interner.intern_source_path(file.as_bytes(), &mut pending)),
                            (*line > 0).then_some(*line),
                        ),
                        _ => (None, None),
                    };
                    if fn_iid.is_some() || source_path_iid.is_some() {
                        let frame_iid = interner.intern_frame(
                            mapping_iid,
                            rel_pc,
                            fn_iid,
                            source_path_iid,
                            line_number,
                            &mut pending,
                        );
                        leaf_first_frame_iids.push(frame_iid);
                        emitted_any = true;
                    }
                }
                if !emitted_any {
                    let frame_iid =
                        interner.intern_frame(mapping_iid, rel_pc, None, None, None, &mut pending);
                    leaf_first_frame_iids.push(frame_iid);
                }
            }

            leaf_first_frame_iids.reverse();
            Some(interner.intern_callstack(leaf_first_frame_iids, &mut pending))
        };

        packets.push(TracePacket {
            timestamp: Some(ts_ns),
            optional_trusted_packet_sequence_id: seq_id_oneof,
            interned_data: (pending != InternedData::default()).then_some(pending),
            sequence_flags: Some(trace_packet::SequenceFlags::SeqNeedsIncrementalState as u32),
            data: Some(trace_packet::Data::PerfSample(PerfSample {
                pid: Some(pid.0 as u32),
                tid: Some(tid.0 as u32),
                cpu_mode: Some(profiling::CpuMode::ModeUser as i32),
                callstack_iid,
                ..Default::default()
            })),
            ..Default::default()
        });
    }

    Trace { packet: packets }.encode_to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::ModuleDetails;
    use crate::symbolize::{SymbolizedRecord, SymbolizedSample};
    use ffx_symbolize::{MappingDetails, MappingFlags, ResolvedLocation};

    fn make_resolved(addr: u64, funcs: &[&str]) -> ResolvedAddress {
        ResolvedAddress {
            addr,
            locations: funcs
                .iter()
                .map(|f| ResolvedLocation {
                    function: (*f).to_string(),
                    file_and_line: None,
                    library: None,
                    library_offset: 0,
                })
                .collect(),
        }
    }

    #[test]
    fn test_encode_perf_samples_empty_records_returns_empty_vec() {
        let records = SymbolizedRecords { records: Vec::new(), modules: HashMap::new() };
        let bytes = encode_perf_samples(&records);
        assert!(bytes.is_empty());
    }

    #[test]
    fn test_encode_perf_samples_round_trip_invariants() {
        let pid1 = Pid(1001);
        let pid2 = Pid(2002);
        let tid1 = Tid(10010);
        let tid2 = Tid(10011);
        let tid3 = Tid(20020);

        let mut modules = HashMap::new();
        modules.insert(
            pid1,
            vec![ModuleWithMmapDetails {
                module: ModuleDetails {
                    name: "starnix_kernel".to_string(),
                    build_id: "59201bd8267f0f574b4dd21a93bf28be0ffdf169".to_string(),
                },
                mmaps: vec![MappingDetails {
                    start_addr: 0x1000_0000,
                    size: 0x2000,
                    vaddr: 0x1000,
                    flags: MappingFlags::empty(),
                }],
            }],
        );
        modules.insert(
            pid2,
            vec![ModuleWithMmapDetails {
                module: ModuleDetails {
                    name: "libart.so".to_string(),
                    build_id: "deadbeef01020304".to_string(),
                },
                mmaps: vec![MappingDetails {
                    start_addr: 0x2000_0000,
                    size: 0x4000,
                    vaddr: 0x0,
                    flags: MappingFlags::empty(),
                }],
            }],
        );

        // Sample A (tid1, timestamp = 2s):
        // - leaf at 0x1000_0500 (inlined ["inner_leaf" at mm.rs:42, "outer_leaf" at task.rs:108])
        // - return address at 0x1000_2000 (exact end of mmap: pc is outside [0x1000_0000, 0x1000_2000),
        //   but pc - 1 = 0x1000_1fff is inside! Tests pc-1 lookup with raw pc rel_pc calculation)
        // - unmapped root at 0x9999_0000 (unresolved)
        let sample_a = SymbolizedSample {
            timestamp: 2_000_000_000,
            frames: vec![
                ResolvedAddress {
                    addr: 0x1000_0500,
                    locations: vec![
                        ResolvedLocation {
                            function: "inner_leaf".to_string(),
                            file_and_line: Some(("src/starnix/mm.rs".to_string(), 42)),
                            library: None,
                            library_offset: 0,
                        },
                        ResolvedLocation {
                            function: "outer_leaf".to_string(),
                            file_and_line: Some(("src/starnix/task.rs".to_string(), 108)),
                            library: None,
                            library_offset: 0,
                        },
                    ],
                },
                make_resolved(0x1000_2000, &["caller_at_boundary"]),
                make_resolved(0x9999_0000, &[]),
            ],
        };

        // Sample B (tid2, timestamp = 1s, earlier than Sample A to test sorting):
        // - empty stack sample
        let sample_b = SymbolizedSample { timestamp: 1_000_000_000, frames: vec![] };

        // Sample C (tid3 in pid2, timestamp = 3s):
        // - leaf at 0x2000_0100 (unresolved in mapped module -> no function_name_id)
        // - root at 0x1000_0500 (same function name "inner_leaf" as Sample A to test dedup)
        let sample_c = SymbolizedSample {
            timestamp: 3_000_000_000,
            frames: vec![
                make_resolved(0x2000_0100, &[]),
                make_resolved(0x2000_0200, &["inner_leaf"]),
            ],
        };

        let records = SymbolizedRecords {
            records: vec![
                (
                    pid1,
                    Some("starnix_kernel.cm".to_string()),
                    vec![
                        SymbolizedRecord {
                            tid: tid1,
                            thread_name: Some("initial-thread".to_string()),
                            samples: vec![sample_a],
                        },
                        SymbolizedRecord {
                            tid: tid2,
                            thread_name: None, // unnamed thread should be omitted from ProcessTree
                            samples: vec![sample_b],
                        },
                    ],
                ),
                (
                    pid2,
                    None, // unnamed process should be omitted from ProcessTree
                    vec![SymbolizedRecord {
                        tid: tid3,
                        thread_name: Some("binder:2002_1".to_string()),
                        samples: vec![sample_c],
                    }],
                ),
            ],
            modules,
        };

        let encoded = encode_perf_samples(&records);
        assert!(!encoded.is_empty());

        let trace = Trace::decode(encoded.as_slice()).expect("valid Trace protobuf");
        // Expect: 1 clear packet + 1 ProcessTree packet + 3 sample packets = 5 packets
        assert_eq!(trace.packet.len(), 5);

        let expected_ts_b = 1_000_000_000u64;
        let expected_ts_a = 2_000_000_000u64;
        let expected_ts_c = 3_000_000_000u64;

        // Check Packet 0: sequence clear packet
        let pkt0 = &trace.packet[0];
        assert_eq!(pkt0.timestamp, Some(expected_ts_b));
        assert_eq!(pkt0.timestamp_clock_id, None);
        assert_eq!(
            pkt0.optional_trusted_packet_sequence_id,
            Some(trace_packet::OptionalTrustedPacketSequenceId::TrustedPacketSequenceId(
                PERFETTO_SEQUENCE_ID
            ))
        );
        assert_eq!(
            pkt0.sequence_flags,
            Some(trace_packet::SequenceFlags::SeqIncrementalStateCleared as u32)
        );
        assert_eq!(pkt0.first_packet_on_sequence, Some(true));

        // Check Packet 1: ProcessTree packet
        let pkt1 = &trace.packet[1];
        assert_eq!(pkt1.timestamp, Some(expected_ts_b));
        assert_eq!(
            pkt1.sequence_flags,
            Some(trace_packet::SequenceFlags::SeqNeedsIncrementalState as u32)
        );
        let Some(trace_packet::Data::ProcessTree(pt)) = &pkt1.data else {
            panic!("expected ProcessTree in packet 1");
        };
        assert_eq!(pt.processes.len(), 1);
        assert_eq!(pt.processes[0].pid, Some(1001));
        assert_eq!(pt.processes[0].cmdline, vec!["starnix_kernel.cm".to_string()]);
        assert_eq!(pt.threads.len(), 2);
        assert_eq!(pt.threads[0].tid, Some(10010));
        assert_eq!(pt.threads[0].tgid, Some(1001));
        assert_eq!(pt.threads[0].name.as_deref(), Some("initial-thread"));
        assert_eq!(pt.threads[1].tid, Some(20020));
        assert_eq!(pt.threads[1].tgid, Some(2002));
        assert_eq!(pt.threads[1].name.as_deref(), Some("binder:2002_1"));

        // Walk all packets and verify incremental interning invariants:
        // Every referenced iid must be defined in an earlier or the same packet,
        // iids are dense 1-based, and never redefined.
        let mut seen_build_ids: HashMap<u64, Vec<u8>> = HashMap::new();
        let mut seen_paths: HashMap<u64, String> = HashMap::new();
        let mut seen_func_names: HashMap<u64, String> = HashMap::new();
        let mut seen_source_paths: HashMap<u64, String> = HashMap::new();
        let mut seen_mappings: HashMap<u64, Mapping> = HashMap::new();
        let mut seen_frames: HashMap<u64, Frame> = HashMap::new();
        let mut seen_callstacks: HashMap<u64, Callstack> = HashMap::new();

        for (pkt_idx, pkt) in trace.packet.iter().enumerate() {
            assert_eq!(
                pkt.optional_trusted_packet_sequence_id,
                Some(trace_packet::OptionalTrustedPacketSequenceId::TrustedPacketSequenceId(
                    PERFETTO_SEQUENCE_ID
                ))
            );
            assert_eq!(pkt.timestamp_clock_id, None);
            if pkt_idx > 0 {
                assert_eq!(
                    pkt.sequence_flags,
                    Some(trace_packet::SequenceFlags::SeqNeedsIncrementalState as u32)
                );
                assert_eq!(pkt.first_packet_on_sequence, None);
            }

            if let Some(interned) = &pkt.interned_data {
                for b in &interned.build_ids {
                    let iid = b.iid.expect("build_id iid");
                    assert_eq!(iid, (seen_build_ids.len() + 1) as u64);
                    assert!(seen_build_ids.insert(iid, b.str.clone().unwrap()).is_none());
                }
                for p in &interned.mapping_paths {
                    let iid = p.iid.expect("mapping_path iid");
                    assert_eq!(iid, (seen_paths.len() + 1) as u64);
                    let s = String::from_utf8(p.str.clone().unwrap()).unwrap();
                    assert!(seen_paths.insert(iid, s).is_none());
                }
                for fn_entry in &interned.function_names {
                    let iid = fn_entry.iid.expect("function_name iid");
                    assert_eq!(iid, (seen_func_names.len() + 1) as u64);
                    let s = String::from_utf8(fn_entry.str.clone().unwrap()).unwrap();
                    assert!(seen_func_names.insert(iid, s).is_none());
                }
                for sp_entry in &interned.source_paths {
                    let iid = sp_entry.iid.expect("source_path iid");
                    assert_eq!(iid, (seen_source_paths.len() + 1) as u64);
                    let s = String::from_utf8(sp_entry.str.clone().unwrap()).unwrap();
                    assert!(seen_source_paths.insert(iid, s).is_none());
                }
                for m in &interned.mappings {
                    let iid = m.iid.expect("mapping iid");
                    assert_eq!(iid, (seen_mappings.len() + 1) as u64);
                    if let Some(bid) = m.build_id {
                        assert!(seen_build_ids.contains_key(&bid));
                    }
                    for &pid_str in &m.path_string_ids {
                        assert!(seen_paths.contains_key(&pid_str));
                    }
                    assert!(seen_mappings.insert(iid, m.clone()).is_none());
                }
                for f in &interned.frames {
                    let iid = f.iid.expect("frame iid");
                    assert_eq!(iid, (seen_frames.len() + 1) as u64);
                    assert!(seen_mappings.contains_key(&f.mapping_id.unwrap()));
                    if let Some(fn_id) = f.function_name_id {
                        assert!(seen_func_names.contains_key(&fn_id));
                    }
                    if let Some(sp_id) = f.source_path_iid {
                        assert!(seen_source_paths.contains_key(&sp_id));
                    }
                    assert!(seen_frames.insert(iid, f.clone()).is_none());
                }
                for cs in &interned.callstacks {
                    let iid = cs.iid.expect("callstack iid");
                    assert_eq!(iid, (seen_callstacks.len() + 1) as u64);
                    assert!(!cs.frame_ids.is_empty());
                    for &fid in &cs.frame_ids {
                        assert!(seen_frames.contains_key(&fid));
                    }
                    assert!(seen_callstacks.insert(iid, cs.clone()).is_none());
                }
            }

            if let Some(trace_packet::Data::PerfSample(ps)) = &pkt.data {
                if let Some(cs_iid) = ps.callstack_iid {
                    assert!(seen_callstacks.contains_key(&cs_iid));
                }
            }
        }

        // Packet 2: Sample B (earliest timestamp, empty stack -> no callstack_iid)
        let pkt2 = &trace.packet[2];
        assert_eq!(pkt2.timestamp, Some(expected_ts_b));
        let Some(trace_packet::Data::PerfSample(ps_b)) = &pkt2.data else {
            panic!("expected PerfSample in packet 2");
        };
        assert_eq!(ps_b.pid, Some(1001));
        assert_eq!(ps_b.tid, Some(10011));
        assert_eq!(ps_b.callstack_iid, None);
        assert_eq!(ps_b.cpu_mode, Some(profiling::CpuMode::ModeUser as i32));

        // Packet 3: Sample A (4 frames after inline expansion: unmapped root, boundary caller, outer_leaf, inner_leaf)
        let pkt3 = &trace.packet[3];
        assert_eq!(pkt3.timestamp, Some(expected_ts_a));
        let Some(trace_packet::Data::PerfSample(ps_a)) = &pkt3.data else {
            panic!("expected PerfSample in packet 3");
        };
        assert_eq!(ps_a.pid, Some(1001));
        assert_eq!(ps_a.tid, Some(10010));
        let cs_a = seen_callstacks.get(&ps_a.callstack_iid.unwrap()).unwrap();
        // Root-first order: [unmapped root, boundary caller, outer_leaf, inner_leaf]
        assert_eq!(cs_a.frame_ids.len(), 4);
        let root_frame = seen_frames.get(&cs_a.frame_ids[0]).unwrap();
        let boundary_frame = seen_frames.get(&cs_a.frame_ids[1]).unwrap();
        let outer_leaf_frame = seen_frames.get(&cs_a.frame_ids[2]).unwrap();
        let leaf_frame = seen_frames.get(&cs_a.frame_ids[3]).unwrap();

        // Root frame is unmapped and unresolved
        assert_eq!(root_frame.function_name_id, None);
        assert_eq!(root_frame.rel_pc, Some(0x9999_0000));
        let root_map = seen_mappings.get(&root_frame.mapping_id.unwrap()).unwrap();
        assert_eq!(root_map.start, Some(0));
        assert_eq!(root_map.end, Some(0));
        assert_eq!(seen_paths.get(&root_map.path_string_ids[0]).unwrap(), "[unmapped]");

        // Boundary frame (pc = 0x1000_2000, lookup at pc - 1 = 0x1000_1fff matched starnix_kernel)
        // rel_pc = 0x1000_2000 - (0x1000_0000 - 0x1000) = 0x3000
        assert_eq!(boundary_frame.rel_pc, Some(0x3000));
        assert_eq!(
            seen_func_names.get(&boundary_frame.function_name_id.unwrap()).unwrap(),
            "caller_at_boundary"
        );
        let starnix_map = seen_mappings.get(&boundary_frame.mapping_id.unwrap()).unwrap();
        assert_eq!(seen_paths.get(&starnix_map.path_string_ids[0]).unwrap(), "starnix_kernel");
        assert_eq!(starnix_map.load_bias, Some(0x1000));
        assert_eq!(
            seen_build_ids.get(&starnix_map.build_id.unwrap()).unwrap(),
            &hex::decode("59201bd8267f0f574b4dd21a93bf28be0ffdf169").unwrap()
        );

        // Inlined caller at leaf address (pc = 0x1000_0500, rel_pc = 0x1500, "outer_leaf" at task.rs:108)
        assert_eq!(outer_leaf_frame.rel_pc, Some(0x1500));
        assert_eq!(
            seen_func_names.get(&outer_leaf_frame.function_name_id.unwrap()).unwrap(),
            "outer_leaf"
        );
        assert_eq!(
            seen_source_paths.get(&outer_leaf_frame.source_path_iid.unwrap()).unwrap(),
            "src/starnix/task.rs"
        );
        assert_eq!(outer_leaf_frame.line_number, Some(108));

        // Leaf frame (pc = 0x1000_0500, rel_pc = 0x1500, innermost function "inner_leaf" at mm.rs:42)
        assert_eq!(leaf_frame.rel_pc, Some(0x1500));
        assert_eq!(
            seen_func_names.get(&leaf_frame.function_name_id.unwrap()).unwrap(),
            "inner_leaf"
        );
        assert_eq!(
            seen_source_paths.get(&leaf_frame.source_path_iid.unwrap()).unwrap(),
            "src/starnix/mm.rs"
        );
        assert_eq!(leaf_frame.line_number, Some(42));
        let reconstructed_leaf_pc = leaf_frame.rel_pc.unwrap() + starnix_map.start.unwrap()
            - starnix_map.load_bias.unwrap_or(0);
        assert_eq!(reconstructed_leaf_pc, 0x1000_0500);

        // Packet 4: Sample C (2 frames: unresolved leaf in libart.so, root with shared "inner_leaf")
        let pkt4 = &trace.packet[4];
        assert_eq!(pkt4.timestamp, Some(expected_ts_c));
        // "inner_leaf" was already interned in Packet 3, so Packet 4 must NOT re-emit any function_names
        assert!(pkt4.interned_data.as_ref().unwrap().function_names.is_empty());
        let Some(trace_packet::Data::PerfSample(ps_c)) = &pkt4.data else {
            panic!("expected PerfSample in packet 4");
        };
        let cs_c = seen_callstacks.get(&ps_c.callstack_iid.unwrap()).unwrap();
        assert_eq!(cs_c.frame_ids.len(), 2);
        let c_leaf = seen_frames.get(&cs_c.frame_ids[1]).unwrap();
        assert_eq!(c_leaf.function_name_id, None);
        assert_eq!(c_leaf.rel_pc, Some(0x100));
    }

    #[test]
    fn test_is_valid_symbol_name_rejects_placeholders() {
        for invalid in [
            "",
            "   ",
            "??",
            "??:0",
            "0x1234",
            "0XDEAD",
            "<plt>",
            "<unknown>",
            "[unknown]",
            "[vdso]",
            "unknown",
        ] {
            assert!(!is_valid_symbol_name(invalid), "expected '{invalid}' to be rejected");
        }
        for valid in [
            "main",
            "starnix_core::mm::mapping",
            "restricted_return_loop",
            "<alloc::vec::Vec<T> as core::ops::drop::Drop>::drop",
            "<T as core::convert::Into<U>>::into",
            "[T]::clone",
            "<[u8]>::copy_from_slice",
            "<VMO#36283=blob-80876207>",
        ] {
            assert!(is_valid_symbol_name(valid), "expected '{valid}' to be accepted");
        }
    }

    #[test]
    fn test_encode_perf_samples_deterministic_across_record_orders_and_tied_timestamps() {
        let pid_a = Pid(100);
        let pid_b = Pid(200);
        let tid_a1 = Tid(101);
        let tid_a2 = Tid(102);
        let tid_b1 = Tid(201);

        let rec_a1 = SymbolizedRecord {
            tid: tid_a1,
            thread_name: Some("worker_a1".to_string()),
            samples: vec![SymbolizedSample {
                timestamp: 5_000,
                frames: vec![make_resolved(0x1000, &["fn_a1"])],
            }],
        };
        let rec_a2 = SymbolizedRecord {
            tid: tid_a2,
            thread_name: Some("worker_a2".to_string()),
            samples: vec![SymbolizedSample {
                timestamp: 5_000,
                frames: vec![make_resolved(0x2000, &["fn_a2"])],
            }],
        };
        let rec_b1 = SymbolizedRecord {
            tid: tid_b1,
            thread_name: Some("worker_b1".to_string()),
            samples: vec![SymbolizedSample {
                timestamp: 5_000,
                frames: vec![make_resolved(0x3000, &["fn_b1"])],
            }],
        };

        let order_1 = SymbolizedRecords {
            records: vec![
                (pid_a, Some("proc_a.cm".to_string()), vec![rec_a1.clone(), rec_a2.clone()]),
                (pid_b, Some("proc_b.cm".to_string()), vec![rec_b1.clone()]),
            ],
            modules: HashMap::new(),
        };
        let order_2 = SymbolizedRecords {
            records: vec![
                (pid_b, Some("proc_b.cm".to_string()), vec![rec_b1]),
                (pid_a, Some("proc_a.cm".to_string()), vec![rec_a2, rec_a1]),
            ],
            modules: HashMap::new(),
        };

        let bytes_1 = encode_perf_samples(&order_1);
        let bytes_2 = encode_perf_samples(&order_2);
        assert_eq!(bytes_1, bytes_2, "Perfetto encoding must be 100% deterministic");
    }
}
