// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

mod bti;
pub mod buffer_chain;
mod bus_transaction_initiator_dispatcher;
#[allow(improper_ctypes)]
mod bus_transaction_initiator_dispatcher_ffi;
mod channel_dispatcher;
mod channel_dispatcher_ffi;
mod clock_dispatcher;
mod clock_dispatcher_ffi;
mod counter_dispatcher;
#[allow(improper_ctypes)]
mod counter_dispatcher_ffi;
pub mod diagnostics;
#[cfg(console_enabled)]
mod diagnostics_ffi;
mod dispatcher;
#[allow(improper_ctypes)]
mod dispatcher_ffi;
mod event_dispatcher;
#[allow(improper_ctypes)]
mod event_dispatcher_ffi;
mod event_pair_dispatcher;
#[allow(improper_ctypes)]
mod event_pair_dispatcher_ffi;
mod exception;
mod exception_dispatcher;
mod exception_dispatcher_ffi;
mod exception_ffi;
mod exceptionate;
#[allow(improper_ctypes)]
mod exceptionate_ffi;
mod executor;
mod fifo_dispatcher;
#[allow(improper_ctypes)]
mod fifo_dispatcher_ffi;
mod futex_context;
mod futex_context_ffi;
mod guest;
mod guest_dispatcher;
#[allow(improper_ctypes)]
mod guest_dispatcher_ffi;
mod handle;
mod handle_table;
mod interrupt_dispatcher;
mod interrupt_dispatcher_ffi;
mod interrupt_event_dispatcher;
mod io_buffer_dispatcher;
#[allow(improper_ctypes)]
mod io_buffer_dispatcher_ffi;
mod io_buffer_shared_region_dispatcher;
mod iommu;
mod iommu_dispatcher;
#[allow(improper_ctypes)]
mod iommu_dispatcher_ffi;
mod job_dispatcher;
#[allow(improper_ctypes)]
mod job_dispatcher_ffi;
mod job_policy;
mod job_policy_ffi;
mod log_dispatcher;
#[allow(improper_ctypes)]
mod log_dispatcher_ffi;
mod mbuf;
mod memory_watchdog;
pub mod message_packet;
mod msi_allocation;
mod msi_dispatcher;
#[allow(improper_ctypes)]
mod msi_dispatcher_ffi;
mod msi_interrupt_dispatcher;
mod pager_dispatcher;
#[allow(improper_ctypes)]
mod pager_dispatcher_ffi;
mod pinned_memory_token_dispatcher;
#[allow(improper_ctypes)]
mod pinned_memory_token_dispatcher_ffi;
mod pmt;
#[allow(improper_ctypes)]
mod port_dispatcher;
mod port_dispatcher_ffi;
mod process_dispatcher;
#[allow(improper_ctypes)]
pub(crate) mod process_dispatcher_ffi;
mod profile_dispatcher;
#[allow(improper_ctypes)]
mod profile_dispatcher_ffi;
mod resource;
mod resource_dispatcher;
mod resource_dispatcher_ffi;
#[allow(improper_ctypes)]
mod resource_ffi;
mod root_job_observer;
mod root_job_observer_ffi;
mod sampler_dispatcher;
#[allow(improper_ctypes)]
mod sampler_dispatcher_ffi;
mod socket_dispatcher;
#[allow(improper_ctypes)]
mod socket_dispatcher_ffi;
mod stream_dispatcher;
#[allow(improper_ctypes)]
mod stream_dispatcher_ffi;
mod suspend_token_dispatcher;
#[allow(improper_ctypes)]
mod suspend_token_dispatcher_ffi;
mod thread_dispatcher;
#[allow(improper_ctypes)]
mod thread_dispatcher_ffi;
mod thread_state;
mod timer_dispatcher;
#[allow(improper_ctypes)]
mod timer_dispatcher_ffi;
mod user_handles;
mod vcpu;
mod vcpu_dispatcher;
#[allow(improper_ctypes)]
mod vcpu_dispatcher_ffi;
mod virtual_interrupt_dispatcher;
mod vm_address_region_dispatcher;
#[allow(improper_ctypes)]
mod vm_address_region_dispatcher_ffi;
mod vm_object_dispatcher;
#[allow(improper_ctypes)]
mod vm_object_dispatcher_ffi;
mod wait_signal_observer;

