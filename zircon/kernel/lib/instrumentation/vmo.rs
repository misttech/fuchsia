// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Kernel instrumentation VMO publication and symbolizer dumpfile generation.

use core::mem::MaybeUninit;

use fbl::RefPtr;
use kprint::{kformat, kprint};
use zr::slice_from_raw_parts;
use zx_status::Status;
use zx_types::{ZX_MAX_NAME_LEN, ZX_RIGHT_WRITE, zx_rights_t};

use crate::object::{HandleOwner, InitialMutability, KernelHandle, VmObjectDispatcher};
use crate::top::boot_constants::kernel_debug_ident;
use crate::vm::pmm;
use crate::vm::vm_object_paged::VmObjectPaged;

use super::InstrumentationDataVmo;
use super::memory_profile::memory_profile_vmo;
use super::vmo_ffi::{
    InstrumentationDataVmoFfi, cpp_llvm_profdata_vmo, cpp_sancov_get_counts_vmo,
    cpp_sancov_get_pc_vmo,
};

impl InstrumentationDataVmo {
    /// Converts an FFI descriptor returned by a C++ instrumentation VMO getter into a safe Rust
    /// [`InstrumentationDataVmo`].
    ///
    /// # Safety
    ///
    /// The caller must ensure that any string pointer with non-zero length in `ffi` is non-null
    /// and points to `'static` bytes valid for reads of that length.
    unsafe fn from_ffi(ffi: InstrumentationDataVmoFfi) -> Self {
        let to_str = |ptr: *const u8, len: usize| -> &'static str {
            // SAFETY: Caller guarantees non-empty string pointers point to `'static` bytes.
            let bytes = unsafe { slice_from_raw_parts(ptr, len) };
            core::str::from_utf8(bytes)
                .expect("instrumentation VMO metadata strings must be valid UTF-8")
        };
        Self {
            announce: to_str(ffi.announce, ffi.announce_len),
            sink_name: to_str(ffi.sink_name, ffi.sink_name_len),
            units: to_str(ffi.units, ffi.units_len),
            scale: ffi.scale,
            handle: ffi.handle,
        }
    }
}

/// # Safety
///
/// Must be called at most once, after the VM system is initialized, because `LlvmProfdataVmo`
/// initializes `gProfdataLiveData` and switches live coverage data to the mapped VMO.
unsafe fn llvm_profdata_vmo() -> InstrumentationDataVmo {
    let mut ffi = MaybeUninit::<InstrumentationDataVmoFfi>::uninit();
    // SAFETY: Caller upholds the single-call/VM-initialized requirement; `cpp_llvm_profdata_vmo`
    // fully initializes `ffi` with `'static` string views and an owned `Handle*` (or null).
    unsafe {
        cpp_llvm_profdata_vmo(&mut ffi);
        InstrumentationDataVmo::from_ffi(ffi.assume_init())
    }
}

/// # Safety
///
/// Must only be called after `InitSancov` (`LK_INIT_LEVEL_KERNEL`) has initialized
/// `gSancovPcVmo`.
unsafe fn sancov_get_pc_vmo() -> InstrumentationDataVmo {
    let mut ffi = MaybeUninit::<InstrumentationDataVmoFfi>::uninit();
    // SAFETY: Caller guarantees `InitSancov` has completed; `cpp_sancov_get_pc_vmo` fully
    // initializes `ffi` with `'static` string views and an owned `Handle*` (or null).
    unsafe {
        cpp_sancov_get_pc_vmo(&mut ffi);
        InstrumentationDataVmo::from_ffi(ffi.assume_init())
    }
}

/// # Safety
///
/// Must only be called after `InitSancov` (`LK_INIT_LEVEL_KERNEL`) has initialized
/// `gSancovCountsVmo`.
unsafe fn sancov_get_counts_vmo() -> InstrumentationDataVmo {
    let mut ffi = MaybeUninit::<InstrumentationDataVmoFfi>::uninit();
    // SAFETY: Caller guarantees `InitSancov` has completed; `cpp_sancov_get_counts_vmo` fully
    // initializes `ffi` with `'static` string views and an owned `Handle*` (or null).
    unsafe {
        cpp_sancov_get_counts_vmo(&mut ffi);
        InstrumentationDataVmo::from_ffi(ffi.assume_init())
    }
}

trait OutputStream {
    fn write(&mut self, str: &[u8]) -> i32;
}

struct StdoutStream;

impl OutputStream for StdoutStream {
    fn write(&mut self, str: &[u8]) -> i32 {
        kprint!("{:s}", str);
        str.len() as i32
    }
}

// This object facilitates doing fprintf directly into the VMO representing
// the symbolizer markup data file.  This gets the symbolizer context for the
// kernel and then a dumpfile element for each VMO published.
struct SymbolizerFile {
    vmo: RefPtr<VmObjectPaged>,
    pos: usize,
}

impl SymbolizerFile {
    const VMO_NAME: &[u8] = b"i/logs/vmzircon.symbolizer";

