// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::sync::atomic::Ordering::{Acquire, Relaxed};
use std::sync::atomic::{AtomicU32, AtomicU64};

use fidl_fuchsia_memory_heapdump_client::{self as fheapdump_client, BuildId, CollectorError};
use futures::StreamExt;

use crate::Region;
use anyhow::{Context, Ok, anyhow, bail, ensure};
use log::{info, warn};
use zx::MonotonicInstant;

const SYMBOLIZER_FILE_PATH: &str = "/boot/kernel/i/logs/physboot";

// Header for the profile mapped from kernel memory.
#[repr(C)]
struct BufferHeader {
    // Indicated which version of the layout the profile uses.
    version: AtomicU32,
    // 0 when all allocations and deallocation event have been accounted for.
    // When greater than zero, there was not enough memory allocated to store all statistics.
    event_dropped: AtomicU32,
}
// Aggregates allocation statistics for an given backtrace (collected at the time of allocation).
// It is immediately followed by the backtrace stored as `backtrace_size` virtual memory addressed
// stored as `u64`.
#[repr(C)]
struct BufferEntry {
    // Number of allocations currently in memory.
    live_count: AtomicU64,
    // Amount of allocated bytes currently in memory.
    live_bytes: AtomicU64,
    // Number of allocations since the start of the program.
    total_count: AtomicU64,
    // Amount of allocated bytes since the start fo the system.
    total_bytes: AtomicU64,
    // Number of elements in the backtrace.
    backtrace_size: AtomicU64,
}

#[allow(dead_code)] // TODO(b/330154077): export total_* when supported by the heapdump protocol.
#[derive(Debug, PartialEq)]
struct ProfileEntry {
    // Number of allocation currently in memory.
    live_count: u64,
    // Amount of allocated bytes currently in memory.
    live_bytes: u64,
    // Number of allocation since the start of the program.
    total_count: u64,
    // Amount of allocated bytes since the start of the system.
    total_bytes: u64,
    // Number of element in the backtrace.
    backtrace: Vec<u64>,
}

/// Returns the content located between 3 curly braces, or None.
/// The content is split by semicolon.
fn extract_markup_content(input: &str) -> Option<Vec<&str>> {
    let start = input.find("{{{")?;
    let end = input.find("}}}")?;
    if start >= end {
        return None;
    }
    Some(input[start + 3..end].split(":").collect())
}

/// Returns the integer from an hexadecimal string that starts with "0x".
fn parse_hex(hex_string: &str) -> anyhow::Result<u64> {
    u64::from_str_radix(
        hex_string.strip_prefix("0x").ok_or_else(|| anyhow::anyhow!("0x prefix missing"))?,
        16,
    )
    .context("Invalid hex content")
}

/// Returns a byte array from an hexadecimal string, each pair of digit being turned into an u8.
fn parse_buildid(hex_string: &str) -> anyhow::Result<Vec<u8>> {
    ensure!(hex_string.len() % 2 == 0, "Buildid length should be even");
    (0..hex_string.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex_string[i..i + 2], 16))
        .collect::<Result<Vec<_>, _>>()
        .context("Failed to parse buildid")
}

