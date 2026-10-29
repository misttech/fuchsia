// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::bpf::fs::get_bpf_object;
use crate::bpf::{BpfMapHandle, check_cap_or_sys_admin};
use crate::security;
use crate::task::{CurrentTask, Kernel, register_delayed_release};
use crate::vfs::{FdNumber, OutputBuffer};
use ebpf::{
    BPF_LDDW, BPF_PSEUDO_BTF_ID, BPF_PSEUDO_FUNC, BPF_PSEUDO_MAP_FD, BPF_PSEUDO_MAP_IDX,
    BPF_PSEUDO_MAP_IDX_VALUE, BPF_PSEUDO_MAP_VALUE, CallingContext, EbpfError, EbpfInstruction,
    EbpfProgram, EbpfProgramContext, NullVerifierLogger, StaticHelperSet, VerifiedEbpfProgram,
    VerifierError, VerifierLogger, link_program, verify_program,
};
use ebpf_api::{AttachType, EbpfApiError, MapsContext, PinnedMap, ProgramType, StructId};
use fidl_fuchsia_ebpf as febpf;
use starnix_lifecycle::{AtomicCounter, ObjectReleaser, ReleaserAction};
use starnix_logging::track_stub;
use starnix_types::ownership::{Releasable, ReleaseGuard};
use starnix_uapi::auth::{CAP_BPF, CAP_NET_ADMIN, CAP_PERFMON};
use starnix_uapi::errors::{Errno, ErrnoCode};
use starnix_uapi::{bpf_attr__bindgen_ty_4, errno, error};
use std::sync::{Arc, Weak};

bitflags::bitflags! {
    /// Verifier log level flags (`BPF_LOG_*` in Linux `include/linux/bpf_verifier.h`).
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct VerifierLogLevel: u32 {
        /// `BPF_LOG_LEVEL1`: log instruction trace, but reset the log buffer on verification
        /// success so only verification failures retain the trace.
        const LEVEL1 = 1 << 0;
        /// `BPF_LOG_LEVEL2`: verbose trace retained even when verification succeeds.
        const LEVEL2 = 1 << 1;
        /// `BPF_LOG_STATS`: log verifier statistics.
        const STATS = 1 << 2;
    }
}

#[derive(Clone, Debug)]
pub struct ProgramInfo {
    pub program_type: ProgramType,
    pub expected_attach_type: AttachType,
    pub log_level: VerifierLogLevel,
}

impl TryFrom<&bpf_attr__bindgen_ty_4> for ProgramInfo {
    type Error = Errno;

    fn try_from(info: &bpf_attr__bindgen_ty_4) -> Result<Self, Self::Error> {
        Ok(Self {
            program_type: info.prog_type.try_into().map_err(map_ebpf_api_error)?,
            expected_attach_type: info.expected_attach_type.into(),
            log_level: VerifierLogLevel::from_bits_retain(info.log_level),
        })
    }
}
pub type ProgramId = u32;

static NEXT_PROGRAM_ID: AtomicCounter<u32> = AtomicCounter::<u32>::new_const(1);
fn new_program_id() -> ProgramId {
    NEXT_PROGRAM_ID.next()
}

#[derive(Debug)]
pub struct Program {
    /// Program info specified during program initialization.
    pub info: ProgramInfo,

    /// Verified program.
    program: VerifiedEbpfProgram,

    /// eBPF maps used by the program. These should match the `maps` field in
    /// the `program`.
    maps: Vec<BpfMapHandle>,

    /// Integer program ID. This is dinstinct from `fidl_id` because `fidl_id`
    /// is 64-bit, while Linux uses 32-bit IDs.
    id: ProgramId,

    /// Handle used when the program is transferred over FIDL to other services.
    fidl_handle: febpf::ProgramHandle,

    /// ID of the program used in FIDL
    fidl_id: febpf::ProgramId,

    /// The service end of the `fidl_handle`. Should be moved to the BPF Service
    /// once it's implemented.
    #[allow(dead_code)]
    service_handle: zx::EventPair,

