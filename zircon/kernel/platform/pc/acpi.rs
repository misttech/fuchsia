// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

unsafe extern "C" {
    fn cpp_global_acpi_parser_state() -> *const core::ffi::c_void;
}

pub fn global_acpi_lite_parser() -> &'static acpi_lite::AcpiParser<'static> {
    // SAFETY: `cpp_global_acpi_parser_state` returns a valid, static global pointer to the ACPI parser.
    let parser_ptr = unsafe { cpp_global_acpi_parser_state() };
    if parser_ptr.is_null() {
        panic!("PlatformInitAcpi() not called");
    }
    unsafe { &*(parser_ptr as *const acpi_lite::AcpiParser<'static>) }
}
