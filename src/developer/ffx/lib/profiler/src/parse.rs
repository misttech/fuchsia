// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use ffx_symbolize::{AddressType, MappingDetails};
use std::collections::HashMap;
use std::fmt;
use thiserror::Error;

#[derive(Eq, Hash, PartialEq, Clone, Copy, Debug)]
pub struct Pid(pub u64);

impl fmt::Display for Pid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Eq, Hash, PartialEq, Clone, Copy, Debug)]
pub struct Tid(pub u64);

impl fmt::Display for Tid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ModuleDetails {
    pub name: String,
    pub build_id: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModuleWithMmapDetails {
    pub module: ModuleDetails,
    pub mmaps: Vec<MappingDetails>,
}

/// Details of an individual backtrace frame, including its instruction address
/// and address classification.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BacktraceDetails {
    /// Program counter address.
    pub address: u64,
    /// Classification of the address (e.g. exact or return address).
    pub address_type: AddressType,
}

impl BacktraceDetails {
    /// Creates a new `BacktraceDetails` with the given address and type.
    pub fn new(address: u64, address_type: AddressType) -> Self {
        Self { address, address_type }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct BacktraceSample {
    pub timestamp: u64,
    pub frames: Vec<BacktraceDetails>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RawSample {
    pub timestamp: u64,
    pub sample_memory: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ProfilingRecordHandler {
    pub process_name: Option<String>,
    pub module_with_mmap_records: HashMap<u16, ModuleWithMmapDetails>,
    pub backtrace_records: HashMap<Tid, Vec<BacktraceSample>>,
    pub raw_samples: HashMap<Tid, Vec<RawSample>>,
}

#[derive(PartialEq, Debug)]
pub struct UnsymbolizedSamples {
    pub handlers: HashMap<Pid, ProfilingRecordHandler>,
    pub thread_names: HashMap<Tid, String>,
}

#[derive(Error, Debug)]
pub enum SymbolizeError {
    #[error("Failed to load ffx environment context.")]
    NoFfxEnvironmentContext,

    #[error("Failed to open the profiler file due to {}", .0)]
    FileError(#[from] std::io::Error),

    #[error("Failed to create symbolizer due to {}", .0)]
    SymbolizerError(#[from] ffx_symbolize::CreateSymbolizerError),

    #[error("Failed to add mapping due to {}", .0)]
    AddMappingError(#[from] ffx_symbolize::AddMappingError),

    #[error("Failed to convert string to u64 due to {}", .0)]
    HexConvertError(#[from] hex::FromHexError),

    #[error("Encountered an unsupported FXT record type.")]
    UnsupportedFxtRecord,

    #[error("Failed to parse FXT file: {}", .0)]
    FxtParseError(#[from] fxt::ParseError),

    #[error("Received non-profiler FXT record.")]
    NonProfilerFxtRecord,

    #[error("Invalid mapping record.")]
    InvalidMappingRecord,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backtrace_details_construction() {
        let bt_exact = BacktraceDetails::new(0x1000, AddressType::Exact);
        assert_eq!(bt_exact.address, 0x1000);
        assert_eq!(bt_exact.address_type, AddressType::Exact);

        let bt_return = BacktraceDetails::new(0x2000, AddressType::Return);
        assert_eq!(bt_return.address, 0x2000);
        assert_eq!(bt_return.address_type, AddressType::Return);

        let bt_unknown = BacktraceDetails::new(0x3000, AddressType::Unknown);
        assert_eq!(bt_unknown.address, 0x3000);
        assert_eq!(bt_unknown.address_type, AddressType::Unknown);
    }
}