/// Reads and parse the Kernel memory mapping and returns the ExecutableRegion required for
/// backtraces symbolization.
fn parse_symbolizer_log(
    symbolizer_log_content: &str,
) -> Result<Vec<fheapdump_client::ExecutableRegion>, anyhow::Error> {
    {
        let mut lines = symbolizer_log_content
            .split("\n")
            .filter_map(extract_markup_content)
            .collect::<VecDeque<Vec<&str>>>();

        // Expect the first symbolizer markup element to be "{{{reset}}}¨
        match lines.pop_front().ok_or_else(|| anyhow!("no symbolizer markup found"))?[..] {
            ["reset"] => (),
            _ => bail!("symbolizer markup should start with a reset token"),
        }

        let mut module_idx_to_buildid = HashMap::<&str, Vec<u8>>::new();
        // Expect the list of regions to contain a single executable region.
        let mut regions = Vec::new();
        while let Some(line) = lines.pop_front() {
            match line[..] {
                ["module", module_idx, _kernel_or_physboot, "elf", buildid] => {
                    module_idx_to_buildid.insert(module_idx, parse_buildid(buildid)?);
                }
                ["mmap", start, size, "load", module_idx, mode, rel_addr] if mode.contains("x") => {
                    let start = parse_hex(start)?;
                    let size = parse_hex(size)?;
                    // According to llvm symbolizer markup documentation For ELF files the module
                    // relative address will be the p_vaddr of the associated program header.
                    let rel_addr = parse_hex(rel_addr)?;
                    regions.push(fheapdump_client::ExecutableRegion {
                        address: Some(start),
                        size: Some(size),
                        // Rely on `kernelBase` logic in pprof/internal/elfexec/elfexec.go
                        // if `loadSegment.Vaddr == start - offset` the the base is `offset`
                        //
                        // We want the base to be `start` - `rel_addr`, when this is assigned to
                        // `offset` the condition above is met, and pprof symbolization works.
                        file_offset: Some(start.wrapping_sub(rel_addr)),
                        build_id: Some(BuildId {
                            value: module_idx_to_buildid
                                .get(module_idx)
                                .ok_or_else(|| {
                                    anyhow::anyhow!(
                                        "No module with index {module_idx} could be found"
                                    )
                                })?
                                .clone(),
                        }),
                        vaddr: Some(rel_addr),
                        ..Default::default()
                    })
                }
                _ => (),
            }
        }
        ensure!(!regions.is_empty(), "No executable region could be found.");
        Ok(regions)
    }
    .context("Failed to parse symbol log")
}

struct RegionReader<'a> {
    region: &'a Region,
    position: usize,
}

impl<'a> RegionReader<'a> {
    fn new(region: &'a Region) -> Self {
        Self { region, position: 0 }
    }

    /// # Safety
    /// The caller must ensure that the generic type `T` meets two conditions:
    ///
    /// 1.  T has C-compatible layout, such as `#[repr(C)]`, or primitive types.
    /// 2.  Immutable values can be read directly, while mutable values must be read atomically.
    unsafe fn read_slice<T>(&mut self, count: usize) -> Option<&[T]> {
        // Calculate the total size needed.
        let total_size = std::mem::size_of::<T>() * count;
        let result = self.region.get::<T>(self.position, count)?;
        // Advance the position.
        self.position += total_size;
        Some(result)
    }

    /// # Safety
    /// The caller must ensure that the generic type `T` meets two conditions:
    ///
    /// 1.  T has C-compatible layout, such as `#[repr(C)]`, or primitive types.
    /// 2.  Immutable values can be read directly, while mutable values must be read atomically.
    unsafe fn read<T>(&mut self) -> Option<&T> {
        Some(self.read_slice::<T>(1).map(|s| &s[0])?)
    }
}