    /// Weak reference to the Kernel where this program is registered.
    kernel: Weak<Kernel>,

    /// The security state associated with this bpf Program.
    pub security_state: security::BpfProgState,
}

fn map_ebpf_api_error(e: EbpfApiError) -> Errno {
    match e {
        EbpfApiError::InvalidProgramType(_) | EbpfApiError::InvalidExpectedAttachType(_) => {
            errno!(EINVAL, e)
        }
        EbpfApiError::UnsupportedProgramType(_) => errno!(ENOTSUP, e),
    }
}

#[track_caller]
fn verifier_error_to_errno(e: VerifierError) -> Errno {
    Errno::with_context(ErrnoCode::from_error_code(e.errno() as i16), e)
}

fn map_ebpf_error(e: EbpfError) -> Errno {
    match e {
        EbpfError::ProgramVerifyError(v) => verifier_error_to_errno(v),
        _ => errno!(EINVAL, e),
    }
}

fn verify_with_logger(
    code: Vec<EbpfInstruction>,
    log_level: VerifierLogLevel,
    logger: &mut dyn OutputBuffer,
    make_calling_context: impl Fn() -> Result<CallingContext, Errno>,
) -> Result<VerifiedEbpfProgram, Errno> {
    if log_level.is_empty() || logger.available() == 0 {
        return verify_program(code, make_calling_context()?, &mut NullVerifierLogger)
            .map_err(verifier_error_to_errno);
    }
    if !log_level.contains(VerifierLogLevel::LEVEL2) {
        if let Ok(program) =
            verify_program(code.clone(), make_calling_context()?, &mut NullVerifierLogger)
        {
            logger.write_all(b"\0")?;
            return Ok(program);
        }
    }
    let mut buffered_logger = BufferVerifierLogger::new(logger);
    let res = verify_program(code, make_calling_context()?, &mut buffered_logger);
    buffered_logger.flush()?;
    res.map_err(verifier_error_to_errno)
}

impl Program {
    pub fn new(
        current_task: &CurrentTask,
        info: ProgramInfo,
        logger: &mut dyn OutputBuffer,
        mut code: Vec<EbpfInstruction>,
    ) -> Result<ProgramHandle, Errno> {
        let maps = link_maps_fds(current_task, &mut code)?;
        let allow_ptr_leaks = check_cap_or_sys_admin(current_task, CAP_PERFMON).is_ok();
        let program = verify_with_logger(code, info.log_level, logger, || {
            let maps_schema = maps.iter().map(|m| m.schema).collect();
            let mut calling_context = info
                .program_type
                .create_calling_context(info.expected_attach_type, maps_schema)
                .map_err(map_ebpf_api_error)?;
            calling_context.allow_ptr_leaks = allow_ptr_leaks;
            Ok(calling_context)
        })?;

        let (fidl_handle, service_handle) = zx::EventPair::create();
        let fidl_id =
            febpf::ProgramId { id: fidl_handle.koid().expect("Failed to get koid").raw_koid() };
        let fidl_handle = febpf::ProgramHandle { handle: fidl_handle };

        let program = ProgramHandle::new(
            Self {
                info,
                program,
                maps,
                id: new_program_id(),
                fidl_handle,
                fidl_id,
                service_handle,
                kernel: Arc::downgrade(current_task.kernel()),
                security_state: security::bpf_prog_alloc(current_task),
            }
            .into(),
        );
        current_task.kernel().ebpf_state.register_program(&program);

        Ok(program)
    }

    pub fn id(&self) -> ProgramId {
        self.id
    }

    pub fn link<C: EbpfProgramContext<Map = PinnedMap> + StaticHelperSet>(
        &self,
        program_type: ProgramType,
    ) -> Result<EbpfProgram<C>, Errno>
    where
        for<'a> C::RunContext<'a>: MapsContext<'a>,
    {
        if program_type != self.info.program_type {
            return error!(EINVAL);
        }

        let maps = self.maps.iter().map(|map| map.get_inner()).collect();
        let program = link_program(&self.program, maps).map_err(map_ebpf_error)?;

        Ok(program)
    }

