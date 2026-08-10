// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

mod counter_dispatcher;
mod counter_dispatcher_ffi;
mod dispatcher;
mod dispatcher_ffi;
mod event_dispatcher;
mod event_dispatcher_ffi;
mod event_pair_dispatcher;
mod event_pair_dispatcher_ffi;
mod handle;
mod iommu;
mod iommu_dispatcher;
mod iommu_dispatcher_ffi;
mod job_dispatcher;
mod job_dispatcher_ffi;
mod log_dispatcher;
mod log_dispatcher_ffi;
mod msi_allocation;
mod msi_dispatcher;
mod msi_dispatcher_ffi;
mod msi_interrupt_dispatcher;
mod msi_interrupt_dispatcher_ffi;
mod process_dispatcher;
mod process_dispatcher_ffi;
mod profile_dispatcher;
mod profile_dispatcher_ffi;
mod resource_ffi;
mod sampler_dispatcher;
mod sampler_dispatcher_ffi;
mod suspend_token_dispatcher;
mod suspend_token_dispatcher_ffi;
mod thread_dispatcher;
mod thread_dispatcher_ffi;
mod timer_dispatcher;
mod timer_dispatcher_ffi;
mod vm_address_region_dispatcher;
mod vm_address_region_dispatcher_ffi;
mod vm_object_dispatcher;
mod vm_object_dispatcher_ffi;

pub use counter_dispatcher::CounterDispatcher;
pub use dispatcher::{Dispatcher, DispatcherOps};
pub use event_dispatcher::EventDispatcher;
pub use event_pair_dispatcher::EventPairDispatcher;
pub use handle::{HandleValue, KernelHandle};
pub use iommu_dispatcher::IommuDispatcher;
pub use job_dispatcher::*;
pub use log_dispatcher::*;
pub use msi_allocation::MsiAllocation;
pub use msi_dispatcher::MsiDispatcher;
pub use msi_interrupt_dispatcher::MsiInterruptDispatcher;
pub use process_dispatcher::ProcessDispatcher;
pub use profile_dispatcher::ProfileDispatcher;
pub use resource_ffi::{
    validate_ranged_resource, validate_resource_kind_base, validate_system_resource,
};
pub use sampler_dispatcher::SamplerDispatcher;
pub use sampler_dispatcher_ffi::*;
pub use suspend_token_dispatcher::SuspendTokenDispatcher;
pub use thread_dispatcher::ThreadDispatcher;
pub use timer_dispatcher::TimerDispatcher;
pub use vm_address_region_dispatcher::VmAddressRegionDispatcher;
pub use vm_object_dispatcher::VmObjectDispatcher;
