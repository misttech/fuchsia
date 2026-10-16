// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Error, anyhow, bail};
use elf_parse::{Elf64Headers, SegmentType};
use fidl_next::{Request, Responder, ServerDispatcher, ServerEnd};
use fidl_next_fuchsia_ldsvc as fuchsia_ldsvc;
use fuchsia_bootfs::{
    BootfsParser, ZeroCopyBootfsEntry, zbi_bootfs_is_aligned, zbi_bootfs_page_align,
};
use fuchsia_ldsvc::loader::{Clone, Config, LoadObject};
use fuchsia_ldsvc::{Loader, LoaderLoadObjectResponse, LoaderServerHandler};
use fuchsia_runtime::{HandleInfo, HandleType};
use process_builder::elf_load::load_elf;
use process_builder::{Message, MessageContents, StartupHandle, compute_initial_stack_pointer};
use std::ffi::CString;
use std::fmt::Write as _;
use std::str;
use zx::sys::{ZX_RSRC_SYSTEM_POWER_BASE, ZX_RSRC_SYSTEM_VMEX_BASE};
use zx::{
    Channel, DebugLog, Job, Name, NullableHandle, ObjectType, Process, ProcessOptions, Resource,
    ResourceKind, Rights, Socket, Status, Vmar, VmarFlags, Vmo, VmoChildOptions,
};
use zx_libc::sanitizer::Log;

// Defined locally here rather than imported from `fdio_sys` because userboot runs before the
// FDIO/POSIX environment exists and does not depend on the fdio crate.
const FDIO_FLAG_USE_FOR_STDIO: u16 = 0x8000;

/// Represents either a DebugLog or a Socket logging handle.
pub enum SystemLog {
    /// Logging using a kernel DebugLog handle.
    DebugLog(DebugLog),
    /// Logging using a Socket handle.
    Socket(Socket),
}

impl SystemLog {
    fn to_handle(&self, rights: Rights) -> Result<NullableHandle, Status> {
        match self {
            Self::DebugLog(h) => h.duplicate_handle(rights).map(|h| h.into_handle()),
            Self::Socket(h) => h.duplicate_handle(rights).map(|h| h.into_handle()),
        }
    }

    fn duplicate(&self, rights: Rights) -> Result<Self, Status> {
        match self {
            Self::DebugLog(h) => Ok(Self::DebugLog(h.duplicate_handle(rights)?)),
            Self::Socket(h) => Ok(Self::Socket(h.duplicate_handle(rights)?)),
        }
    }
}

/// Essential system handles extracted from the bootstrap capabilities message
/// required to setup and launch the next program.
pub struct SystemHandles {
    /// Root job handle for creating child processes and default jobs.
    pub root_job: Job,
    /// VMEX resource handle for creating executable VMOs.
    pub vmex_resource: Resource,
    /// Power resource handle for power off / shutdown.
    pub power_resource: Option<Resource>,
    /// Handle to the stable vDSO VMO.
    pub vdso_vmo: Vmo,
    /// Handle to the ZBI VMO containing boot payload.
    pub zbi_vmo: Vmo,
    /// Handle to the debug log or log socket.
    pub log: SystemLog,
    /// Additional startup handles (resources, vDSOs, kernel files) to pass to the child process.
    pub startup_handles: Vec<StartupHandle>,
}