pub use bti::{IOMMU_FLAG_PERM_EXECUTE, IOMMU_FLAG_PERM_READ, IOMMU_FLAG_PERM_WRITE};
pub use buffer_chain::BufferChain;
pub use bus_transaction_initiator_dispatcher::BusTransactionInitiatorDispatcher;
pub use channel_dispatcher::ChannelDispatcher;
pub use clock_dispatcher::{ClockDispatcher, ClockUpdateArgs};
pub use counter_dispatcher::CounterDispatcher;
pub use diagnostics::{
    VmarMapsInfoWriter, VmoInfoWriter, get_process_vmos, get_vm_aspace_maps, get_vm_aspace_vmos,
    get_vmar_maps,
};
pub use dispatcher::{Dispatcher, DispatcherOps};
pub use event_dispatcher::EventDispatcher;
pub use event_pair_dispatcher::EventPairDispatcher;
pub use exception::{dispatch_user_exception, dump_common_exception_context};
pub use exception_dispatcher::ExceptionDispatcher;
pub use exceptionate::Exceptionate;
pub use executor::Executor;
pub use fifo_dispatcher::FifoDispatcher;
pub use futex_context::{FutexContext, FutexId, OwnerAction, ProcessType};
pub use guest::Guest;
pub use guest_dispatcher::GuestDispatcher;
pub use handle::{Handle, HandleOwner, HandleRef, HandleValue, KernelHandle};
pub use handle_table::{
    HandleEntry, HandleTable, HandleTableLockClass, HandleTableReadGuard, HandleTableWriteGuard,
};
pub use interrupt_dispatcher::InterruptDispatcher;
pub use interrupt_event_dispatcher::InterruptEventDispatcher;
pub use io_buffer_dispatcher::IoBufferDispatcher;
pub use io_buffer_shared_region_dispatcher::IoBufferSharedRegionDispatcher;
pub use iommu_dispatcher::IommuDispatcher;
pub use job_dispatcher::*;
pub use job_policy::{JobPolicy, JobPolicyCollection, Policy};
pub use log_dispatcher::*;
pub use memory_watchdog::*;
pub use message_packet::{MessagePacket, MessagePacketPtr};
pub use msi_allocation::MsiAllocation;
pub use msi_dispatcher::MsiDispatcher;
pub use msi_interrupt_dispatcher::MsiInterruptDispatcher;
pub use pager_dispatcher::PagerDispatcher;
pub use pinned_memory_token_dispatcher::{PinnedMemoryTokenDispatcher, dev_vaddr_t};
pub use port_dispatcher::{PortDispatcher, PortPacket, RawPortPacket};
pub use process_dispatcher::{CurrentProcessDispatcher, ProcessDispatcher};
pub use profile_dispatcher::ProfileDispatcher;
pub use resource::{
    validate_ranged_resource, validate_ranged_resource_dispatcher, validate_resource_irq,
    validate_resource_kind_base, validate_system_resource,
};
pub use resource_dispatcher::{ResourceDispatcher, is_valid_kind};
pub use root_job_observer::RootJobObserver;
pub use root_job_observer_ffi::start_root_job_observer;
pub use sampler_dispatcher::SamplerDispatcher;
pub use socket_dispatcher::{Disposition, ReadType, SocketDispatcher};
pub use stream_dispatcher::{StreamDispatcher, StreamOptions};
pub use suspend_token_dispatcher::SuspendTokenDispatcher;
pub use thread_dispatcher::{AutoBlocked, Blocked, CurrentMessageWaiter, ThreadDispatcher};
pub use timer_dispatcher::TimerDispatcher;
pub use user_handles::{ReadHandles, WriteHandles, remove_user_handles};
pub use vcpu::Vcpu;
pub use vcpu_dispatcher::VcpuDispatcher;
pub use virtual_interrupt_dispatcher::VirtualInterruptDispatcher;
pub use vm_address_region_dispatcher::VmAddressRegionDispatcher;
pub use vm_object_dispatcher::{
    InitialMutability, VmObjectDispatcher, VmoOwnership, vmo_to_info_entry,
};
pub use wait_signal_observer::WaitSignalObserver;
