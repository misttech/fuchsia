// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fuchsia_runtime::process_self;
use helper::{bootfs_entries, init_test_logging, koid};
use libc::{dl_iterate_phdr, dl_phdr_info, size_t};
use std::collections::BTreeSet;
use std::ffi::{c_int, c_void};
use zx::{Koid, MapDetails};

unsafe extern "C" fn collect_module_addr(
    hdr: *mut dl_phdr_info,
    _hdr_size: size_t,
    ck: *mut c_void,
) -> c_int {
    // SAFETY: `hdr` and `ck` are provided by `dl_iterate_phdr`, where `ck`
    // points to the `BTreeSet<usize>` passed by `check_bootfs_entries`, and
    // `hdr->dlpi_phdr` has at least one program header (`dlpi_phnum > 0`).
    unsafe {
        let modules = &mut *(ck as *mut BTreeSet<usize>);
        let info = &*hdr;
        assert!(info.dlpi_phnum > 0);
        let first_phdr = &*info.dlpi_phdr;
        // Pick the first program header to locate the VMO backing the mapping.
        modules.insert((info.dlpi_addr + first_phdr.p_vaddr) as usize);
    }
    0
}

fn check_bootfs_entries() {
    let entries = bootfs_entries();

    // Base address of RO section of each module.
    let mut modules = BTreeSet::<usize>::new();
    // SAFETY: `collect_module_addr` casts `data` back to `&mut BTreeSet<usize>`
    // for the duration of the synchronous `dl_iterate_phdr` call.
    unsafe {
        dl_iterate_phdr(Some(collect_module_addr), (&raw mut modules).cast::<c_void>());
    }

    // One of the entries is the vDSO, which is not a blob handed over from bootfs.
    assert_eq!(modules.len(), entries.len() + 1);

    // Determine that all the blobs in `entries` exist in this process by checking
    // the base addresses and matching them up.
    let mappings = process_self().info_maps_vec().expect("failed to query process maps");

    let mut module_koids = BTreeSet::<Koid>::new();
    for mapping in &mappings {
        let MapDetails::Mapping(details) = mapping.details() else {
            continue;
        };

        let Some(&module_addr) = modules.range(mapping.base..).next() else {
            continue;
        };
        // `module_addr` may or not be contained in our mapping.
        if module_addr < mapping.base + mapping.size {
            module_koids.insert(details.vmo_koid);
        }
    }

    // Now we have a unique mapping from a module to a mapping. Let's check that the mappings KOIDs
    // and content match. We don't need to match the entire VMO contents, since some sections may be
    // execute only, but the selected mapping will give us the offset in the vmo and the mapping
    // size should give us the length we can use to compare. The VMO KOID will allow us to match
    // those handed over by userboot protocol.
    let entry_koids: BTreeSet<Koid> =
        entries.iter().map(|entry| koid(entry.contents.as_handle_ref())).collect();

    let not_found_in_entries: Vec<Koid> = module_koids.difference(&entry_koids).copied().collect();
    // There should only be one missing KOID, that is the vDSO VMO.
    assert_eq!(not_found_in_entries.len(), 1);

    let not_found_in_modules: Vec<Koid> = entry_koids.difference(&module_koids).copied().collect();
    // All VMOs in `entries` are present in the mappings, though not all VMOs in
    // `mappings` are present in `entries` due to the vDSO.
    assert!(not_found_in_modules.is_empty());
}

fn main() {
    init_test_logging();
    check_bootfs_entries();
}