impl SystemHandles {
    /// Extracts essential system handles from the provided handle iterator.
    pub fn from_handles(handles: impl IntoIterator<Item = zx::HandleInfo>) -> Result<Self, Error> {
        let mut root_job = None;
        let mut vmex_resource = None;
        let mut power_resource = None;
        let mut vdso_vmo = None;
        let mut zbi_vmo = None;
        let mut log = None;

        let mut startup_handles = Vec::new();
        let mut vdso_count = 0u16;
        let mut kernel_file_count = 0u16;

        for zx::HandleInfo { object_type, handle, .. } in handles {
            match (object_type, handle.get_name()) {
                (ObjectType::JOB, Ok(name)) => {
                    if name == "root" {
                        root_job = Some(Job::from(handle));
                    }
                }
                (ObjectType::RESOURCE, Ok(name)) => {
                    let htype = if name == "mmio" {
                        HandleType::MmioResource
                    } else if name == "irq" {
                        HandleType::IrqResource
                    } else if name == "io_port" {
                        HandleType::IoportResource
                    } else if name == "smc" {
                        HandleType::SmcResource
                    } else if name == "system" {
                        let resource = handle.as_handle_ref().cast::<Resource>();
                        if vmex_resource.is_none() {
                            vmex_resource = resource
                                .create_child(
                                    ResourceKind::SYSTEM,
                                    None,
                                    ZX_RSRC_SYSTEM_VMEX_BASE,
                                    1,
                                    b"vmex",
                                )
                                .ok();
                        }
                        if power_resource.is_none() {
                            power_resource = resource
                                .create_child(
                                    ResourceKind::SYSTEM,
                                    None,
                                    ZX_RSRC_SYSTEM_POWER_BASE,
                                    1,
                                    b"power",
                                )
                                .ok();
                        }
                        HandleType::SystemResource
                    } else if name == "vmex" {
                        vmex_resource = Some(Resource::from(handle));
                        continue;
                    } else if name == "power" {
                        power_resource = Some(Resource::from(handle));
                        continue;
                    } else {
                        continue;
                    };
                    startup_handles.push(StartupHandle { handle, info: HandleInfo::new(htype, 0) });
                }
                (ObjectType::VMO, Ok(name)) => {
                    // Skip zero-sized VMOs.
                    if handle.as_handle_ref().cast::<Vmo>().get_size() == Ok(0) {
                        continue;
                    }
                    if name == "zbi" {
                        zbi_vmo = Some(Vmo::from(handle));
                    } else if name.as_bstr().starts_with(b"vdso/") {
                        if vdso_vmo.is_none() {
                            vdso_vmo =
                                Some(Vmo::from(handle.duplicate_handle(Rights::SAME_RIGHTS)?));
                        }
                        startup_handles.push(StartupHandle {
                            handle,
                            info: HandleInfo::new(HandleType::VdsoVmo, vdso_count),
                        });
                        vdso_count += 1;
                    } else {
                        startup_handles.push(StartupHandle {
                            handle,
                            info: HandleInfo::new(HandleType::KernelFileVmo, kernel_file_count),
                        });
                        kernel_file_count += 1;
                    }
                }
                (ObjectType::DEBUGLOG, _) => {
                    log = Some(SystemLog::DebugLog(DebugLog::from(handle)));
                }
                (ObjectType::SOCKET, _) => {
                    log = Some(SystemLog::Socket(Socket::from(handle)));
                }
                _ => {}
            }
        }

        let root_job = root_job.ok_or_else(|| anyhow!("Root job handle not found"))?;
        let vmex_resource =
            vmex_resource.ok_or_else(|| anyhow!("VMEX resource handle not found"))?;
        let vdso_vmo = vdso_vmo.ok_or_else(|| anyhow!("vDSO VMO handle not found"))?;
        let zbi_vmo = zbi_vmo.ok_or_else(|| anyhow!("ZBI VMO handle not found"))?;
        let log = log.ok_or_else(|| anyhow!("DebugLog/Socket handle not found"))?;

        Ok(SystemHandles {
            root_job,
            vmex_resource,
            power_resource,
            vdso_vmo,
            zbi_vmo,
            log,
            startup_handles,
        })
    }

    /// Duplicates internal handles to create a new SystemHandles instance.
    pub fn duplicate(&self) -> Result<Self, Status> {
        Ok(Self {
            root_job: self.root_job.duplicate_handle(Rights::SAME_RIGHTS)?,
            vmex_resource: self.vmex_resource.duplicate_handle(Rights::SAME_RIGHTS)?,
            power_resource: self
                .power_resource
                .as_ref()
                .map(|p| p.duplicate_handle(Rights::SAME_RIGHTS))
                .transpose()?,
            vdso_vmo: self.vdso_vmo.duplicate_handle(Rights::SAME_RIGHTS)?,
            zbi_vmo: self.zbi_vmo.duplicate_handle(Rights::SAME_RIGHTS)?,
            log: self.log.duplicate(Rights::SAME_RIGHTS)?,
            startup_handles: self
                .startup_handles
                .iter()
                .map(|h| {
                    Ok(StartupHandle {
                        handle: h.handle.duplicate_handle(Rights::SAME_RIGHTS)?,
                        info: h.info,
                    })
                })
                .collect::<Result<Vec<_>, Status>>()?,
        })
    }
}

/// Reserves the low half of the address space so initial process allocations
/// (program, vDSO, stack) stay out of low memory required by sanitizers like ASan.
fn reserve_low_address_space(root_vmar: &Vmar) -> Result<Vmar, Error> {
    let info = root_vmar.info()?;
    let page_size = zx::system_get_page_size() as usize;
    let reserve_len = (info.len / 2) & !(page_size - 1);
    let (vmar, addr) = root_vmar.allocate(0, reserve_len, VmarFlags::SPECIFIC)?;
    if addr != info.base {
        bail!("zx_vmar_allocate gave wrong address for low address space reservation");
    }
    Ok(vmar)
}

/// Creates a VMO representing a file entry within the BOOTFS image at the given `offset` and
/// `size`, and marks it as executable using the VMEX resource.
///
/// When `bootfs_entries` is `Some`, the file's pages are transferred directly out of `bootfs_vmo`
/// via `zx_vmo_transfer_data` (decommitting the source range in `bootfs_vmo`) and a handle to the
/// resulting VMO is recorded in `bootfs_entries` (or reused if already transferred). When
/// `bootfs_entries` is `None`, a read-only `SNAPSHOT_AT_LEAST_ON_WRITE` child VMO is created so
/// `bootfs_vmo` remains intact for subsequent program launches.
fn create_bootfs_file_vmo(
    bootfs_vmo: &Vmo,
    offset: u64,
    size: u64,
    vmex_resource: &Resource,
    name: &Name,
    bootfs_entries: Option<&mut Vec<(u32, Vmo)>>,
) -> Result<Vmo, Error> {
    if let Some(entries) = bootfs_entries {
        let offset_u32 = u32::try_from(offset)?;
        if !zbi_bootfs_is_aligned(offset_u32) {
            bail!("BOOTFS entry at offset {:#x} is not page-aligned", offset_u32);
        }
        if let Some((_, existing_vmo)) = entries.iter().find(|(off, _)| *off == offset_u32) {
            return Ok(existing_vmo.duplicate_handle(Rights::SAME_RIGHTS)?);
        }
        let aligned_size = u64::from(zbi_bootfs_page_align(u32::try_from(size)?));
        let vmo = Vmo::create(size)?;
        if aligned_size > 0 {
            vmo.transfer_data(
                zx::TransferDataOptions::empty(),
                0,
                aligned_size,
                bootfs_vmo,
                offset,
            )?;
        }
        vmo.set_name(name)?;
        let vmo = vmo
            .replace_handle(Rights::VMO_DEFAULT - Rights::WRITE)?
            .replace_as_executable(vmex_resource)?;
        entries.push((offset_u32, vmo.duplicate_handle(Rights::SAME_RIGHTS)?));
        Ok(vmo)
    } else {
        let vmo = bootfs_vmo
            .create_child(
                VmoChildOptions::SNAPSHOT_AT_LEAST_ON_WRITE | VmoChildOptions::NO_WRITE,
                offset,
                size,
            )?
            .replace_as_executable(vmex_resource)?;
        vmo.set_name(name)?;
        Ok(vmo)
    }
}

