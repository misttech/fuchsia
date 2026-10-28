// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Universal Flash Storage (UFS 3.1/4.0) wire protocol, DMA queues, UIC, and transfer engine.

pub mod constants;
pub mod controller;
pub mod descriptors;
pub mod dma_buffer;
pub mod query;
pub mod registers;
pub mod request_list;
pub mod scsi;
pub mod transfer;
pub mod transfer_request_processor;
pub mod uic;
pub mod upiu;

/// Standard UFS driver error representation mapping to Zircon `zx::Status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum UfsError {
    /// Requested transfer slot index is out of bounds.
    #[error("Invalid transfer slot: {0}")]
    InvalidSlot(u32),
    /// Controller, UIC, or UTP operation timed out.
    #[error("UFS operation timed out")]
    Timeout,
    /// UIC command failed with the given opcode and result code.
    #[error("UIC command {opcode:#x} failed with code {code:#x}")]
    UicError {
        /// UIC command opcode (`DME_*`).
        opcode: u8,
        /// Hardware result or power mode status code.
        code: u32,
    },
    /// Target device returned SCSI `CHECK CONDITION`.
    #[error("SCSI Check Condition")]
    CheckCondition,
    /// General hardware or transport I/O error.
    #[error("UFS I/O error")]
    IoError,
    /// Caller supplied an out-of-range or malformed parameter.
    #[error("Invalid parameter")]
    InvalidParameter,
    /// All transfer request slots are currently in use.
    #[error("No transfer slot resources available")]
    NoResources,
}

impl From<UfsError> for zx::Status {
    fn from(err: UfsError) -> Self {
        match err {
            UfsError::InvalidSlot(_) => zx::Status::OUT_OF_RANGE,
            UfsError::Timeout => zx::Status::TIMED_OUT,
            UfsError::UicError { .. } => zx::Status::INTERNAL,
            UfsError::CheckCondition => zx::Status::IO_REFUSED,
            UfsError::IoError => zx::Status::IO,
            UfsError::InvalidParameter => zx::Status::INVALID_ARGS,
            UfsError::NoResources => zx::Status::NO_RESOURCES,
        }
    }
}

#[cfg(test)]
mod testing;

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    fn test_ufs_error_to_status() {
        assert_eq!(zx::Status::from(UfsError::InvalidSlot(40)), zx::Status::OUT_OF_RANGE);
        assert_eq!(zx::Status::from(UfsError::Timeout), zx::Status::TIMED_OUT);
        assert_eq!(zx::Status::from(UfsError::CheckCondition), zx::Status::IO_REFUSED);
        assert_eq!(zx::Status::from(UfsError::IoError), zx::Status::IO);
        assert_eq!(zx::Status::from(UfsError::InvalidParameter), zx::Status::INVALID_ARGS);
        assert_eq!(zx::Status::from(UfsError::NoResources), zx::Status::NO_RESOURCES);
    }

    #[fuchsia::test]
    fn test_ufs_error_display() {
        assert_eq!(UfsError::InvalidSlot(7).to_string(), "Invalid transfer slot: 7");
        assert_eq!(
            UfsError::UicError { opcode: 0x16, code: 0x1 }.to_string(),
            "UIC command 0x16 failed with code 0x1"
        );
    }
}