fn collect_profile(region: &Region) -> anyhow::Result<Vec<ProfileEntry>> {
    let mut buffer = RegionReader::new(region);
    let mut profile: Vec<ProfileEntry> = vec![];
    // SAFETY: `BufferHeader` is C-compatible and only contains atomics.
    let header = unsafe { buffer.read::<BufferHeader>() }
        .context("buffer underflow while reading header")?;
    let version = header.version.load(Acquire);

    const EXPECTED_VERSION: u32 = 1;
    ensure!(
        version == EXPECTED_VERSION,
        "profiler version mismatch (actual={version} expected={EXPECTED_VERSION})"
    );

    let event_dropped = header.event_dropped.load(Acquire);
    ensure!(event_dropped == 0, "profiler dropped {event_dropped} allocations");

    // SAFETY: `BufferEntry` is C-compatible and only contains atomics.
    while let Some(entry) = unsafe { buffer.read::<BufferEntry>() } {
        // Use `Acquire` memory ordering to ensure the write of backtrace elements happened before
        // the read of a non zero bt_size.
        let bt_size: usize = entry.backtrace_size.load(Acquire).try_into().unwrap();
        ensure!(bt_size < 256, "Stack trace cannot be that large. Buffer corrupted.");
        if bt_size == 0 {
            break;
        }

        let counters = ProfileEntry {
            live_count: entry.live_count.load(Relaxed),
            live_bytes: entry.live_bytes.load(Relaxed),
            total_count: entry.total_count.load(Relaxed),
            total_bytes: entry.total_bytes.load(Relaxed),
            // SAFETY: backtrace is a primitive type and is immutable after write.
            // The write happened before the `entry.backtrace_size` write.
            backtrace: unsafe { buffer.read_slice::<u64>(bt_size) }
                .context("buffer underflow while reading backtrace")?
                .to_vec(),
        };
        profile.push(counters);
    }
    info!(
        "Zircon memory profile size: {} elements, {} KiB (capacity: {} KiB)",
        profile.len(),
        buffer.position / 1024,
        buffer.region.size() / 1024
    );
    Ok(profile)
}

pub struct KernelCollector<'a> {
    region: &'a Region,
}

impl<'a> KernelCollector<'a> {
    pub fn new(region: &'a Region) -> Self {
        Self { region }
    }