/// Finds a BOOTFS entry whose path equals `parts` joined by `'/'` (or ends with `'/'` followed by
/// `parts` joined by `'/'` when nested under a `userboot.root` prefix).
fn find_bootfs_entry<'a>(
    bootfs_parser: &'a BootfsParser,
    parts: &[&str],
) -> Option<ZeroCopyBootfsEntry<'a>> {
    bootfs_parser.zero_copy_iter().filter_map(Result::ok).find(|e| {
        let mut rest = e.name;
        for (i, &part) in parts.iter().rev().enumerate() {
            if i > 0 {
                let Some(r) = rest.strip_suffix('/') else {
                    return false;
                };
                rest = r;
            }
            let Some(r) = rest.strip_suffix(part) else {
                return false;
            };
            rest = r;
        }
        rest.is_empty() || rest.ends_with('/')
    })
}

/// Loads and executes the target binary (and its ELF interpreter if dynamically linked)
/// from the provided BOOTFS VMO into a new child process under `handles.root_job`.
///
/// If the program requires dynamic linking (`PT_INTERP`), this function also sets up and
/// asynchronously serves the `fuchsia.ldsvc.Loader` service to resolve dynamic library
/// dependencies.
pub async fn launch_program(
    program_name: &str,
    target_path: &str,
    args: impl IntoIterator<Item = &str>,
    bootfs_vmo: Vmo,
    handles: SystemHandles,
    mut bootfs_entries: Option<&mut Vec<(u32, Vmo)>>,
    log: &mut Log,
) -> Result<Process, Error> {
    let SystemHandles {
        root_job,
        vmex_resource,
        power_resource: _,
        vdso_vmo,
        zbi_vmo,
        log: system_log,
        startup_handles,
    } = handles;

    let (child_process, child_vmar) =
        root_job.create_child_process(ProcessOptions::empty(), program_name.as_bytes())?;
    let reserved_vmar = reserve_low_address_space(&child_vmar)?;
    let child_thread = child_process.create_thread(program_name.as_bytes())?;

    let bootfs_parser = BootfsParser::create_from_vmo(&bootfs_vmo)?;

    let entry = bootfs_parser
        .zero_copy_iter()
        .filter_map(Result::ok)
        .find(|e| e.name == target_path)
        .ok_or_else(|| anyhow!("Program '{}' not found in bootfs", target_path))?;

    let program_vmo = create_bootfs_file_vmo(
        &bootfs_vmo,
        entry.offset,
        entry.size,
        &vmex_resource,
        &Name::new_lossy(program_name),
        bootfs_entries.as_deref_mut(),
    )?;

    let headers = Elf64Headers::from_vmo(&program_vmo)?;

    let (to_child_client, to_child_server) = Channel::create();
    let mut loader_server = None;

    let entry_point = if let Some(interp_hdr) =
        headers.program_header_with_type(SegmentType::Interp)?
    {
        let mut interp_bytes = vec![0u8; interp_hdr.filesz as usize];
        program_vmo.read(&mut interp_bytes, interp_hdr.offset as u64)?;
        let interp_str =
            str::from_utf8(&interp_bytes)?.trim_end_matches('\0').trim_start_matches('/');
        let interp_name = interp_str.strip_prefix("lib/").unwrap_or(interp_str);

        let interp_entry = find_bootfs_entry(&bootfs_parser, &["lib", interp_name])
            .ok_or_else(|| anyhow!("Interpreter 'lib/{interp_name}' not found in bootfs"))?;

        let interp_vmo = create_bootfs_file_vmo(
            &bootfs_vmo,
            interp_entry.offset,
            interp_entry.size,
            &vmex_resource,
            &Name::new_lossy(interp_entry.name),
            bootfs_entries.as_deref_mut(),
        )?;

        let interp_headers = Elf64Headers::from_vmo(&interp_vmo)?;
        let loaded_interp = load_elf(&interp_vmo, &interp_headers, &child_vmar)?;

        let (ldsvc_client, ldsvc_server) = Channel::create();
        loader_server = Some(ldsvc_server);

        let handles = vec![
            StartupHandle {
                handle: program_vmo.duplicate_handle(Rights::SAME_RIGHTS)?.into_handle().into(),
                info: HandleInfo::new(HandleType::ExecutableVmo, 0),
            },
            StartupHandle {
                handle: system_log.to_handle(Rights::SAME_RIGHTS)?,
                info: HandleInfo::new(HandleType::FileDescriptor, FDIO_FLAG_USE_FOR_STDIO),
            },
            StartupHandle {
                handle: child_process.duplicate_handle(Rights::SAME_RIGHTS)?.into_handle().into(),
                info: HandleInfo::new(HandleType::ProcessSelf, 0),
            },
            StartupHandle {
                handle: child_vmar.duplicate_handle(Rights::SAME_RIGHTS)?.into_handle().into(),
                info: HandleInfo::new(HandleType::RootVmar, 0),
            },
            StartupHandle {
                handle: loaded_interp.vmar.into_handle().into(),
                info: HandleInfo::new(HandleType::LoadedVmar, 0),
            },
            StartupHandle {
                handle: child_thread.duplicate_handle(Rights::SAME_RIGHTS)?.into_handle().into(),
                info: HandleInfo::new(HandleType::ThreadSelf, 0),
            },
            StartupHandle {
                handle: ldsvc_client.into_handle().into(),
                info: HandleInfo::new(HandleType::LdsvcLoader, 0),
            },
        ];

        let contents = MessageContents {
            // Passing LD_DEBUG=1 to the dynamic linker (PT_INTERP) enables verbose loading
            // and relocation logs on the console for early system boot diagnostic purposes.
            environment_vars: vec![CString::new("LD_DEBUG=1")?],
            handles,
            ..Default::default()
        };

        let message = Message::build(contents)?;
        message.write(&to_child_client)?;
        loaded_interp.entry
    } else {
        let loaded_elf = load_elf(&program_vmo, &headers, &child_vmar)?;
        let mut initial_handles = vec![
            system_log.to_handle(Rights::SAME_RIGHTS)?,
            child_process.duplicate_handle(Rights::SAME_RIGHTS)?.into_handle(),
            child_vmar.duplicate_handle(Rights::SAME_RIGHTS)?.into_handle(),
            loaded_elf.vmar.into_handle(),
            child_thread.duplicate_handle(Rights::SAME_RIGHTS)?.into_handle(),
        ];
        to_child_client.write(&[], &mut initial_handles)?;
        loaded_elf.entry
    };

    let vdso_headers = Elf64Headers::from_vmo(&vdso_vmo)?;
    let loaded_vdso = load_elf(&vdso_vmo, &vdso_headers, &child_vmar)?;
    let vdso_base = loaded_vdso.vmar_base;

    let stack_size: usize = 256 * 1024;
    let stack_vmo = Vmo::create(stack_size as u64)?;
    stack_vmo.set_name(&Name::new_lossy("userboot-child-initial-stack"))?;
    let stack_base = child_vmar.map(
        0,
        &stack_vmo,
        0,
        stack_size,
        VmarFlags::PERM_READ | VmarFlags::PERM_WRITE,
    )?;
    let sp = compute_initial_stack_pointer(stack_base, stack_size);

    let mut handles = vec![
        StartupHandle {
            handle: child_process.duplicate_handle(Rights::SAME_RIGHTS)?.into_handle().into(),
            info: HandleInfo::new(HandleType::ProcessSelf, 0),
        },
        StartupHandle {
            handle: child_vmar.duplicate_handle(Rights::SAME_RIGHTS)?.into_handle().into(),
            info: HandleInfo::new(HandleType::RootVmar, 0),
        },
        StartupHandle {
            handle: child_thread.duplicate_handle(Rights::SAME_RIGHTS)?.into_handle().into(),
            info: HandleInfo::new(HandleType::ThreadSelf, 0),
        },
        StartupHandle {
            handle: root_job.duplicate_handle(Rights::SAME_RIGHTS)?.into_handle().into(),
            info: HandleInfo::new(HandleType::DefaultJob, 0),
        },
        StartupHandle {
            handle: zbi_vmo.duplicate_handle(Rights::SAME_RIGHTS)?.into_handle().into(),
            info: HandleInfo::new(HandleType::BootdataVmo, 0),
        },
        StartupHandle {
            handle: system_log.to_handle(Rights::SAME_RIGHTS)?,
            info: HandleInfo::new(HandleType::FileDescriptor, FDIO_FLAG_USE_FOR_STDIO),
        },
    ];
    handles.extend(startup_handles);

    let contents = MessageContents {
        args: args.into_iter().map(CString::new).collect::<Result<Vec<_>, _>>()?,
        handles,
        namespace_paths: vec![CString::new("/svc")?],
        ..Default::default()
    };

    let message = Message::build(contents)?;
    message.write(&to_child_client)?;

    // Destroy the low address space reservation VMAR now that all initial mappings (program ELF,
    // vDSO, stack) have been placed in upper address space. This frees up the lower address space
    // for sanitizers.
    //
    // SAFETY: `reserved_vmar` contains no active mappings and destroying it safely frees low
    // address space for ASan.
    unsafe {
        reserved_vmar.destroy()?;
    }

    child_process.start(&child_thread, entry_point, sp, to_child_server.into(), vdso_base)?;
    writeln!(log, "Started child process: {}", program_name)?;

    if let Some(ldsvc_server) = loader_server {
        writeln!(log, "Serving loader service for child process...")?;
        let server = UserbootLoaderServer::new(
            bootfs_parser,
            bootfs_vmo,
            vmex_resource,
            bootfs_entries,
            log,
        );
        let _ = ServerDispatcher::new(ServerEnd::<Loader, Channel>::from_untyped(ldsvc_server))
            .run(server)
            .await;
    }

    Ok(child_process)
}