    pub(super) fn check_load_access(
        current_task: &CurrentTask,
        info: &ProgramInfo,
    ) -> Result<(), Errno> {
        if matches!(info.program_type, ProgramType::CgroupSkb | ProgramType::SocketFilter)
            && current_task.kernel().allow_unprivileged_bpf()
        {
            return Ok(());
        }

        check_cap_or_sys_admin(current_task, CAP_BPF)?;

        match info.program_type {
            // Loading tracing program types additionally require the CAP_PERFMON capability.
            ProgramType::Kprobe
            | ProgramType::Tracepoint
            | ProgramType::PerfEvent
            | ProgramType::RawTracepoint
            | ProgramType::RawTracepointWritable
            | ProgramType::Tracing => check_cap_or_sys_admin(current_task, CAP_PERFMON),

            // Loading networking program types additionally require the CAP_NET_ADMIN capability.
            ProgramType::SchedCls
            | ProgramType::SchedAct
            | ProgramType::Xdp
            | ProgramType::SockOps
            | ProgramType::SkSkb
            | ProgramType::SkMsg
            | ProgramType::SkLookup
            | ProgramType::SkReuseport
            | ProgramType::FlowDissector
            | ProgramType::Netfilter => check_cap_or_sys_admin(current_task, CAP_NET_ADMIN),

            // No additional checks are necessary for other program types.
            ProgramType::CgroupDevice
            | ProgramType::CgroupSkb
            | ProgramType::CgroupSock
            | ProgramType::CgroupSockAddr
            | ProgramType::CgroupSockopt
            | ProgramType::CgroupSysctl
            | ProgramType::Ext
            | ProgramType::LircMode2
            | ProgramType::Lsm
            | ProgramType::LwtIn
            | ProgramType::LwtOut
            | ProgramType::LwtSeg6Local
            | ProgramType::LwtXmit
            | ProgramType::SocketFilter
            | ProgramType::StructOps
            | ProgramType::Syscall
            | ProgramType::Unspec => Ok(()),
        }
    }

    pub fn fidl_id(&self) -> febpf::ProgramId {
        self.fidl_id
    }

    pub fn fidl_handle(&self) -> febpf::ProgramHandle {
        let handle = self
            .fidl_handle
            .handle
            .duplicate_handle(zx::Rights::TRANSFER | zx::Rights::SIGNAL | zx::Rights::WAIT)
            .expect("Failed to duplicate handle");
        febpf::ProgramHandle { handle }
    }
}

impl Releasable for Program {
    type Context<'a> = &'a CurrentTask;

    fn release<'a>(self, _current_task: &'a CurrentTask) {
        if let Some(kernel) = self.kernel.upgrade() {
            kernel.ebpf_state.unregister_program(self.id);
        }

        // Signal the FIDL handle to indicate that the program handle is defunct
        // and should be closed.
        self.fidl_handle
            .handle
            .signal(
                zx::Signals::NONE,
                zx::Signals::from_bits_truncate(febpf::PROGRAM_DEFUNCT_SIGNAL),
            )
            .unwrap();
    }
}

pub enum ProgramReleaserAction {}
impl ReleaserAction<Program> for ProgramReleaserAction {
    fn release(program: ReleaseGuard<Program>) {
        register_delayed_release(program);
    }
}
pub type ProgramReleaser = ObjectReleaser<Program, ProgramReleaserAction>;
pub type ProgramHandle = Arc<ProgramReleaser>;
pub type WeakProgramHandle = Weak<ProgramReleaser>;

impl TryFrom<&Program> for febpf::VerifiedProgram {
    type Error = Errno;

