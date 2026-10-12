// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub use crate::parse::{SymbolizeError, UnsymbolizedSamples};

use crate::parse::{BacktraceDetails, ModuleWithMmapDetails, Pid, ProfilingRecordHandler, Tid};
use ffx_symbolize::{ResolvedLocation, Symbolizer};
use rayon::prelude::*;
use std::collections::HashMap;
use std::path::Path;

// It defines how many processes a symbolizer will handle.
// We create a symbolizer for every thread.
// More threads => more symbolizers => more latency, but higher throughput.
// NUM_PROCESS_PER_THREAD is a hard coded number considering the trade off above.
const NUM_PROCESS_PER_THREAD: usize = 4;

/// A resolved address.
#[derive(Clone, PartialEq)]
pub struct ResolvedAddress {
    /// Address for which source locations were resolved.
    pub addr: u64,
    /// Source locations found at `addr`.
    pub locations: Vec<ResolvedLocation>,
}

impl std::fmt::Debug for ResolvedAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedAddress")
            .field("addr", &format_args!("0x{:x}", self.addr))
            .field("lines", &self.locations)
            .finish()
    }
}

/// A single symbolized backtrace sample with its timestamp in nanoseconds.
#[derive(Clone, Debug, PartialEq)]
pub struct SymbolizedSample {
    pub timestamp: u64,
    pub frames: Vec<ResolvedAddress>,
}

/// Symbolized record hash map. key: pid, value: all of the records belong to this pid.
#[derive(Clone, Debug, Default)]
pub struct SymbolizedRecords {
    pub records: Vec<(Pid, Option<String>, Vec<SymbolizedRecord>)>,
    pub modules: HashMap<Pid, Vec<ModuleWithMmapDetails>>,
}

/// Symbolized bt list for a single tid.
#[derive(Clone, Debug)]
pub struct SymbolizedRecord {
    pub tid: Tid,
    pub thread_name: Option<String>,
    pub samples: Vec<SymbolizedSample>,
}

impl SymbolizedRecord {
    fn add_sample(&mut self, timestamp: u64, frames: Vec<ResolvedAddress>) {
        self.samples.push(SymbolizedSample { timestamp, frames });
    }
}

pub fn create_unsymbolized_samples(
    input: impl AsRef<Path>,
) -> Result<UnsymbolizedSamples, SymbolizeError> {
    logging_rust_cpp_bridge::init_with_log_severity(logging_rust_cpp_bridge::FUCHSIA_LOG_FATAL);
    UnsymbolizedSamples::new_from_fxt_file(input)
}

fn find_debug_file(
    symbol_index: &symbol_index::SymbolIndex,
    extra_build_id_dirs: &[std::path::PathBuf],
    binary_id: &str,
) -> Option<std::path::PathBuf> {
    if binary_id.len() <= 2 {
        return None;
    }
    let probe_dir = |base: &Path| -> Option<std::path::PathBuf> {
        let prefix = base.join(&binary_id[..2]);
        let with_debug = prefix.join(format!("{}.debug", &binary_id[2..]));
        if with_debug.exists() {
            return Some(with_debug);
        }
        let plain = prefix.join(&binary_id[2..]);
        if plain.exists() {
            return Some(plain);
        }
        None
    };

    for dir in extra_build_id_dirs {
        if let Some(p) = probe_dir(dir) {
            return Some(p);
        }
    }

    if let Some(p) =
        symbol_index.build_id_dirs.iter().find_map(|dir| probe_dir(Path::new(&dir.path)))
    {
        return Some(p);
    }

    // Fallback to the default symbol cache and build-id directories
    if let Ok(home) = std::env::var("HOME") {
        let home_path = std::path::PathBuf::from(home);
        for rel in [".fuchsia/debug/symbol-cache", ".fuchsia/debug/build-id"] {
            if let Some(p) = probe_dir(&home_path.join(rel)) {
                return Some(p);
            }
        }
    }
    None
}

/// The device reports a process's main executable as an anonymous VMO name (`<VMO#…=blob-…>`
/// or `blob-…`) while shared libraries already carry their real names. Substitute the owning
/// component/process name (minus `.cm`) for the former so Perfetto mappings stay readable.
fn display_module_name(device_name: &str, process_name: Option<&str>) -> String {
    if device_name.starts_with("<VMO#") || device_name.starts_with("blob-") {
        if let Some(pname) = process_name {
            let stripped = pname.strip_suffix(".cm").unwrap_or(pname).trim();
            if !stripped.is_empty() {
                return stripped.to_string();
            }
        }
    }
    device_name.to_string()
}