/// Serves `fuchsia.ldsvc.Loader` for dynamic library loading during early boot.
struct UserbootLoaderServer<'a> {
    /// Bootfs parser used to locate dynamic libraries in the BOOTFS image.
    bootfs_parser: BootfsParser,
    /// VMO handle containing the BOOTFS filesystem image.
    bootfs_vmo: Vmo,
    /// System VMEX resource handle required to mark dynamic library VMOs as executable.
    vmex_resource: Resource,
    /// Optional vector for collected BOOTFS file VMO entries loaded via loader service.
    entries: Option<&'a mut Vec<(u32, Vmo)>>,
    /// Log handle for reporting loader errors.
    log: &'a mut Log,
    /// Configured subdirectory prefix for library resolution (e.g. "asan").
    subdir: String,
    /// Whether library lookup in fallback directory is excluded.
    exclusive: bool,
}

impl<'a> UserbootLoaderServer<'a> {
    fn new(
        bootfs_parser: BootfsParser,
        bootfs_vmo: Vmo,
        vmex_resource: Resource,
        entries: Option<&'a mut Vec<(u32, Vmo)>>,
        log: &'a mut Log,
    ) -> Self {
        Self {
            bootfs_parser,
            bootfs_vmo,
            vmex_resource,
            entries,
            log,
            subdir: String::new(),
            exclusive: false,
        }
    }
}

