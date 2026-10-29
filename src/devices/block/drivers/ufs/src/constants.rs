// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Protocol, register, descriptor, and driver constants for UFS 3.1/4.0 and UFSHCI 3.0/4.0.

// --- UFSHCI register, UTRD, UCD, and transfer processor constants (UFSHCI 3.0/4.0 sec 5 - 7) ---

/// Byte granularity of `MCQCAP.QCFGPTR`: the per-queue configuration block starts at
/// `QCFGPTR * 0x200` (UFSHCI 4.0 sections 5.2.2 and 5.2.9).
pub const MCQ_QUEUE_CONFIG_PTR_UNIT: usize = 0x200;
/// Time allowed for `HCE` to toggle on reset/enable and for a UIC command issued during link
/// startup to complete (UFSHCI 3.0/4.0 sections 5.3.3, 5.3.4, and 7.1.1).
pub const HOST_CONTROLLER_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(10);
/// Number of `DME_LINKSTARTUP` attempts before bring-up or recovery gives up (UFSHCI 3.0/4.0
/// section 7.1.1 allows the first attempt to fail while the device is still booting).
pub const LINK_STARTUP_ATTEMPTS: u32 = 3;
/// Delay between [`LINK_STARTUP_ATTEMPTS`] so the device can finish its own link startup.
pub const LINK_STARTUP_RETRY_DELAY: zx::MonotonicDuration = zx::MonotonicDuration::from_millis(10);
/// Largest `AHIT.AH8ITV` (10-bit idle timer value, UFSHCI 3.0 section 5.2.5).
pub const AUTO_HIBERNATE_MAX_TIMER_VALUE: u32 = 0x3FF;
/// Largest `AHIT.TS` timer scale (`5` selects 100 ms steps, UFSHCI 3.0 section 5.2.5).
pub const AUTO_HIBERNATE_MAX_TIMER_SCALE: u32 = 5;
/// `UECPA.EC` error code for `PA_LINE_RESET`, which is reported but not fatal (UFSHCI 3.0/4.0
/// section 5.3.5).
pub const UIC_PHY_ADAPTER_LAYER_LINE_RESET: u32 = 0x10;

/// Largest transfer request list the driver tracks; bounds the per-slot state arrays (`UTRLBA`
/// list, UFSHCI 3.0/4.0 section 5.4.3).
pub const MAX_REQUEST_LIST_SIZE: u8 = 64;
/// Transfer slots in single-doorbell mode: one per `UTRLDBR` bit (UFSHCI 3.0/4.0 section 5.4.3).
pub const SDB_SLOTS: usize = 32;
/// Slot reserved for Admin/Query UPIUs so block I/O cannot starve control commands.
pub const ADMIN_COMMAND_SLOT_NUMBER: u8 = (SDB_SLOTS - 1) as u8;
/// `UTRD.CT` command type for UFS storage commands (UFSHCI 3.0/4.0 section 6.1.1).
pub const COMMAND_TYPE_UFS_STORAGE: u8 = 0x1;
/// Byte offset of dword 2 (`OCS`) inside a UTRD, polled directly after a posted completion
/// (UFSHCI 3.0/4.0 section 6.1.1).
pub const OCS_OFFSET: usize = 8;
/// `UTRD.I` interrupt bit: request a completion interrupt for this slot (UFSHCI 3.0/4.0 section
/// 6.1.1).
pub const UTRD_INTERRUPT_BIT: u32 = 1 << 24;
/// Bit position of `UTRD.DD` (data direction) in dword 0 (UFSHCI 3.0/4.0 section 6.1.1).
pub const UTRD_DATA_DIRECTION_SHIFT: u32 = 25;
/// Width mask of the 2-bit `UTRD.DD` field (UFSHCI 3.0/4.0 section 6.1.1).
pub const UTRD_DATA_DIRECTION_MASK: u32 = 0x3;
/// Bit position of `UTRD.CT` (command type) in dword 0 (UFSHCI 3.0/4.0 section 6.1.1).
pub const UTRD_COMMAND_TYPE_SHIFT: u32 = 28;
/// Width mask of the 4-bit `UTRD.CT` field (UFSHCI 3.0/4.0 section 6.1.1).
pub const UTRD_COMMAND_TYPE_MASK: u32 = 0xF;
/// Mask of the 8-bit `UTRD.OCS` (overall command status) field in dword 2 (UFSHCI 3.0/4.0
/// section 6.1.1).
pub const UTRD_OCS_MASK: u32 = 0xFF;
/// Size of one UTP Command Descriptor: command UPIU, response UPIU, and PRDT (UFSHCI 3.0/4.0
/// section 6.1.2). 8 KiB leaves room for [`MAX_PRDT_ENTRIES`] plus the inline data tail.
pub const UTP_COMMAND_DESCRIPTOR_SIZE: usize = 8192;
/// Size of the inline data tail at the end of each UCD, used to DMA small `HostToDevice`
/// payloads (such as `UNMAP` parameter lists) without pinning a client VMO.
pub const INLINE_DATA_SIZE: usize = 64;
/// Offset of the inline data tail inside a UCD.
pub const INLINE_DATA_OFFSET: usize = UTP_COMMAND_DESCRIPTOR_SIZE - INLINE_DATA_SIZE;
/// Largest PRDT the driver builds per command; with 4 KiB pages this bounds a transfer to 1 MiB
/// (UFSHCI 3.0/4.0 section 6.1.2, `PRDTL`).
pub const MAX_PRDT_ENTRIES: usize = 256;
/// Largest byte count of one PRDT entry: the 18-bit `DBC` field holds `bytes - 1` (UFSHCI 3.0/4.0
/// section 6.1.2).
pub const MAX_PRDT_ENTRY_BYTES: u32 = 256 * 1024;
/// Mask of the 18-bit PRDT `DBC` (data byte count minus one) field (UFSHCI 3.0/4.0 section
/// 6.1.2).
pub const PRDT_DBC_MASK: u32 = MAX_PRDT_ENTRY_BYTES - 1;
/// Low bits that must be clear in PRDT addresses and byte counts, which are dword granular
/// (UFSHCI 3.0/4.0 section 6.1.2).
pub const DWORD_ALIGN_MASK: u64 = 0x3;