impl UnsymbolizedSamples {
    pub fn process_unsymbolized_samples(
        self,
        output: impl AsRef<Path>,
        pprof_conversion: bool,
        context: &ffx_config::EnvironmentContext,
    ) -> Result<SymbolizedRecords, SymbolizeError> {
        if let Err(e) = symbol_index::ensure_symbol_index_registered(context) {
            log::warn!("ensure_symbol_index_registered failed, error was: {:#?}", e);
        }
        let symbol_index_path =
            symbol_index::global_symbol_index_path().unwrap_or_else(|_| "".to_string());
        let global_symbol_index = symbol_index::SymbolIndex::load_aggregate(&symbol_index_path)
            .unwrap_or_else(|_| symbol_index::SymbolIndex::new());

        let mut extra_build_id_dirs = Vec::new();
        if let Some(bd) = context.build_dir() {
            let bid = bd.join(".build-id");
            if bid.exists() {
                extra_build_id_dirs.push(bid);
            }
        }

        let mut handlers: Vec<(Pid, ProfilingRecordHandler)> = self.handlers.into_iter().collect();
        handlers.sort_by_key(|(pid, _)| pid.0);

        let symbolized_samples = handlers.par_iter().chunks(NUM_PROCESS_PER_THREAD).map(|chunk| -> Result<Vec<(Pid, Option<String>, Vec<SymbolizedRecord>)>, SymbolizeError> {
        let mut symbolizer = Symbolizer::with_context(context)?;
        let symbolized_samples_per_thread: Result<Vec<(Pid, Option<String>, Vec<SymbolizedRecord>)>, SymbolizeError> = chunk.into_iter().map(|(pid, handler):&(Pid, ProfilingRecordHandler)| -> Result<(Pid, Option<String>, Vec<SymbolizedRecord>), SymbolizeError> {
                    let mut res_per_pid = vec![];

                    let unwinder = crate::unwinder::Unwinder::new();

                    // We use a hashmap to store the seen backtrace, to avoid symbolize the same backtrace multiple times.
                    let mut seen_bt: HashMap<BacktraceDetails, ResolvedAddress> = HashMap::new();
                    let mut mod_entries: Vec<(&u16, &ModuleWithMmapDetails)> =
                        handler.module_with_mmap_records.iter().collect();
                    mod_entries.sort_by_key(|(id, _)| **id);
                    for (_, ModuleWithMmapDetails {module, mmaps}) in mod_entries {
                        let build_id_bytes = hex::decode(&module.build_id)?;
                        let module_id = symbolizer
                            .add_module(&module.name, &build_id_bytes);

                        // Provide the unstripped host binary file path to the unwinder
                        let debug_file = find_debug_file(&global_symbol_index, &extra_build_id_dirs, &module.build_id);
                        let debug_path = debug_file.as_ref().and_then(|p| p.to_str());

                        let mut min_load_address = u64::MAX;
                        for mmap_record in mmaps {
                            symbolizer.add_mapping(module_id, mmap_record.clone())?;
                            if debug_path.is_some() {
                                let load_address = mmap_record.start_addr.saturating_sub(mmap_record.vaddr);
                                min_load_address = min_load_address.min(load_address);
                            }
                        }
                        if let Some(path_str) = debug_path {
                            if min_load_address != u64::MAX {
                                unwinder.add_module(min_load_address, path_str);
                            }
                        }
                    }

                    for (tid, backtraces) in &handler.backtrace_records {
                        let thread_name = self.thread_names.get(tid).cloned();
                        let mut symbolized_record = SymbolizedRecord {
                            tid: *tid,
                            thread_name,
                            samples: Vec::new(),
                        };

                        for bt_sample in backtraces {
                            let mut current_call_stack = vec![];
                            for backtrace in &bt_sample.frames {
                                let resolved_addr =
                                    seen_bt.entry(*backtrace).or_insert_with_key(|bt_key| {
                                        let resolved_locations = symbolizer
                                            .resolve_addr(bt_key.address, bt_key.address_type)
                                            .unwrap_or_default();
                                        ResolvedAddress {
                                            addr: bt_key.address,
                                            locations: resolved_locations,
                                        }
                                    }).to_owned();
                                current_call_stack.push(resolved_addr);
                            }
                            symbolized_record.add_sample(bt_sample.timestamp, current_call_stack);
                        }
                        res_per_pid.push(symbolized_record);
                    }

                    for (tid, samples) in &handler.raw_samples {
                        let thread_name = self.thread_names.get(tid).cloned();
                        let mut symbolized_record = SymbolizedRecord {
                            tid: *tid,
                            thread_name,
                            samples: Vec::new(),
                        };

                        for sample in samples {
                            if let Ok(regs_data) = unwinder.set_sample_context(&sample.sample_memory) {
                                let frames = unwinder.unwind(regs_data, 128);
                                let mut current_call_stack = vec![];
                                for frame in frames {
                                    let bt = BacktraceDetails::new(
                                        frame.pc,
                                        frame.pc_is_return_address.into(),
                                    );
                                    let resolved_addr = seen_bt.entry(bt).or_insert_with_key(|bt_key| {
                                        let resolved_locations = symbolizer
                                            .resolve_addr(bt_key.address, bt_key.address_type)
                                            .unwrap_or_default();
                                        ResolvedAddress {
                                            addr: bt_key.address,
                                            locations: resolved_locations,
                                        }
                                    }).to_owned();
                                    current_call_stack.push(resolved_addr);
                                }
                                symbolized_record.add_sample(sample.timestamp, current_call_stack);
                            }
                        }
                        if !symbolized_record.samples.is_empty() {
                            res_per_pid.push(symbolized_record);
                        }
                    }

                    res_per_pid.sort_by_key(|r| r.tid.0);
                    symbolizer.reset();
                    Ok((*pid, handler.process_name.clone(), res_per_pid))
        })
        .collect::<Result<Vec<(Pid, Option<String>, Vec<SymbolizedRecord>)>, SymbolizeError>>();
        symbolized_samples_per_thread
        }).collect::<Result<Vec<Vec<(Pid, Option<String>, Vec<SymbolizedRecord>)>>, SymbolizeError>>()?;
        let mut symbolized_samples: Vec<(Pid, Option<String>, Vec<SymbolizedRecord>)> =
            symbolized_samples.into_iter().flatten().collect();
        symbolized_samples.sort_by_key(|(pid, _, _)| pid.0);

        // Map each executable build_id owned by a `.cm` component to that `.cm` name so
        // dynamic child processes (e.g. `foo.cm:bar`) whose first module shares the same
        // executable build_id can normalize back to `foo.cm`. Because `handlers` is sorted
        // by ascending `Pid`, if multiple `.cm` components share the same binary, `or_insert`
        // deterministically attributes ownership to whichever `.cm` process has the lowest PID.
        let mut build_id_cm_owner: HashMap<&str, &str> = HashMap::new();
        for (_pid, handler) in &handlers {
            if let Some(pname) = handler.process_name.as_deref() {
                if pname.ends_with(".cm") {
                    for details in handler.module_with_mmap_records.values() {
                        if !details.module.build_id.is_empty() {
                            build_id_cm_owner
                                .entry(details.module.build_id.as_str())
                                .or_insert(pname);
                        }
                    }
                }
            }
        }

        let mut modules_by_pid: HashMap<Pid, Vec<ModuleWithMmapDetails>> = HashMap::new();
        for (pid, handler) in &handlers {
            let mut entries: Vec<(&u16, &ModuleWithMmapDetails)> =
                handler.module_with_mmap_records.iter().collect();
            entries.sort_by_key(|(id, _)| **id);
            let mut mods = Vec::with_capacity(entries.len());
            for (_, details) in entries {
                let mut cloned = details.clone();
                let effective_process_name = build_id_cm_owner
                    .get(cloned.module.build_id.as_str())
                    .copied()
                    .or(handler.process_name.as_deref());
                cloned.module.name =
                    display_module_name(&cloned.module.name, effective_process_name);
                mods.push(cloned);
            }
            modules_by_pid.insert(*pid, mods);
        }

        if !pprof_conversion {
            std::fs::write(output, format!("{symbolized_samples:#?}\n"))?;
        }
        Ok(SymbolizedRecords { records: symbolized_samples, modules: modules_by_pid })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ffx_symbolize::AddressType;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn test_backtrace_details_cache_partitioning() {
        let exact_bt = BacktraceDetails::new(0x1234, AddressType::Exact);
        let return_bt = BacktraceDetails::new(0x1234, AddressType::Return);
        let unknown_bt = BacktraceDetails::new(0x1234, AddressType::Unknown);

        assert_ne!(exact_bt, return_bt);
        assert_ne!(exact_bt, unknown_bt);
        assert_ne!(return_bt, unknown_bt);

        let mut map = HashMap::new();
        map.insert(
            exact_bt,
            ResolvedAddress {
                addr: 0x1234,
                locations: vec![ResolvedLocation {
                    function: "exact_fn".to_string(),
                    file_and_line: None,
                    library: None,
                    library_offset: 0,
                }],
            },
        );
        map.insert(
            return_bt,
            ResolvedAddress {
                addr: 0x1234,
                locations: vec![ResolvedLocation {
                    function: "return_fn".to_string(),
                    file_and_line: None,
                    library: None,
                    library_offset: 0,
                }],
            },
        );
        map.insert(
            unknown_bt,
            ResolvedAddress {
                addr: 0x1234,
                locations: vec![ResolvedLocation {
                    function: "unknown_fn".to_string(),
                    file_and_line: None,
                    library: None,
                    library_offset: 0,
                }],
            },
        );

        assert_eq!(map.len(), 3);
        assert_eq!(map.get(&exact_bt).unwrap().locations[0].function, "exact_fn");
        assert_eq!(map.get(&return_bt).unwrap().locations[0].function, "return_fn");
        assert_eq!(map.get(&unknown_bt).unwrap().locations[0].function, "unknown_fn");
    }

    #[test]
    fn test_seen_bt_or_insert_with_key_cache_behavior() {
        let mut seen_bt: HashMap<BacktraceDetails, ResolvedAddress> = HashMap::new();
        let exact_calls = AtomicUsize::new(0);
        let return_calls = AtomicUsize::new(0);

        let resolve_mock = |details: &BacktraceDetails| -> ResolvedAddress {
            let fn_name = match details.address_type {
                AddressType::Exact => {
                    exact_calls.fetch_add(1, Ordering::SeqCst);
                    format!("exact_0x{:x}", details.address)
                }
                AddressType::Return => {
                    return_calls.fetch_add(1, Ordering::SeqCst);
                    format!("return_0x{:x}", details.address)
                }
                AddressType::Unknown => format!("unknown_0x{:x}", details.address),
            };
            ResolvedAddress {
                addr: details.address,
                locations: vec![ResolvedLocation {
                    function: fn_name,
                    file_and_line: None,
                    library: None,
                    library_offset: 0,
                }],
            }
        };

        let addr = 0x5000;
        let exact_key = BacktraceDetails::new(addr, AddressType::Exact);
        let return_key = BacktraceDetails::new(addr, AddressType::Return);

        // First resolution for Exact key
        let res1 = seen_bt.entry(exact_key).or_insert_with_key(resolve_mock).clone();
        assert_eq!(res1.locations[0].function, "exact_0x5000");
        assert_eq!(exact_calls.load(Ordering::SeqCst), 1);

        // First resolution for Return key at identical address
        let res2 = seen_bt.entry(return_key).or_insert_with_key(resolve_mock).clone();
        assert_eq!(res2.locations[0].function, "return_0x5000");
        assert_eq!(return_calls.load(Ordering::SeqCst), 1);

        // Second lookup for Exact key must hit cache without invoking mock resolver
        let res3 = seen_bt.entry(exact_key).or_insert_with_key(resolve_mock).clone();
        assert_eq!(res3.locations[0].function, "exact_0x5000");
        assert_eq!(exact_calls.load(Ordering::SeqCst), 1); // count remains 1

        // Second lookup for Return key must hit cache without invoking mock resolver
        let res4 = seen_bt.entry(return_key).or_insert_with_key(resolve_mock).clone();
        assert_eq!(res4.locations[0].function, "return_0x5000");
        assert_eq!(return_calls.load(Ordering::SeqCst), 1); // count remains 1

        assert_eq!(seen_bt.len(), 2);
    }

    #[test]
    fn test_display_module_name_substitutes_process_name_for_anonymous_vmos() {
        // Main executables arrive as anonymous VMO names; use the owning component name.
        assert_eq!(
            display_module_name("<VMO#12345=blob-deadbeef>", Some("my_service.cm")),
            "my_service"
        );
        assert_eq!(display_module_name("blob-deadbeef", Some("netstack3.cm")), "netstack3");
        assert_eq!(display_module_name("blob-deadbeef", Some("sh")), "sh");

        // Shared libraries already carry real names; keep them.
        assert_eq!(display_module_name("libcustom.so", Some("my_service.cm")), "libcustom.so");

        // No usable process name: keep whatever the device reported.
        assert_eq!(
            display_module_name("<VMO#12345=blob-deadbeef>", None),
            "<VMO#12345=blob-deadbeef>"
        );
        assert_eq!(display_module_name("blob-deadbeef", Some(".cm")), "blob-deadbeef");
    }
}
