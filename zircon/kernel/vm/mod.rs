// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

pub mod arch_vm_aspace;
pub mod attribution;
pub mod compressor;
pub mod discardable_vmo_tracker;
pub mod evictor;
pub mod fault;
pub mod page;
pub mod page_queues;
pub mod page_source;
pub mod page_state;
pub mod physical_page_borrowing_config;
pub mod physmap;
pub mod pinned_vm_object;
pub mod pmm;
pub mod pmm_arena;
pub mod pmm_node;
pub mod scanner;
#[allow(clippy::module_inception)]
pub mod vm;
pub mod vm_address_region;
pub mod vm_aspace;
pub mod vm_cow_pages;
pub mod vm_mapping;
pub mod vm_object;
pub mod vm_object_paged;
pub mod vm_object_physical;
pub mod vm_page_list;
pub mod vmm;
