// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Shared test fixtures: the fake UFS host controller and helpers for driving a
//! [`TransferRequestProcessor`] against it. Tests live alongside the code they exercise.

pub mod fake_ufs;

use crate::constants::ADMIN_COMMAND_SLOT_NUMBER;
use crate::controller::UfsController;
use crate::request_list::{OverallCommandStatus, SlotState};
use crate::scsi::ScsiStatus;
use crate::transfer_request_processor::{IoCompletion, TransferRequestProcessor};
use crate::upiu::{NopInUpiu, NopOutUpiu, ResponseUpiu, UpiuHeader, UpiuTransactionCode};
use fake_bti::FakeBti;
use fake_ufs::{FakeMmio, FakeUtp, wait_until};
use std::sync::Arc;
use zerocopy::byteorder::big_endian::U16;

/// A processor over the fake register block.
pub(crate) type TestProcessor = TransferRequestProcessor<FakeMmio>;

/// Register block size shared by the single-doorbell unit tests.
pub(crate) const MMIO_SIZE: usize = 4096;
/// Page size assumed for test data VMOs.
pub(crate) const PAGE_SIZE: u64 = 4096;
/// The admin command slot.
pub(crate) const ADMIN_SLOT: u8 = ADMIN_COMMAND_SLOT_NUMBER;
/// Overall Command Status value reported by the fake for a successful request.
pub(crate) const OCS_SUCCESS: u8 = OverallCommandStatus::Success as u8;

/// Creates an initialized 32-slot processor and the VMO backing its registers.
pub(crate) fn create_test_processor() -> (zx::Vmo, Arc<TestProcessor>) {
    let bti = FakeBti::create().expect("Failed to create fake bti");
    let vmo = zx::Vmo::create(MMIO_SIZE as u64).unwrap();
    let controller = Arc::new(UfsController::new(FakeMmio::new(&vmo, MMIO_SIZE)));
    let processor = Arc::new(TransferRequestProcessor::new(&bti, controller).unwrap());
    processor.init().unwrap();
    (vmo, processor)
}

/// Maps the fake hardware view of `processor`'s registers and descriptors. Completions raised
/// through [`FakeUtp::complete`] are delivered to `processor` by interrupt.
pub(crate) fn fake_utp(vmo: &zx::Vmo, processor: &Arc<TestProcessor>) -> FakeUtp {
    FakeUtp::new(vmo, MMIO_SIZE, processor)
}

/// Returns the state of `slot`.
pub(crate) fn slot_state(processor: &TestProcessor, slot: u8) -> SlotState {
    processor.request_list.lock().slot(slot).unwrap().state()
}

/// Returns whether `slot` currently holds a pinned client buffer.
pub(crate) fn slot_pinned(processor: &TestProcessor, slot: u8) -> bool {
    processor.request_list.lock().slot(slot).unwrap().pmt.is_some()
}

/// Moves `slot`'s deadline into the past so the next completion pass (or the timeout task) times
/// it out.
pub(crate) fn expire_deadline(processor: &TestProcessor, slot: u8) {
    processor.request_list.lock().slot_mut(slot).unwrap().deadline =
        Some(zx::MonotonicInstant::get() - zx::MonotonicDuration::from_seconds(1));
}

/// Receives the outcome delivered to an [`io_callback`].
pub(crate) type IoOutcomeReceiver = futures::channel::oneshot::Receiver<Result<(), zx::Status>>;

/// A callback [`IoCompletion`] that forwards the outcome to the returned receiver.
pub(crate) fn io_callback() -> (IoCompletion, IoOutcomeReceiver) {
    let (tx, rx) = futures::channel::oneshot::channel();
    let completion = IoCompletion::Callback(Box::new(move |status| {
        let _ = tx.send(status);
    }));
    (completion, rx)
}

/// Returns the outcome already delivered to `rx`, panicking if none was.
pub(crate) fn delivered(rx: &mut IoOutcomeReceiver) -> Result<(), zx::Status> {
    rx.try_recv().expect("Completion dropped").expect("Completion not delivered")
}

/// NOP IN response for `task_tag`.
pub(crate) fn nop_in(task_tag: u8) -> NopInUpiu {
    let mut response = NopOutUpiu::new();
    response.header.trans_type = UpiuTransactionCode::NopIn as u8;
    response.header.task_tag = task_tag;
    response
}

/// RESPONSE UPIU for `task_tag` with SCSI `status`.
pub(crate) fn scsi_response(task_tag: u8, status: u8) -> ResponseUpiu {
    ResponseUpiu {
        header: UpiuHeader {
            trans_type: UpiuTransactionCode::Response as u8,
            task_tag,
            status,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// CHECK CONDITION response with fixed-format sense data.
pub(crate) fn check_condition(task_tag: u8, sense_key: u8) -> ResponseUpiu {
    let mut response = scsi_response(task_tag, ScsiStatus::CheckCondition.as_u8());
    response.sense_data_length = U16::new(18);
    response.sense_data[0] = 0x70;
    response.sense_data[2] = sense_key;
    response.sense_data[7] = 10;
    response
}

/// Returns the lowest slot whose doorbell bit is set, waiting for one.
pub(crate) fn wait_for_any_doorbell(utp: &FakeUtp) -> u8 {
    assert!(wait_until(|| utp.doorbell() != 0), "No doorbell rung");
    utp.doorbell().trailing_zeros() as u8
}