/// Time a UTP transfer request may stay outstanding before the stall watchdog logs an error.
pub const COMMAND_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(10);
/// Spin re-reads of `UTRD.OCS` after a completion is signalled, covering the window in which the
/// controller's posted write of the UTRD has not yet landed in memory.
pub const OCS_RETRIES: u32 = 100;
/// Retries of a SCSI command that completes with `UNIT ATTENTION`, `NOT READY`, `BUSY` or
/// `TASK SET FULL` (SAM-5 section 5.3).
pub const SCSI_RETRIES: u32 = 3;
/// Delay before each [`SCSI_RETRIES`] attempt.
pub const SCSI_RETRY_DELAY: zx::MonotonicDuration = zx::MonotonicDuration::from_millis(5);
/// Largest transfer, in pages, whose PRDT and physical address list are built on the stack
/// instead of in a heap allocation.
pub const SMALL_TRANSFER_PAGES: usize = 32;

// --- MIPI UniPro / UIC DME constants (UniPro v1.8/v2.0, UFSHCI 3.0/4.0 section 5.6) ---

/// `PA_TxGear`: TX gear requested for the next power mode change (MIPI UniPro v2.0 section
/// 5.8.1).
pub const PA_TXGEAR: u16 = 0x1568;
/// `PA_PWRMode`: writing it triggers the power mode change for both directions (MIPI UniPro v2.0
/// section 5.8.3).
pub const PA_PWRMODE: u16 = 0x1571;
/// `PA_MaxRxHSGear`: highest HS gear the local (or, via `DME_PEER_GET`, the peer) receiver
/// supports (MIPI UniPro v2.0 section 5.8.2).
pub const PA_MAXRXHSGEAR: u16 = 0x1587;

/// Sleep between register polls (`HCE`, `UCRDY`, UIC completion, doorbells) once the spin
/// budget is exhausted.
pub const REGISTER_POLL_INTERVAL: zx::MonotonicDuration = zx::MonotonicDuration::from_micros(10);
/// Time a register poll spins before it starts sleeping [`REGISTER_POLL_INTERVAL`] between
/// reads, so fast completions are not delayed by a timer.
pub const SPIN_POLL_BUDGET: zx::MonotonicDuration = zx::MonotonicDuration::from_micros(10);

// --- UPIU, Query, Descriptor, and Device Manager constants (UFS 3.1/4.0 section 10 - 14) ---