    pub async fn serve_client_stream(
        &self,
        mut stream: fheapdump_client::CollectorRequestStream,
    ) -> Result<(), anyhow::Error> {
        while let Some(request) = stream.next().await.transpose()? {
            match request {
                fheapdump_client::CollectorRequest::TakeLiveSnapshot { payload, .. } => {
                    info!("Zircon heap profile snapshot requested");
                    let receiver =
                        payload.receiver.context("missing required receiver")?.into_proxy();

                    match payload.process_selector {
                        Some(fheapdump_client::ProcessSelector::ByKoid(1)) => {
                            // Reads the heap profile from the shared VMO, and build a copy.
                            let mut profile = collect_profile(self.region)?;
                            // Stream the heap profile back to the caller.
                            let mut streamer = heapdump_snapshot::Streamer::new(receiver);
                            let text = fs::read_to_string(SYMBOLIZER_FILE_PATH).context(
                                format!("Read kernel symbol file {SYMBOLIZER_FILE_PATH}"),
                            )?;
                            let regions = parse_symbolizer_log(&text).context(format!(
                                "Read kernel symbol file {SYMBOLIZER_FILE_PATH}"
                            ))?;

                            for region in regions {
                                streamer = streamer
                                    .push_element(
                                        fheapdump_client::SnapshotElement::ExecutableRegion(region),
                                    )
                                    .await?
                            }
                            streamer = streamer
                                .push_element(fheapdump_client::SnapshotElement::ThreadInfo(
                                    fheapdump_client::ThreadInfo {
                                        koid: Some(0),
                                        name: Some("undefined".to_owned()),
                                        thread_info_key: Some(0),
                                        ..Default::default()
                                    },
                                ))
                                .await?;
                            let mut trace_index = 0;
                            while let Some(value) = profile.pop() {
                                streamer = streamer
                                    .push_element(fheapdump_client::SnapshotElement::StackTrace(
                                        fheapdump_client::StackTrace {
                                            stack_trace_key: Some(trace_index),
                                            program_addresses: Some(value.backtrace),
                                            ..Default::default()
                                        },
                                    ))
                                    .await?
                                    .push_element(fheapdump_client::SnapshotElement::Allocation(
                                        fheapdump_client::Allocation {
                                            thread_info_key: Some(0),
                                            timestamp: Some(MonotonicInstant::from_nanos(0)),
                                            address: Some(trace_index),
                                            size: Some(value.live_bytes),
                                            stack_trace_key: Some(trace_index),
                                            ..Default::default()
                                        },
                                    ))
                                    .await?;
                                trace_index += 1;
                            }
                            streamer.end_of_stream().await?
                        }
                        _ => {
                            warn!("Missing process selector");
                            receiver
                                .report_error(CollectorError::ProcessSelectorUnsupported)
                                .await
                                .context("reporting error")?
                        }
                    };
                }
                fheapdump_client::CollectorRequest::ListStoredSnapshots { .. } => {
                    bail!("Not supported by kernel collector.")
                }
                fheapdump_client::CollectorRequest::DownloadStoredSnapshot { .. } => {
                    bail!("Not supported by kernel collector.")
                }
                fheapdump_client::CollectorRequest::_UnknownMethod { ordinal, .. } => {
                    bail!("Unknown CollectorRequest ordinal: {}", ordinal);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::mem;

    use zx::Vmo;

    use super::*;

    #[test]
    fn test_parse_hex() {
        assert!(parse_hex("ab").is_err(), "0x prefix missing");
        assert!(parse_hex("0ab").is_err(), "0x prefix missing");
        assert!(parse_hex("xab").is_err(), "0x prefix missing");
        assert!(parse_hex("ab0x").is_err(), "0x prefix missing");
        assert!(parse_hex("0Xab").is_err(), "0x prefix missing");

        assert_eq!(parse_hex("0xab").unwrap(), 0xab);
        assert_eq!(parse_hex("0xab0").unwrap(), 0xab0 as u64);
        assert_eq!(parse_hex("0xab00").unwrap(), 0xab00 as u64);
        assert_eq!(parse_hex("0xAB").unwrap(), 0xab as u64);

        assert!(parse_hex("0xABCDEFGHI").is_err(), "G is not a valid hex");
    }

    #[test]
    fn test_parse_buildid() {
        assert!(parse_buildid("a").is_err(), "Buildid length should be even");
        assert!(parse_buildid("0xba").is_err(), "0x should not be present");

        assert_eq!(parse_buildid("ab").unwrap(), vec![0xab]);
        assert_eq!(parse_buildid("abcd").unwrap(), vec![0xab, 0xcd]);
        assert_eq!(parse_buildid("abcdef").unwrap(), vec![0xab, 0xcd, 0xef]);
        assert_eq!(parse_buildid("AB").unwrap(), vec![0xab]);
    }

    #[test]
    fn test_extract_markup_content() {
        assert_eq!(extract_markup_content("a"), None);
        assert_eq!(extract_markup_content("b{{{a"), None);
        assert_eq!(extract_markup_content("b}}}a"), None);
        assert_eq!(extract_markup_content("a}}}b{{{a"), None);
        assert_eq!(extract_markup_content("a{{{a}}"), None);
        assert_eq!(extract_markup_content("a{{b}}}c"), None);
        assert_eq!(extract_markup_content("a{{b{c}}}d"), None);
        assert_eq!(extract_markup_content("a{{{c}c}}d"), None);
        assert_eq!(extract_markup_content("a{{{}}}d"), Some(vec![""]));
        assert_eq!(extract_markup_content("a{{{c}}}d"), Some(vec!["c"]));
        assert_eq!(extract_markup_content("a{{{:c: }}}d"), Some(vec!["", "c", " "]));
        assert_eq!(extract_markup_content("{{{:c: }}}d"), Some(vec!["", "c", " "]));
        assert_eq!(extract_markup_content("a{{{:c: }}}"), Some(vec!["", "c", " "]));
        assert_eq!(extract_markup_content("{{{:c: }}}"), Some(vec!["", "c", " "]));
        assert_eq!(extract_markup_content("a{{{c}}}d{{{e}}}"), Some(vec!["c"]));
    }

    #[test]
    fn test_parse_symbolizer_log() {
        assert_eq!(parse_symbolizer_log("
            {{{reset}}}
            {{{module:0:kernel:elf:001c122d7c44434165b2e75e9876db2650817d5a}}}
            {{{mmap:0xffffffff00100000:0x326d60:load:0:rx:0xffffffff80100000}}}
            {{{mmap:0xffffffff00427000:0xb1000:load:0:r:0xffffffff80427000}}}
            {{{mmap:0xffffffff004d8000:0x95a8:load:0:rw:0xffffffff804d8000}}}
            {{{mmap:0xffffffff004e2000:0x504000:load:0:rw:0xffffffff804e2000}}}
            Memory profile: {{{dumpfile:memory-profile:i/memory-profile/d/heap.bin}}} maximum 4194304 bytes.
            ").unwrap(), vec![ fheapdump_client::ExecutableRegion {
            address: Some(0xffffffff00100000),
            size: Some(0x326d60),
            file_offset: Some(0xffffffff80000000),
            build_id: Some(BuildId {
                value: vec![0x00,0x1c,0x12,0x2d,0x7c,0x44,0x43,0x41,0x65,0xb2,0xe7,0x5e,0x98,0x76,0xdb,0x26,0x50,0x81,0x7d,0x5a]}),

            ..Default::default()
        }]);

        // Reset markup missing
        assert!(parse_symbolizer_log("
            {{{module:0:kernel:elf:001c122d7c44434165b2e75e9876db2650817d5a}}}
            {{{mmap:0xffffffff00100000:0x326d60:load:0:rx:0xffffffff80100000}}}
            {{{mmap:0xffffffff00427000:0xb1000:load:0:r:0xffffffff80427000}}}
            {{{mmap:0xffffffff004d8000:0x95a8:load:0:rw:0xffffffff804d8000}}}
            {{{mmap:0xffffffff004e2000:0x504000:load:0:rw:0xffffffff804e2000}}}
            Memory profile: {{{dumpfile:memory-profile:i/memory-profile/d/heap.bin}}} maximum 4194304 bytes.
            ").is_err());

        // Module idx mismatch
        assert!(parse_symbolizer_log("
            {{{reset}}}
            {{{module:1:kernel:elf:001c122d7c44434165b2e75e9876db2650817d5a}}}
            {{{mmap:0xffffffff00100000:0x326d60:load:0:rx:0xffffffff80100000}}}
            {{{mmap:0xffffffff00427000:0xb1000:load:0:r:0xffffffff80427000}}}
            {{{mmap:0xffffffff004d8000:0x95a8:load:0:rw:0xffffffff804d8000}}}
            {{{mmap:0xffffffff004e2000:0x504000:load:0:rw:0xffffffff804e2000}}}
            Memory profile: {{{dumpfile:memory-profile:i/memory-profile/d/heap.bin}}} maximum 4194304 bytes.
            ").is_err());

        // No executable region
        assert!(parse_symbolizer_log("
            {{{reset}}}
            {{{module:0:kernel:elf:001c122d7c44434165b2e75e9876db2650817d5a}}}
            {{{mmap:0xffffffff00100000:0x326d60:load:0:r:0xffffffff80100000}}}
            {{{mmap:0xffffffff00427000:0xb1000:load:0:r:0xffffffff80427000}}}
            {{{mmap:0xffffffff004d8000:0x95a8:load:0:rw:0xffffffff804d8000}}}
            {{{mmap:0xffffffff004e2000:0x504000:load:0:rw:0xffffffff804e2000}}}
            Memory profile: {{{dumpfile:memory-profile:i/memory-profile/d/heap.bin}}} maximum 4194304 bytes.
            ").is_err());
    }

    #[test]
    fn test_collect_profile() {
        fn to_region(input: &[u64]) -> Region {
            let bytes: Vec<u8> = input.iter().flat_map(|&n| n.to_ne_bytes()).collect();
            let vmo = Vmo::create(bytes.len().try_into().unwrap()).unwrap();
            vmo.write(&bytes, 0).unwrap();
            Region::new(&vmo).unwrap()
        }

        assert_eq!(
            collect_profile(&to_region(&[0x0])).unwrap_err().to_string(),
            "profiler version mismatch (actual=0 expected=1)"
        );
        assert_eq!(
            collect_profile(&to_region(&[0x00000007_00000001])).unwrap_err().to_string(),
            "profiler dropped 7 allocations"
        );

        // Empty profiles.
        const HEADER: u64 = 0x00000000_00000001;
        assert_eq!(collect_profile(&to_region(&[HEADER])).unwrap(), vec![]);
        assert_eq!(
            collect_profile(&to_region(&[
                HEADER, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0
            ]))
            .unwrap(),
            vec![]
        );

        // Profile with one element.
        assert_eq!(
            collect_profile(&to_region(&[
                HEADER, /*BufferEntry*/ 10, 20, 30, 40, 4, /*backtrace*/ 1, 2, 3, 4
            ]))
            .unwrap(),
            vec![ProfileEntry {
                live_count: 10,
                live_bytes: 20,
                total_count: 30,
                total_bytes: 40,
                backtrace: vec![1, 2, 3, 4]
            }]
        );
        assert_eq!(
            collect_profile(&to_region(&[
                HEADER, /*BufferEntry*/ 10, 20, 30, 40, 4, /*backtrace*/ 1, 2, 3, 4,
                /* zeros */ 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0
            ]))
            .unwrap(),
            vec![ProfileEntry {
                live_count: 10,
                live_bytes: 20,
                total_count: 30,
                total_bytes: 40,
                backtrace: vec![1, 2, 3, 4]
            }]
        );
        assert_eq!(
            collect_profile(&to_region(&[
                HEADER, /*BufferEntry*/ 10, 20, 30, 40, 4, /*backtrace*/ 1, 2, 3, 4,
                /*zeros*/ 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0
            ]))
            .unwrap(),
            vec![ProfileEntry {
                live_count: 10,
                live_bytes: 20,
                total_count: 30,
                total_bytes: 40,
                backtrace: vec![1, 2, 3, 4]
            }]
        );

        // Backtrace underflow
        let assumed_buffer_size = zx::system_get_page_size() as usize;
        let data = {
            let assumed_buffer_size_in_u64 = assumed_buffer_size / mem::size_of::<u64>();
            let mut data = vec![HEADER];
            const ENTRY_SIZE_BYTES: usize = 256;
            const BACKTRACE_SIZE: usize =
                (ENTRY_SIZE_BYTES - mem::size_of::<BufferHeader>()) / mem::size_of::<u64>();
            const ENTRY: [u64; 5] = [10u64, 20, 30, 40, BACKTRACE_SIZE as u64];
            while data.len() < assumed_buffer_size_in_u64 {
                data.extend_from_slice(&ENTRY);
                data.extend_from_slice(&[0; BACKTRACE_SIZE]);
            }
            assert_ne!(
                data.len(),
                assumed_buffer_size_in_u64,
                "Test internal error, if it fits perfectly, it is not possible to test underflow"
            );
            data.truncate(assumed_buffer_size_in_u64);
            data
        };
        let region = to_region(&data);
        assert_eq!(
            region.size(),
            assumed_buffer_size,
            "The mapping is assumed to be rounded up to 1 page"
        );
        assert_eq!(
            collect_profile(&region).unwrap_err().to_string(),
            "buffer underflow while reading backtrace"
        );

        // Profile with two elements
        assert_eq!(
            collect_profile(&to_region(&[
                HEADER, /*BufferEntry*/ 100, 200, 300, 400, 4, /*backtrace*/ 10, 20, 30,
                40, /*BufferEntry*/ 10, 20, 30, 40, 4, /*backtrace*/ 1, 2, 3, 4,
                /*zero*/ 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0
            ]))
            .unwrap(),
            vec![
                ProfileEntry {
                    live_count: 100,
                    live_bytes: 200,
                    total_count: 300,
                    total_bytes: 400,
                    backtrace: vec![10, 20, 30, 40]
                },
                ProfileEntry {
                    live_count: 10,
                    live_bytes: 20,
                    total_count: 30,
                    total_bytes: 40,
                    backtrace: vec![1, 2, 3, 4]
                },
            ]
        );
    }
}