impl LoaderServerHandler<Channel> for UserbootLoaderServer<'_> {
    /// Called when the loader client sends `Done`.
    async fn done(&mut self) {}

    /// Loads a shared library object from the `lib/` directory in BOOTFS.
    async fn load_object(
        &mut self,
        request: Request<LoadObject, Channel>,
        responder: Responder<LoadObject, Channel>,
    ) {
        let name = &request.payload().object_name;
        let mut found = None;
        if !self.subdir.is_empty() {
            found = find_bootfs_entry(&self.bootfs_parser, &["lib", &self.subdir, name]);
        }
        if found.is_none() && (!self.exclusive || self.subdir.is_empty()) {
            found = find_bootfs_entry(&self.bootfs_parser, &["lib", name]);
        }

        let lib_vmo = found.as_ref().and_then(|entry| {
            match create_bootfs_file_vmo(
                &self.bootfs_vmo,
                entry.offset,
                entry.size,
                &self.vmex_resource,
                &Name::new_lossy(name),
                self.entries.as_deref_mut(),
            ) {
                Ok(vmo) => Some(vmo),
                Err(e) => {
                    let _ = writeln!(self.log, "Failed to load object '{name}': {e:?}");
                    None
                }
            }
        });

        let (rv, object) = lib_vmo.map_or((Err(Status::NOT_FOUND), None), |v| (Ok(()), Some(v)));
        let _ = responder.respond(LoaderLoadObjectResponse { rv, object }).await;
    }

    /// Configures the loader service search path prefix.
    async fn config(
        &mut self,
        request: Request<Config, Channel>,
        responder: Responder<Config, Channel>,
    ) {
        let payload = request.payload();
        let config_str = payload.config.as_str();
        self.exclusive = config_str.ends_with('!');
        self.subdir = config_str.strip_suffix('!').unwrap_or(config_str).to_string();
        let _ = responder.respond(Ok(())).await;
    }

    /// Clones the loader service handle (unsupported in userboot).
    async fn clone(
        &mut self,
        _request: Request<Clone, Channel>,
        responder: Responder<Clone, Channel>,
    ) {
        let _ = responder.respond(Err(Status::NOT_SUPPORTED)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use elf_parse::{
        CURRENT_ARCH, ELF_MAGIC, Elf64FileHeader, Elf64ProgramHeader, ElfClass, ElfIdent, ElfType,
        ElfVersion, NATIVE_ENCODING, SegmentFlags,
    };
    use fidl_fuchsia_kernel::{PowerResourceMarker, VmexResourceMarker};
    use fuchsia_bootfs::bootfs::ZBI_BOOTFS_MAGIC;
    use fuchsia_component::client::connect_to_protocol;
    use fuchsia_runtime::{job_default, take_startup_handle};
    use std::mem::size_of;
    use std::sync::LazyLock;
    use zerocopy::IntoBytes;

    static VDSO_VMO: LazyLock<Vmo> = LazyLock::new(|| {
        take_startup_handle(HandleInfo::new(HandleType::VdsoVmo, 0))
            .map(Vmo::from)
            .expect("expected real vDSO VMO handle from process startup handles")
    });

    async fn create_test_system_handles() -> SystemHandles {
        let root_job = job_default().duplicate_handle(Rights::SAME_RIGHTS).unwrap();
        let vmex_resource = connect_to_protocol::<VmexResourceMarker>()
            .expect("failed to connect to VmexResource protocol")
            .get()
            .await
            .expect("failed to get VmexResource");
        let power_resource = connect_to_protocol::<PowerResourceMarker>()
            .expect("failed to connect to PowerResource protocol")
            .get()
            .await
            .expect("failed to get PowerResource");
        let vdso_vmo = VDSO_VMO.duplicate_handle(Rights::SAME_RIGHTS).unwrap();
        let zbi_vmo = Vmo::create(4096).unwrap();
        let (s1, _s2) = zx::Socket::create_stream();
        let log = SystemLog::Socket(s1);
        let startup_handles = vec![];

        SystemHandles {
            root_job,
            vmex_resource,
            power_resource: Some(power_resource),
            vdso_vmo,
            zbi_vmo,
            log,
            startup_handles,
        }
    }

    fn create_bootfs_vmo(files: &[(&str, &[u8])]) -> Vmo {
        let mut total_dirsize = 0u32;
        for (name, _) in files {
            let name_len = (name.len() + 1) as u32;
            let dirent_size = (12 + name_len + 3) & !3;
            total_dirsize += dirent_size;
        }

        let data_start_offset = ((16 + total_dirsize + 4095) & !4095) as u64;
        let mut vmo_bytes = vec![0u8; data_start_offset as usize];

        // Write header
        vmo_bytes[0..4].copy_from_slice(&ZBI_BOOTFS_MAGIC.to_le_bytes());
        vmo_bytes[4..8].copy_from_slice(&total_dirsize.to_le_bytes());

        let mut current_dir_off = 16usize;
        let mut current_data_off = data_start_offset;

        for (name, data) in files {
            let name_c = format!("{}\0", name);
            let name_bytes = name_c.as_bytes();
            let name_len = name_bytes.len() as u32;
            let data_len = data.len() as u32;

            vmo_bytes[current_dir_off..current_dir_off + 4]
                .copy_from_slice(&name_len.to_le_bytes());
            vmo_bytes[current_dir_off + 4..current_dir_off + 8]
                .copy_from_slice(&data_len.to_le_bytes());
            vmo_bytes[current_dir_off + 8..current_dir_off + 12]
                .copy_from_slice(&(current_data_off as u32).to_le_bytes());
            vmo_bytes[current_dir_off + 12..current_dir_off + 12 + name_bytes.len()]
                .copy_from_slice(name_bytes);

            let dirent_size = ((12 + name_len + 3) & !3) as usize;
            current_dir_off += dirent_size;

            if vmo_bytes.len() < (current_data_off as usize + data.len()) {
                vmo_bytes.resize(current_data_off as usize + data.len(), 0);
            }
            vmo_bytes[current_data_off as usize..current_data_off as usize + data.len()]
                .copy_from_slice(data);

            let page_aligned_data_len = ((data.len() + 4095) & !4095) as u64;
            current_data_off += page_aligned_data_len.max(4096);
        }

        let vmo = Vmo::create(vmo_bytes.len() as u64).unwrap();
        vmo.write(&vmo_bytes, 0).unwrap();
        vmo
    }

    /// Generates a raw ELF64 buffer in memory with an optional PT_INTERP segment.
    fn create_elf_bytes(interp_path: Option<&str>) -> Vec<u8> {
        let has_interp = interp_path.is_some();
        let phnum = if has_interp { 2 } else { 1 };
        let ehsize = size_of::<Elf64FileHeader>() as u16;
        let phentsize = size_of::<Elf64ProgramHeader>() as u16;

        let interp_str_bytes = interp_path.map(|p| format!("{}\0", p)).unwrap_or_default();
        let interp_len = interp_str_bytes.len();

        let phoff = ehsize as usize;
        let interp_offset = phoff + (phnum as usize) * (phentsize as usize);

        let file_header = Elf64FileHeader {
            ident: ElfIdent {
                magic: ELF_MAGIC,
                class: ElfClass::Elf64 as u8,
                data: NATIVE_ENCODING as u8,
                version: ElfVersion::Current as u8,
                osabi: 0,
                abiversion: 0,
                pad: [0; 7],
            },
            elf_type: ElfType::Executable as u16,
            machine: CURRENT_ARCH as u16,
            version: 1,
            entry: 0x1000,
            phoff,
            shoff: 0,
            flags: 0,
            ehsize,
            phentsize,
            phnum,
            shentsize: 0,
            shnum: 0,
            shstrndx: 0,
        };

        let mut bytes = Vec::new();
        bytes.extend_from_slice(file_header.as_bytes());

        // PT_LOAD segment header
        let load_phdr = Elf64ProgramHeader {
            segment_type: SegmentType::Load as u32,
            flags: (SegmentFlags::READ | SegmentFlags::EXECUTE).bits(),
            offset: 0,
            vaddr: 0x1000,
            paddr: 0x1000,
            filesz: 4096,
            memsz: 4096,
            align: 4096,
        };
        bytes.extend_from_slice(load_phdr.as_bytes());

        if let Some(_) = interp_path {
            let interp_phdr = Elf64ProgramHeader {
                segment_type: SegmentType::Interp as u32,
                flags: SegmentFlags::READ.bits(),
                offset: interp_offset,
                vaddr: 0,
                paddr: 0,
                filesz: interp_len as u64,
                memsz: interp_len as u64,
                align: 1,
            };
            bytes.extend_from_slice(interp_phdr.as_bytes());
            bytes.extend_from_slice(interp_str_bytes.as_bytes());
        }

        if bytes.len() < 4096 {
            bytes.resize(4096, 0);
        }

        bytes
    }

    #[fuchsia::test]
    async fn test_launch_program_invalid_bootfs() {
        let handles = create_test_system_handles().await;
        let invalid_bootfs = Vmo::create(4096).unwrap();
        let mut log = Log::new();

        let res = launch_program(
            "test_prog",
            "bin/test_prog",
            ["arg1"],
            invalid_bootfs,
            handles,
            None,
            &mut log,
        )
        .await;

        assert!(res.is_err());
    }

    #[fuchsia::test]
    async fn test_launch_program_target_not_in_bootfs() {
        let handles = create_test_system_handles().await;
        let bootfs = create_bootfs_vmo(&[("bin/other", b"some_data")]);
        let mut log = Log::new();

        let res =
            launch_program("test_prog", "bin/test_prog", ["arg1"], bootfs, handles, None, &mut log)
                .await;

        assert!(res.is_err());
        let err_msg = res.unwrap_err().to_string();
        assert!(
            err_msg.contains("Program 'bin/test_prog' not found in bootfs"),
            "unexpected err: {err_msg}"
        );
    }

    #[fuchsia::test]
    async fn test_launch_program_corrupt_elf() {
        let handles = create_test_system_handles().await;
        let bootfs = create_bootfs_vmo(&[("bin/test_prog", b"NOT_AN_ELF_BINARY")]);
        let mut log = Log::new();

        let res =
            launch_program("test_prog", "bin/test_prog", [], bootfs, handles, None, &mut log).await;

        assert!(res.is_err());
    }

    #[fuchsia::test]
    async fn test_launch_program_missing_interpreter() {
        let handles = create_test_system_handles().await;
        let elf_data = create_elf_bytes(Some("lib/ld.so.1"));
        let bootfs = create_bootfs_vmo(&[("bin/test_prog", &elf_data)]);
        let mut log = Log::new();

        let res = launch_program(
            "test_prog",
            "bin/test_prog",
            ["--flag"],
            bootfs,
            handles,
            None,
            &mut log,
        )
        .await;

        assert!(res.is_err());
        let err_msg = res.unwrap_err().to_string();
        assert!(
            err_msg.contains("Interpreter 'lib/ld.so.1' not found in bootfs"),
            "unexpected err: {err_msg}"
        );
    }

    #[fuchsia::test]
    async fn test_launch_program_interpreter_path_normalization() {
        let handles = create_test_system_handles().await;
        // Leading slash /lib/ld.so.1 should normalize to lib/ld.so.1
        let elf_data = create_elf_bytes(Some("/lib/ld.so.1"));
        let bootfs = create_bootfs_vmo(&[("bin/test_prog", &elf_data)]);
        let mut log = Log::new();

        let res =
            launch_program("test_prog", "bin/test_prog", [], bootfs, handles, None, &mut log).await;

        assert!(res.is_err());
        let err_msg = res.unwrap_err().to_string();
        assert!(
            err_msg.contains("Interpreter 'lib/ld.so.1' not found in bootfs"),
            "unexpected err: {err_msg}"
        );

        // Naked filename "ld.so.1" should prefix with "lib/" -> "lib/ld.so.1"
        let handles2 = create_test_system_handles().await;
        let elf_data2 = create_elf_bytes(Some("ld.so.1"));
        let bootfs2 = create_bootfs_vmo(&[("bin/test_prog", &elf_data2)]);
        let mut log2 = Log::new();

        let res2 =
            launch_program("test_prog", "bin/test_prog", [], bootfs2, handles2, None, &mut log2)
                .await;

        assert!(res2.is_err());
        let err_msg2 = res2.unwrap_err().to_string();
        assert!(
            err_msg2.contains("Interpreter 'lib/ld.so.1' not found in bootfs"),
            "unexpected err: {err_msg2}"
        );
    }

    #[fuchsia::test]
    async fn test_launch_program_success() {
        let handles = create_test_system_handles().await;
        let elf_data = create_elf_bytes(None);
        let bootfs = create_bootfs_vmo(&[("bin/test_prog", &elf_data)]);
        let bootfs_dup = bootfs.duplicate_handle(Rights::SAME_RIGHTS).unwrap();
        let mut log = Log::new();

        let mut bootfs_entries = Vec::new();
        let res = launch_program(
            "test_prog",
            "bin/test_prog",
            ["arg1", "arg2"],
            bootfs,
            handles,
            Some(&mut bootfs_entries),
            &mut log,
        )
        .await;

        assert!(res.is_ok(), "expected launch_program to succeed, got: {:?}", res);
        assert_eq!(bootfs_entries.len(), 1);
        let (offset, ref entry_vmo) = bootfs_entries[0];
        let mut transferred_bytes = vec![0u8; elf_data.len()];
        entry_vmo.read(&mut transferred_bytes, 0).unwrap();
        assert_eq!(transferred_bytes, elf_data);

        // Verify that the file range in the original BOOTFS VMO was decommitted by transfer_data.
        let mut original_bytes = vec![0xffu8; elf_data.len()];
        bootfs_dup.read(&mut original_bytes, offset as u64).unwrap();
        assert!(original_bytes.iter().all(|&b| b == 0));
    }

    #[fuchsia::test]
    async fn test_launch_program_arg_with_null_byte() {
        let handles = create_test_system_handles().await;
        let elf_data = create_elf_bytes(None);
        let bootfs = create_bootfs_vmo(&[("bin/test_prog", &elf_data)]);
        let mut log = Log::new();

        let res = launch_program(
            "test_prog",
            "bin/test_prog",
            ["arg1\0with_null"],
            bootfs,
            handles,
            None,
            &mut log,
        )
        .await;

        assert!(res.is_err());
    }

    #[fuchsia::test]
    async fn test_system_handles_success() {
        let root_job = job_default().duplicate_handle(Rights::SAME_RIGHTS).unwrap();
        root_job.set_name(&Name::new("root").unwrap()).unwrap();
        let root_job_info =
            zx::HandleInfo::new(root_job.into_handle(), ObjectType::JOB, Rights::SAME_RIGHTS);

        let vmex = connect_to_protocol::<VmexResourceMarker>().unwrap().get().await.unwrap();
        let vmex_info =
            zx::HandleInfo::new(vmex.into_handle(), ObjectType::RESOURCE, Rights::SAME_RIGHTS);

        let power = connect_to_protocol::<PowerResourceMarker>().unwrap().get().await.unwrap();
        let power_info =
            zx::HandleInfo::new(power.into_handle(), ObjectType::RESOURCE, Rights::SAME_RIGHTS);

        let vdso = VDSO_VMO.duplicate_handle(Rights::SAME_RIGHTS).unwrap();
        vdso.set_name(&Name::new("vdso/stable").unwrap()).unwrap();
        let vdso_info =
            zx::HandleInfo::new(vdso.into_handle(), ObjectType::VMO, Rights::SAME_RIGHTS);

        let zbi = Vmo::create(4096).unwrap();
        zbi.set_name(&Name::new("zbi").unwrap()).unwrap();
        let zbi_info = zx::HandleInfo::new(zbi.into_handle(), ObjectType::VMO, Rights::SAME_RIGHTS);

        let (s1, _s2) = zx::Socket::create_stream();
        let log_info =
            zx::HandleInfo::new(s1.into_handle(), ObjectType::SOCKET, Rights::SAME_RIGHTS);

        let zero_vmo = Vmo::create(0).unwrap();
        zero_vmo.set_name(&Name::new("zero").unwrap()).unwrap();
        let zero_info =
            zx::HandleInfo::new(zero_vmo.into_handle(), ObjectType::VMO, Rights::SAME_RIGHTS);

        let kfile_vmo = Vmo::create(4096).unwrap();
        kfile_vmo.set_name(&Name::new("kernel_file").unwrap()).unwrap();
        let kfile_info =
            zx::HandleInfo::new(kfile_vmo.into_handle(), ObjectType::VMO, Rights::SAME_RIGHTS);

        let res = SystemHandles::from_handles(vec![
            root_job_info,
            vmex_info,
            power_info,
            vdso_info,
            zbi_info,
            log_info,
            zero_info,
            kfile_info,
        ]);
        let handles = res.expect("expected SystemHandles::from_handles to succeed");
        assert_eq!(handles.root_job.get_name().unwrap(), "root");
        assert_eq!(handles.vdso_vmo.get_name().unwrap(), "vdso/stable");
        assert_eq!(handles.zbi_vmo.get_name().unwrap(), "zbi");
        assert_eq!(handles.startup_handles.len(), 2);
    }

    #[fuchsia::test]
    fn test_find_bootfs_entry() {
        let bootfs = create_bootfs_vmo(&[
            ("ld.so.1", b"root_file"),
            ("lib/asan/libc.so", b"asan_libc"),
            ("lib/ld.so.1", b"interp"),
            ("my_lib/foo.so", b"wrong_prefix"),
            ("zbi-test/lib/asan/libfoo.so", b"prefixed_asan"),
            ("zbi-test/lib/libbar.so", b"prefixed_lib"),
        ]);
        let parser = BootfsParser::create_from_vmo(&bootfs).unwrap();

        // Exact matches at root of bootfs.
        let entry = find_bootfs_entry(&parser, &["lib", "ld.so.1"]).unwrap();
        assert_eq!(entry.name, "lib/ld.so.1");

        let entry = find_bootfs_entry(&parser, &["lib", "asan", "libc.so"]).unwrap();
        assert_eq!(entry.name, "lib/asan/libc.so");

        // Suffix matches under a prefix directory.
        let entry = find_bootfs_entry(&parser, &["lib", "asan", "libfoo.so"]).unwrap();
        assert_eq!(entry.name, "zbi-test/lib/asan/libfoo.so");

        let entry = find_bootfs_entry(&parser, &["lib", "libbar.so"]).unwrap();
        assert_eq!(entry.name, "zbi-test/lib/libbar.so");

        // Non-matches: partial component prefix or missing directory component.
        assert!(find_bootfs_entry(&parser, &["lib", "foo.so"]).is_none());
        assert!(find_bootfs_entry(&parser, &["lib", "other", "libc.so"]).is_none());
        assert!(find_bootfs_entry(&parser, &["other", "ld.so.1"]).is_none());
    }
}