/// Mask of the 6-bit transaction code in UPIU byte 0; bits 7:6 are the HD and DD flags (UFS
/// 3.1/4.0 section 10.6.2).
pub const TRANSACTION_CODE_MASK: u8 = 0x3F;
/// COMMAND UPIU header flag `W`: the command has a Data-Out phase (UFS 3.1/4.0 section 10.7.1).
pub const FLAG_WRITE: u8 = 0x20;
/// COMMAND UPIU header flag `R`: the command has a Data-In phase (UFS 3.1/4.0 section 10.7.1).
pub const FLAG_READ: u8 = 0x40;
/// RESPONSE UPIU header flag `O`: residual overflow (UFS 3.1/4.0 section 10.7.2).
pub const FLAG_OVERFLOW: u8 = 0x40;
/// RESPONSE UPIU header flag `U`: residual underflow (UFS 3.1/4.0 section 10.7.2).
pub const FLAG_UNDERFLOW: u8 = 0x20;
/// Size of the Query UPIU header plus transaction-specific fields, excluding the data segment
/// (UFS 3.1/4.0 section 10.7.8).
pub const QUERY_HEADER_SIZE: usize = 32;
/// Size of the Query UPIU data segment, which must hold the largest descriptor (UFS 3.1/4.0
/// section 10.7.8).
pub const QUERY_DATA_SEGMENT_SIZE: usize = 256;
/// Size of the `bLength` + `bDescriptorIDN` header that starts every descriptor (UFS 3.1/4.0
/// section 14.1); a returned descriptor shorter than this cannot be validated.
pub const DESCRIPTOR_HEADER_SIZE: usize = 2;

/// Smallest logical block size exposed to the block stack (UFS 3.1/4.0 section 14.1.4.5
/// requires `bLogicalBlockSize >= 12`).
pub const MIN_LOGICAL_BLOCK_SIZE: u32 = 4096;
/// Unit of Geometry Descriptor `bMinAddrBlockSize`, `bMaxInBufferSize` and `bMaxOutBufferSize`
/// (UFS 3.1/4.0 section 14.1.4.4).
pub const GEOMETRY_SIZE_UNIT: u32 = 512;

// --- SCSI command set, sense, and RPMB constants (SPC-4 / SBC-3 / UFS 3.1/4.0 sec 11 - 12) ---

/// Mask of the sense data `RESPONSE CODE` in byte 0; bit 7 is `VALID` (SPC-4 section 4.5).
pub const SENSE_RESPONSE_CODE_MASK: u8 = 0x7F;
/// Sense `RESPONSE CODE` for descriptor-format current errors, whose sense key is in byte 1
/// rather than byte 2 (SPC-4 section 4.5.2).
pub const SENSE_DESCRIPTOR_FORMAT_CURRENT: u8 = 0x72;
/// Sense `RESPONSE CODE` for descriptor-format deferred errors (SPC-4 section 4.5.2).
pub const SENSE_DESCRIPTOR_FORMAT_DEFERRED: u8 = 0x73;
/// SCSI LUN prefix of the well-known logical unit addressing method (SAM-5 section 4.7.8).
pub const SCSI_WELL_KNOWN_LUN_ID: u16 = 0xC100;
/// Mask selecting the addressing method byte of a 16-bit SCSI LUN (SAM-5 section 4.7.8).
pub const SCSI_WELL_KNOWN_LUN_MASK: u16 = 0xFF00;
/// Bit set in the UPIU LUN field to address a well-known LU (UFS 3.1/4.0 section 10.8.5).
pub const UFS_WELL_KNOWN_LUN_ID: u8 = 0x80;
/// Highest regular LUN a UFS device can expose (UFS 3.1/4.0 section 10.8.5 allows 32 LUs).
pub const MAX_LUN_INDEX: u16 = 31;
/// `SERVICE ACTION IN (16)` service action selecting `READ CAPACITY (16)` (SBC-3 section 5.16).
pub const SERVICE_ACTION_READ_CAPACITY_16: u8 = 0x10;
/// `FUA` bit in byte 1 of `READ`/`WRITE (10)/(16)` CDBs (SBC-3 sections 5.11 and 5.33).
pub const CDB_FUA_BIT: u8 = 0x08;
/// `SECURITY PROTOCOL` value for UFS RPMB in `SECURITY PROTOCOL IN/OUT` (UFS 3.1/4.0 section
/// 12.2).
pub const UFS_SECURITY_PROTOCOL: u8 = 0xEC;
/// `SECURITY PROTOCOL SPECIFIC` value selecting RPMB region 0 (UFS 3.1/4.0 section 12.2).
pub const RPMB_SECURITY_PROTOCOL_SPECIFIC: u16 = 0x0001;