    fn try_from(program: &Program) -> Result<febpf::VerifiedProgram, Errno> {
        let mut maps = Vec::with_capacity(program.maps.len());
        for map in program.maps.iter() {
            maps.push(map.share().map_err(|_| errno!(EIO))?);
        }

        let code_u64: &[u64] = zerocopy::transmute_ref!(program.program.code());

        let mut struct_access_instructions =
            Vec::with_capacity(program.program.struct_access_instructions().len());
        for v in program.program.struct_access_instructions() {
            let struct_id = StructId::try_from(&v.memory_id).map_err(|()| errno!(EINVAL))?.into();
            struct_access_instructions.push(febpf::StructAccess {
                pc: v.pc.try_into().unwrap(),
                struct_id,
                field_offset: v.field_offset.try_into().unwrap(),
                is_32_bit_ptr_load: v.is_32_bit_ptr_load,
            })
        }
        Ok(febpf::VerifiedProgram {
            code: Some(code_u64.to_vec()),
            struct_access_instructions: Some(struct_access_instructions),
            maps: Some(maps),
            ..Default::default()
        })
    }
}

/// Links maps referenced by FD, replacing them with by-index references.
fn link_maps_fds(
    current_task: &CurrentTask,
    code: &mut Vec<EbpfInstruction>,
) -> Result<Vec<BpfMapHandle>, Errno> {
    let code_len = code.len();
    let mut maps = Vec::<BpfMapHandle>::new();
    for (pc, instruction) in code.iter_mut().enumerate() {
        if instruction.code() == BPF_LDDW {
            // BPF_LDDW requires 2 instructions.
            if pc >= code_len - 1 {
                return error!(EINVAL);
            }

            match instruction.src_reg() {
                0 => {}
                BPF_PSEUDO_MAP_FD | BPF_PSEUDO_MAP_VALUE => {
                    let lddw_type = if instruction.src_reg() == BPF_PSEUDO_MAP_FD {
                        BPF_PSEUDO_MAP_IDX
                    } else {
                        BPF_PSEUDO_MAP_IDX_VALUE
                    };
                    // If the instruction references a map fd, then we need to look up the map fd
                    // and create a reference from this program to that object.
                    instruction.set_src_reg(lddw_type);

                    let fd = FdNumber::from_raw(instruction.imm());
                    let object = get_bpf_object(current_task, fd)?;
                    let map: &BpfMapHandle = object.as_map()?;

                    // Find the map in `maps` or insert it otherwise.
                    let maybe_index = maps.iter().position(|v| Arc::ptr_eq(v, map));
                    let index = match maybe_index {
                        Some(index) => index,
                        None => {
                            let index = maps.len();
                            maps.push(map.clone());
                            index
                        }
                    };

                    instruction.set_imm(index.try_into().unwrap());
                }
                BPF_PSEUDO_MAP_IDX
                | BPF_PSEUDO_MAP_IDX_VALUE
                | BPF_PSEUDO_BTF_ID
                | BPF_PSEUDO_FUNC => {
                    track_stub!(
                        TODO("https://fxbug.dev/378564467"),
                        "unsupported pseudo src for ldimm64",
                        instruction.src_reg()
                    );
                    return error!(ENOTSUP);
                }
                _ => {
                    return error!(EINVAL);
                }
            }
        }
    }
    Ok(maps)
}

struct BufferVerifierLogger<'a> {
    buffer: &'a mut dyn OutputBuffer,
    data: Vec<u8>,
    max_len: usize,
    full: bool,
}

impl<'a> BufferVerifierLogger<'a> {
    fn new(buffer: &'a mut dyn OutputBuffer) -> Self {
        // Reserve 1 byte in the output buffer for the trailing NUL terminator.
        let max_len = buffer.available().saturating_sub(1);
        let full = max_len == 0;
        Self { buffer, data: Vec::new(), max_len, full }
    }

    fn flush(mut self) -> Result<(), Errno> {
        if self.buffer.available() > 0 {
            self.data.push(0);
            self.buffer.write_all(&self.data)?;
        }
        Ok(())
    }
}

