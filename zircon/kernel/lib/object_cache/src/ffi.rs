// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::{
    count_object_allocation, count_object_free, count_slab_allocation, count_slab_free,
    get_processor_count,
};

#[unsafe(no_mangle)]
pub extern "C" fn rust_object_cache_count_object_allocation() {
    count_object_allocation();
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_object_cache_count_object_free() {
    count_object_free();
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_object_cache_count_slab_allocation() {
    count_slab_allocation();
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_object_cache_count_slab_free() {
    count_slab_free();
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_object_cache_get_processor_count() -> usize {
    get_processor_count()
}