    fn new() -> Self {
        let status =
            VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, VmObjectPaged::RESIZABLE, page::SIZE as u64);
        assert!(status.is_ok());
        Self { vmo: status.unwrap(), pos: 0 }
    }

    fn stream(&mut self) -> &mut Self {
        self
    }

    fn write(&mut self, str: &[u8]) -> i32 {
        let status = self.vmo.write(self.pos as u64, str);
        assert!(status.is_ok());
        self.pos += str.len();
        str.len() as i32
    }

    // Move the VMO into a handle and return it.
    fn finish(self) -> Option<HandleOwner> {
        // Pass `self.pos` directly as the initial `stream_size` to `VmObjectDispatcher::create`
        // (rather than initializing the stream size to `0` and calling `set_stream_size(self.pos)`,
        // which would zero `[0, self.pos)` under RFC-0238).
        let status =
            VmObjectDispatcher::create(&self.vmo, self.pos as u64, InitialMutability::Mutable);
        assert!(status.is_ok());
        let (handle, rights) = status.unwrap();
        let status = handle.dispatcher().set_name(Self::VMO_NAME);
        debug_assert!(status.is_ok());
        HandleOwner::make(handle, rights)
    }
}

impl OutputStream for SymbolizerFile {
    fn write(&mut self, str: &[u8]) -> i32 {
        SymbolizerFile::write(self, str)
    }
}

fn print_dumpfile(data: &InstrumentationDataVmo, streams: &mut [&mut dyn OutputStream]) {
    let Some(handle) = &data.handle else {
        return;
    };

    let vmo = handle
        .dispatcher_ref()
        .downcast::<VmObjectDispatcher>()
        .expect("instrumentation VMO handle must wrap VmObjectDispatcher");

    let mut name_buffer = [0u8; ZX_MAX_NAME_LEN];
    let status = vmo.get_name(&mut name_buffer);
    debug_assert!(status.is_ok());
    let nul_pos = name_buffer.iter().position(|&b| b == 0).unwrap_or(name_buffer.len());
    let vmo_name = &name_buffer[..nul_pos];

    let stream_size = vmo.get_stream_size() as usize;
    let scaled_size = stream_size / data.scale;

    for f in streams {
        let mut buf = [0u8; 256];
        let line = kformat!(
            &mut buf,
            "{:s}: {{{{{{dumpfile:{:s}:{:s}}}}}}} maximum {} {:s}.\n",
            data.announce,
            data.sink_name,
            vmo_name,
            scaled_size,
            data.units
        );
        f.write(line);
    }
}

#[repr(u32)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[expect(clippy::enum_variant_names)]
enum Vmo {
    SymbolizerVmo = 0,
    LlvmProfdataVmo = 1,
    SancovVmo = 2,
    SancovCountsVmo = 3,
    MemoryProfileVmo = 4,

    VmoCount = 5,
}

/// Kernel instrumentation VMO publisher for `userboot` handoff (`InstrumentationData`).
#[derive(Debug)]
pub struct InstrumentationData;

impl InstrumentationData {
    /// Total number of instrumentation VMO handles passed to `userboot`.
    pub const fn vmo_count() -> usize {
        Vmo::VmoCount as usize
    }

    /// Populates `handles` with the kernel instrumentation VMOs (or read-only empty stub VMOs
    /// for unused slots).
    ///
    /// # Safety
    ///
    /// Must be called at most once during `userboot` startup after the instrumentation backends
    /// have completed initialization.
    pub unsafe fn get_vmos(
        handles: &mut [Option<HandleOwner>; Self::vmo_count()],
    ) -> Result<(), Status> {
        // To keep the protocol with userboot simple, we always supply all the VMO
        // handles.  Slots with no instrumentation data to report will hold an empty
        // VMO with no name.  Create this the first time it's needed and then just
        // duplicate the read-only handle as needed.
        let stub_vmo: Option<RefPtr<VmObjectDispatcher>> = None;
        let mut rights: zx_rights_t = 0;
        let mut get_stub_vmo = || -> Option<HandleOwner> {
            let mut handle = stub_vmo.as_ref().map(|s| KernelHandle::new(s.clone()));
            if stub_vmo.is_none() {
                let vmo = VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, 0);
                assert!(vmo.is_ok());
                let status =
                    VmObjectDispatcher::create(&vmo.unwrap(), 0, InitialMutability::Mutable);
                assert!(status.is_ok());
                let (new_handle, new_rights) = status.unwrap();
                handle = Some(new_handle);
                rights = new_rights & !ZX_RIGHT_WRITE;
            }
            HandleOwner::make(handle.unwrap(), rights)
        };

        let mut symbolizer = SymbolizerFile::new();
        symbolizer.stream().write(kernel_debug_ident());

