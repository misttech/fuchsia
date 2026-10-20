// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use zx_types::ZX_MAX_NAME_LEN;

/// Fixed-size buffer holding an encoded debugdata VMO name.
pub type VmoName = [u8; ZX_MAX_NAME_LEN];

/// Higher level components will search under this root for debug data files from early boot.
pub const ROOT_PATH: &str = "i/";

/// Debugdata files from early boot under this directory imply that the data is in flux, and
/// continuously being updated.
pub const DYNAMIC_SUB_DIR: &str = "/d/";

/// Debugdata files from early boot under this directory imply that the data is static, and wont
/// change.
pub const STATIC_SUB_DIR: &str = "/s/";

/// Debugdata sink used for logs.
pub const LOG_DEBUG_DATA_SINK: &str = "logs";

/// Returns an encoded path that fits within an object name. The encoded path is of the form:
///   "$PATH = $ROOT_DIR/$SINK_NAME/$DATA_TYPE/$MODULE_NAME.$SUFFIX"
///
/// Given the length limit of an object's name property, and it's truncated to fit.
///
/// It is considered if at least the module name does not fit into the encoded path.
pub fn debugdata_vmo_name(
    sink_name: &str,
    module_name: &str,
    suffix: &str,
    is_static: bool,
) -> VmoName {
    let mut name: VmoName = [0; ZX_MAX_NAME_LEN];
    let mut buffer = &mut name[..];
    let data_type = if is_static { STATIC_SUB_DIR } else { DYNAMIC_SUB_DIR };
    let path = [ROOT_PATH, sink_name, data_type, module_name];

    // At least a few characters of the module name must fit in the name.
    let length: usize = path.iter().map(|str| str.len()).sum();
    let overage = length as isize - ZX_MAX_NAME_LEN as isize;
    assert!(overage <= 4, "VMO name would be {} > {} by {}", length, ZX_MAX_NAME_LEN, overage);
    for component in path {
        let copy_len = component.len().min(buffer.len() - 1);
        buffer[..copy_len].copy_from_slice(&component.as_bytes()[..copy_len]);
        buffer = &mut buffer[copy_len..];
        if buffer.is_empty() {
            break;
        }
    }

    // Either append full suffix or nothing at all.
    let suffix_bytes = suffix.as_bytes();
    if buffer.len() > suffix_bytes.len() {
        buffer[0] = b'.';
        buffer[1..1 + suffix_bytes.len()].copy_from_slice(suffix_bytes);
    }
    name
}

/// Kernel unit tests for `debugdata`.
#[cfg(ktest)]
#[unittest::suite(name = "debugdata_tests")]
mod tests {
    use unittest::expect_true;

    /// Tests encoding of dynamic, static, and truncated debugdata VMO names.
    #[test]
    fn test_debugdata_vmo_name() {
        let dyn_name = debugdata_vmo_name("memory-profile", "heap", "bin", false);
        expect_true!(dyn_name.starts_with(b"i/memory-profile/d/heap.bin\0"));

        let static_name = debugdata_vmo_name("logs", "physboot", "log", true);
        expect_true!(static_name.starts_with(b"i/logs/s/physboot.log\0"));

        // When the suffix does not fit in the remaining buffer (e.g. 25 + 8 = 33 > 32), it is
        // omitted.
        let long_name = debugdata_vmo_name("llvm-profile", "vmzircon", "profraw", true);
        expect_true!(long_name.starts_with(b"i/llvm-profile/s/vmzircon\0"));

        // When the suffix exactly fills the remaining buffer (24 + 8 = 32), it is appended and the
        // name is not NUL-terminated.
        let exact_name = debugdata_vmo_name("llvm-profile", "vmzirco", "profraw", true);
        expect_true!(exact_name == *b"i/llvm-profile/s/vmzirco.profraw");

        // When the path itself overflows by up to 4 bytes (here 34 > 32), the module name is cut
        // to leave room for the NUL terminator and the suffix is omitted.
        let cut_name = debugdata_vmo_name("llvm-profile", "vmzircon-physboot", "profraw", true);
        expect_true!(cut_name == *b"i/llvm-profile/s/vmzircon-physb\0");
    }
}
