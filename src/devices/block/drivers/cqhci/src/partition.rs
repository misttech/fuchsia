// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::borrow::Cow;
use std::num::NonZero;
use std::sync::Arc;

use crate::command_queue::CommandQueue;
use crate::partition_name;
use block_server::callback_interface::{Interface, Request, Session, SessionManager};
use block_server::{DeviceInfo, PartitionInfo};
use fidl_fuchsia_storage_block::{BlockInfo, MAX_TRANSFER_UNBOUNDED};
use fidl_next_fuchsia_hardware_cqhci::EmmcPartitionId;

pub struct EmmcPartition {
    partition: EmmcPartitionId,
    device_info: DeviceInfo,
    command_queue: std::sync::Weak<CommandQueue>,
}

impl EmmcPartition {
    pub fn new(
        partition: EmmcPartitionId,
        command_queue: std::sync::Weak<CommandQueue>,
        block_info: BlockInfo,
    ) -> Self {
        Self {
            partition,
            device_info: DeviceInfo::Partition(PartitionInfo {
                device_flags: block_info.flags,
                block_range: Some(0..block_info.block_count),
                max_transfer_blocks: if block_info.max_transfer_size != MAX_TRANSFER_UNBOUNDED {
                    NonZero::new(block_info.max_transfer_size / block_info.block_size)
                } else {
                    None
                },
                type_guid: [0u8; 16],
                instance_guid: [0u8; 16],
                name: partition_name(partition).to_string(),
                flags: 0,
            }),
            command_queue,
        }
    }
}

impl Interface for EmmcPartition {
    type Orchestrator = SessionManager<Self>;

    fn get_info(&self) -> Cow<'_, DeviceInfo> {
        Cow::Borrowed(&self.device_info)
    }

    fn spawn_session(&self, session: Arc<Session<Self>>) {
        std::thread::spawn(move || {
            if let Err(err) = fuchsia_scheduler::set_role_for_this_thread(
                "fuchsia.devices.block.drivers.sdmmc.worker",
            ) {
                log::warn!(err:?; "Failed to set thread role");
            }
            session.run();
        });
    }

    fn on_requests(&self, requests: &[Request]) {
        let Some(command_queue) = self.command_queue.upgrade() else {
            return;
        };
        for request in requests {
            let request_id = request.request_id;
            let trace_flow_id = request.trace_flow_id;
            match &request.operation {
                block_server::Operation::Read {
                    device_block_offset,
                    block_count,
                    _unused,
                    vmo_offset,
                    // TODO(https://fxbug.dev/42176727): Use read options
                    options: _,
                } => command_queue.submit_read(
                    self.partition,
                    request_id,
                    *device_block_offset,
                    *block_count,
                    request.vmo.as_ref().unwrap().clone(),
                    *vmo_offset,
                    trace_flow_id,
                ),
                block_server::Operation::Write {
                    device_block_offset,
                    block_count,
                    _unused,
                    vmo_offset,
                    options,
                } => command_queue.submit_write(
                    self.partition,
                    request_id,
                    *device_block_offset,
                    *block_count,
                    request.vmo.as_ref().unwrap().clone(),
                    *vmo_offset,
                    *options,
                    trace_flow_id,
                ),
                block_server::Operation::Flush => {
                    command_queue.submit_flush(self.partition, request_id, trace_flow_id)
                }
                block_server::Operation::Trim { device_block_offset, block_count } => command_queue
                    .submit_trim(
                        self.partition,
                        request_id,
                        *device_block_offset,
                        *block_count,
                        trace_flow_id,
                    ),
                block_server::Operation::CloseVmo => {
                    unreachable!()
                }
                block_server::Operation::StartDecompressedRead { .. } => {
                    unimplemented!()
                }
                block_server::Operation::ContinueDecompressedRead { .. } => {
                    unimplemented!()
                }
            };
        }
    }
}