        // This is just a fixed list of steps but using a loop and a switch gets the
        // compiler to check that every enum case is handled.
        let get_getter = |idx: Vmo| -> Option<unsafe fn() -> InstrumentationDataVmo> {
            match idx {
                Vmo::LlvmProfdataVmo => Some(llvm_profdata_vmo),
                Vmo::SancovVmo => Some(sancov_get_pc_vmo),
                Vmo::SancovCountsVmo => Some(sancov_get_counts_vmo),
                Vmo::MemoryProfileVmo => Some(memory_profile_vmo),
                // The symbolizer file is done separately below since it must be last.
                Vmo::SymbolizerVmo | Vmo::VmoCount => None,
            }
        };
        const ALL_VMOS: [Vmo; InstrumentationData::vmo_count()] = [
            Vmo::SymbolizerVmo,
            Vmo::LlvmProfdataVmo,
            Vmo::SancovVmo,
            Vmo::SancovCountsVmo,
            Vmo::MemoryProfileVmo,
        ];
        let mut have_data = false;
        for (idx, &vmo_kind) in ALL_VMOS.iter().enumerate() {
            if let Some(getter) = get_getter(vmo_kind) {
                // SAFETY: The caller guarantees `get_vmos` is invoked at most once during
                // `userboot` startup after the instrumentation backends have been initialized.
                let mut data = unsafe { getter() };
                if data.handle.is_some() {
                    if !data.sink_name.is_empty() {
                        let mut stdout = StdoutStream;
                        print_dumpfile(&data, &mut [&mut stdout, symbolizer.stream()]);
                        have_data = true;
                    }
                    handles[idx] = data.handle.take();
                } else {
                    handles[idx] = get_stub_vmo();
                }
            }
        }

        handles[Vmo::SymbolizerVmo as usize] =
            if have_data { symbolizer.finish() } else { get_stub_vmo() };

        Ok(())
    }
}

/// Kernel unit tests for `vmo`.
#[cfg(ktest)]
#[unittest::suite(name = "instrumentation_vmo_tests")]
mod tests {
    use crate::vm::scanner::AutoVmScannerDisable;
    use unittest::{expect_eq, expect_ok, expect_true, unwrap_ok, unwrap_some};

    /// Tests writing symbolizer markup to `SymbolizerFile` and finishing it into a VMO handle.
    #[test]
    fn test_symbolizer_file_write_and_finish() {
        let _scanner_disable = AutoVmScannerDisable::new();

        let mut sym = SymbolizerFile::new();
        let line1 = b"{{{reset}}}\n";
        let line2 = b"{{{module:0:kernel:elf:deadbeef}}}\n";
        expect_eq!(sym.stream().write(line1), line1.len() as i32);
        expect_eq!(sym.stream().write(line2), line2.len() as i32);

        let handle = unwrap_some!(sym.finish());
        let disp = unwrap_some!(handle.dispatcher_ref().downcast::<VmObjectDispatcher>());

        let mut name_buf = [0u8; ZX_MAX_NAME_LEN];
        expect_ok!(disp.get_name(&mut name_buf));
        expect_true!(name_buf.starts_with(b"i/logs/vmzircon.symbolizer\0"));

        let expected_len = line1.len() + line2.len();
        expect_eq!(disp.get_stream_size(), expected_len as u64);

        let mut read_buf = [MaybeUninit::<u8>::uninit(); 64];
        let actual = unwrap_ok!(disp.vmo().read(0, &mut read_buf[..expected_len]));
        expect_true!(&actual[..line1.len()] == line1);
        expect_true!(&actual[line1.len()..] == line2);
    }

    /// Tests `print_dumpfile` formatting and scaling into an `OutputStream`.
    #[test]
    fn test_print_dumpfile() {
        let _scanner_disable = AutoVmScannerDisable::new();

        let mut sym = SymbolizerFile::new();

        // Empty `InstrumentationDataVmo` (no handle) should emit nothing.
        let empty = InstrumentationDataVmo::default();
        print_dumpfile(&empty, &mut [sym.stream()]);
        expect_eq!(sym.pos, 0usize);

        // Populated `InstrumentationDataVmo` should emit formatted dumpfile line with scaled size.
        let vmo = unwrap_ok!(VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, page::SIZE as u64));
        let (khandle, rights) =
            unwrap_ok!(VmObjectDispatcher::create(&vmo, 64, InitialMutability::Mutable));
        expect_ok!(khandle.dispatcher().set_name(b"data/test.vmo"));
        let handle = HandleOwner::make(khandle, rights & !ZX_RIGHT_WRITE);

        let data = InstrumentationDataVmo {
            announce: "TestCov",
            sink_name: "test-sink",
            units: "counters",
            scale: 8,
            handle,
        };
        print_dumpfile(&data, &mut [sym.stream()]);

        let expected = b"TestCov: {{{dumpfile:test-sink:data/test.vmo}}} maximum 8 counters.\n";
        expect_eq!(sym.pos, expected.len());

        let sym_handle = unwrap_some!(sym.finish());
        let sym_disp = unwrap_some!(sym_handle.dispatcher_ref().downcast::<VmObjectDispatcher>());
        let mut read_buf = [MaybeUninit::<u8>::uninit(); 128];
        let actual = unwrap_ok!(sym_disp.vmo().read(0, &mut read_buf[..expected.len()]));
        expect_true!(actual == expected);
    }
}