impl VerifierLogger for BufferVerifierLogger<'_> {
    fn is_enabled(&self) -> bool {
        !self.full
    }

    fn log(&mut self, line: &[u8]) {
        debug_assert!(line.is_ascii());

        if self.full {
            return;
        }
        if self.data.len() + line.len() + 1 > self.max_len {
            self.full = true;
            return;
        }
        self.data.extend_from_slice(line);
        self.data.push(b'\n');
        if self.data.len() == self.max_len {
            self.full = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::buffers::VecOutputBuffer;
    use ebpf::{BPF_ADD, BPF_ALU64, BPF_EXIT, BPF_JMP, BPF_K, BPF_MOV};

    #[::fuchsia::test]
    fn test_buffer_verifier_logger_batches_and_nul_terminates() {
        // Buffer has room for "abc\n" (4 bytes) + "\0" (1 byte) = 5 bytes, so second line is
        // dropped while still NUL-terminating.
        let mut out = VecOutputBuffer::new(6);
        let mut logger = BufferVerifierLogger::new(&mut out);
        assert!(logger.is_enabled());
        logger.log(b"abc");
        assert!(logger.is_enabled());
        logger.log(b"def");
        assert!(!logger.is_enabled());
        logger.flush().expect("flush");
        assert_eq!(out.data(), b"abc\n\0");
    }

    #[::fuchsia::test]
    fn test_verify_with_logger_log_levels() {
        let valid_code = vec![
            EbpfInstruction::new(BPF_ALU64 | BPF_MOV | BPF_K, 0, 0, 0, 0),
            EbpfInstruction::new(BPF_JMP | BPF_EXIT, 0, 0, 0, 0),
        ];
        let invalid_code = vec![
            EbpfInstruction::new(BPF_ALU64 | BPF_ADD | BPF_K, 0, 0, 0, 1),
            EbpfInstruction::new(BPF_JMP | BPF_EXIT, 0, 0, 0, 0),
        ];
        let make_context = || {
            ProgramType::SocketFilter
                .create_calling_context(AttachType::Unspecified, Vec::new())
                .map_err(map_ebpf_api_error)
        };

        // log_level = empty: nothing written to log buffer even when provided.
        let mut out = VecOutputBuffer::new(256);
        let _prog = verify_with_logger(
            valid_code.clone(),
            VerifierLogLevel::empty(),
            &mut out,
            make_context,
        )
        .expect("verify valid program with log_level=0");
        assert!(out.data().is_empty());

        // log_level = LEVEL1 (BPF_LOG_LEVEL1) on success: resets log buffer to empty NUL-terminated
        // string without formatting instruction trace.
        let mut out = VecOutputBuffer::new(256);
        let _prog = verify_with_logger(
            valid_code.clone(),
            VerifierLogLevel::LEVEL1,
            &mut out,
            make_context,
        )
        .expect("verify valid program with log_level=1");
        assert_eq!(out.data(), b"\0");

        // log_level = LEVEL1 (BPF_LOG_LEVEL1) on failure: populates full instruction trace + error
        // and NUL-terminates.
        let mut out = VecOutputBuffer::new(256);
        let Err(err) =
            verify_with_logger(invalid_code, VerifierLogLevel::LEVEL1, &mut out, make_context)
        else {
            panic!("invalid program must fail");
        };
        assert_eq!(err, errno!(EACCES));
        assert!(out.data().ends_with(b"\0"));
        let log_str = std::str::from_utf8(&out.data()[..out.data().len() - 1]).unwrap();
        assert!(log_str.contains("0: (07) add %r0, 0x1"), "unexpected log: {log_str}");

        // log_level = LEVEL2 (BPF_LOG_LEVEL2) on success: retains full instruction trace and
        // NUL-terminates.
        let mut out = VecOutputBuffer::new(256);
        let _prog =
            verify_with_logger(valid_code, VerifierLogLevel::LEVEL2, &mut out, make_context)
                .expect("verify valid program with log_level=2");
        assert!(out.data().ends_with(b"\0"));
        let log_str = std::str::from_utf8(&out.data()[..out.data().len() - 1]).unwrap();
        assert_eq!(log_str, "0: (b7) mov %r0, 0x0\n1: (95) exit\n");
    }
}
