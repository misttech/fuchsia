// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::util::maur::{self, TaskWritable};
use fdio::service_connect;
use kgsl_libmagma::{Buffer, Connection, Device, QueryOutput, Semaphore, initialize_logging};
use kgsl_magma_params::{AdrenoKgslParams, MAGMA_QCOM_ADRENO_QUERY_KGSL_PARAMS};
use kgsl_range_allocator::Allocator;
use kgsl_strings::{ioctl_kgsl, kgsl_prop};
use magma::{MAGMA_QUERY_DEVICE_ID, MAGMA_QUERY_VENDOR_ID};
use starnix_core::mm::MemoryAccessorExt;
use starnix_core::task::CurrentTask;
use starnix_core::vfs::{FileObject, FileOps, FsNode};
use starnix_core::{fileops_impl_dataless, fileops_impl_nonseekable, fileops_impl_noop_sync};
use starnix_logging::{log_error, log_info, log_warn, track_stub};
use starnix_sync::{Locked, Mutex, Unlocked};
use starnix_syscalls::{SUCCESS, SyscallArg, SyscallResult};
use starnix_uapi::device_type::DeviceType;
use starnix_uapi::errors::Errno;
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::user_address::{UserAddress, UserRef};
use starnix_uapi::{errno, error, uapi};
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::Once;
use std::sync::atomic::{AtomicU32, Ordering};

#[cfg(feature = "starnix-kgsl-debug")]
#[macro_export]
macro_rules! kgsl_debug {
    ($fmt:expr $(, $arg:expr)*) => {
        log_info!("kgsl: {}:{}: {}", file!(), line!(), format_args!($fmt $(, $arg)*));
    };
}

#[cfg(not(feature = "starnix-kgsl-debug"))]
#[macro_export]
macro_rules! kgsl_debug {
    ($($arg:tt)*) => {};
}

// TODO(b/393160668): this should be PAGE_SIZE of the container
const BUFFER_ALIGNMENT: u64 = 4096;

pub struct KgslFile {
    // This member will be read in a future change. A linter attribute is used
    // instead of an underscore prefix so that field init shorthand still works.
    #[expect(dead_code)]
    device: Device,
    connection: Connection,
    adreno_kgsl_params: AdrenoKgslParams,
    // TODO(b/481419355): transition to id-map container once available
    syncsources: Mutex<HashMap<u32, Semaphore>>,
    next_syncsource_id: AtomicU32,
    // TODO(b/481419355): transition to id-map container once available
    gpuobjs: Mutex<HashMap<u32, GpuObject>>,
    next_gpuobj_id: AtomicU32,
    allocator: Mutex<Allocator>,
    #[expect(dead_code)]
    shadow: GpuObject,
    shadow_properties: uapi::kgsl_shadowprop,
}

struct GpuObject {
    #[expect(dead_code)]
    buffer: Buffer,
    flags: u64,
    size: u64,
    #[expect(dead_code)]
    mmapsize: u64,
    gpuaddr: u64,
}

impl KgslFile {
    pub fn init() {
        match Self::init_magma_logging() {
            Ok(()) => log_info!("kgsl: magma logging enabled"),
            Err(()) => log_warn!("kgsl: magma logging failed to initialize"),
        };
    }

    fn init_magma_logging() -> Result<(), ()> {
        let (client, server) = zx::Channel::create();
        service_connect("/svc/fuchsia.logger.LogSink", server).map_err(|_| ())?;
        return initialize_logging(client);
    }

    fn import_device(path: &str) -> Result<Device, zx::Status> {
        let (client, server) = zx::Channel::create();
        service_connect(&path, server)?;
        let device = Device::from_channel(client).map_err(|_| zx::Status::INTERNAL)?;
        let QueryOutput::Value(vendor_id) =
            device.query(MAGMA_QUERY_VENDOR_ID).map_err(|_| zx::Status::INTERNAL)?
        else {
            return Err(zx::Status::INTERNAL);
        };
        let QueryOutput::Value(device_id) =
            device.query(MAGMA_QUERY_DEVICE_ID).map_err(|_| zx::Status::INTERNAL)?
        else {
            return Err(zx::Status::INTERNAL);
        };

        log_info!(
            "kgsl: magma device at {} is vendor {:#04x} device {:#04x}",
            path,
            vendor_id,
            device_id
        );
        Ok(device)
    }

    pub fn new_file(
        _current_task: &CurrentTask,
        _dev: DeviceType,
        _node: &FsNode,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            Self::init();
        });
        let mut devices = std::fs::read_dir("/svc/fuchsia.gpu.magma.Service")
            .map_err(|_| errno!(ENXIO))?
            .filter_map(|x| x.ok())
            .filter_map(|entry| entry.path().join("device").into_os_string().into_string().ok())
            .filter_map(|path| Self::import_device(&path).ok());
        let device = devices.next().ok_or_else(|| errno!(ENXIO))?;
        let QueryOutput::Buffer(adreno_kgsl_params_vmo) =
            device.query(MAGMA_QCOM_ADRENO_QUERY_KGSL_PARAMS).map_err(|_| errno!(ENXIO))?
        else {
            return Err(errno!(ENXIO));
        };

        let adreno_kgsl_params = adreno_kgsl_params_vmo
            .read_to_object::<AdrenoKgslParams>(0)
            .map_err(|_| errno!(ENXIO))?;

        let connection = device.create_connection().map_err(|_| errno!(ENXIO))?;

        let mut allocator = Allocator::create(0, adreno_kgsl_params.gpu_va64_size);

        // Reserve GPU secure VA space.
        allocator
            .allocate(
                adreno_kgsl_params.gpu_secure_va_size,
                adreno_kgsl_params.secure_buf_alignment as u64,
            )
            .ok_or_else(|| errno!(ENOMEM))?;

        // Shadow buffer must immediately follow the secure VA space.
        let shadow_size = adreno_kgsl_params.device_shadow_size;
        let shadow_buffer = connection.create_buffer(shadow_size).map_err(|_| errno!(ENOMEM))?;
        let shadow_gpuaddr =
            allocator.allocate(shadow_size, BUFFER_ALIGNMENT).ok_or_else(|| errno!(ENOMEM))?;
        let shadow = GpuObject {
            buffer: shadow_buffer,
            flags: adreno_kgsl_params.device_shadow_flags.into(),
            size: shadow_size,
            mmapsize: shadow_size,
            gpuaddr: shadow_gpuaddr,
        };
        let shadow_properties = uapi::kgsl_shadowprop {
            gpuaddr: shadow_gpuaddr.try_into().map_err(|_| errno!(ENXIO))?,
            size: shadow_size.try_into().map_err(|_| errno!(ENXIO))?,
            flags: adreno_kgsl_params.device_shadow_flags,
            ..Default::default()
        };

        Ok(Box::new(Self {
            device,
            connection,
            adreno_kgsl_params,
            syncsources: Mutex::new(HashMap::new()),
            next_syncsource_id: AtomicU32::new(1),
            gpuobjs: Mutex::new(HashMap::new()),
            next_gpuobj_id: AtomicU32::new(1),
            allocator: Mutex::new(allocator),
            shadow,
            shadow_properties,
        }))
    }

    fn kgsl_device_getproperty(
        &self,
        current_task: &CurrentTask,
        arg: SyscallArg,
    ) -> Result<SyscallResult, Errno> {
        let params_ref = maur::kgsl_device_getproperty::new(current_task, arg);
        let params = current_task.read_multi_arch_object(params_ref)?;
        kgsl_debug!("kgsl_device_getproperty {:?}", params);

        let params_size: usize = params.sizebytes.try_into().map_err(|_| errno!(EINVAL))?;
        // TODO(b/393160668): check params_size against all property types

        match params.type_ {
            uapi::KGSL_PROP_DEVICE_INFO => {
                let prop_value = uapi::kgsl_devinfo {
                    device_id: self.adreno_kgsl_params.device_id,
                    chip_id: self.adreno_kgsl_params.chip_id,
                    mmu_enabled: self.adreno_kgsl_params.mmu_enabled,
                    gmem_gpubaseaddr: 0, // This field is unused by the driver.
                    gpu_id: self.adreno_kgsl_params.gpu_id,
                    gmem_sizebytes: self.adreno_kgsl_params.gmem_sizebytes,
                    ..Default::default()
                };
                kgsl_debug!("KGSL_PROP_DEVICE_INFO: {:?}", prop_value);
                prop_value.write(&current_task, params.value)
            }
            uapi::KGSL_PROP_DEVICE_SHADOW => {
                let prop_value = self.shadow_properties;
                kgsl_debug!("KGSL_PROP_DEVICE_SHADOW: {:?}", prop_value);
                prop_value.write(&current_task, params.value)
            }
            uapi::KGSL_PROP_UCHE_GMEM_VADDR => {
                let prop_value = 0u32; // Unified cache unsupported.
                kgsl_debug!("KGSL_PROP_UCHE_GMEM_VADDR: {:?}", prop_value);
                prop_value.write(&current_task, params.value)
            }
            uapi::KGSL_PROP_UCODE_VERSION => {
                let prop_value = uapi::kgsl_ucode_version {
                    pfp: self.adreno_kgsl_params.ucode_version_pfp,
                    pm4: self.adreno_kgsl_params.ucode_version_pm4,
                    ..Default::default()
                };
                kgsl_debug!("KGSL_PROP_UCODE_VERSION: {:?}", prop_value);
                prop_value.write(&current_task, params.value)
            }
            uapi::KGSL_PROP_HIGHEST_BANK_BIT => {
                let prop_value = self.adreno_kgsl_params.highest_bank_bit;
                kgsl_debug!("KGSL_PROP_HIGHEST_BANK_BIT: {:?}", prop_value);
                prop_value.write(&current_task, params.value)
            }
            uapi::KGSL_PROP_DEVICE_BITNESS => {
                let prop_value = self.adreno_kgsl_params.device_bitness;
                kgsl_debug!("KGSL_PROP_DEVICE_BITNESS: {:?}", prop_value);
                prop_value.write(&current_task, params.value)
            }
            uapi::KGSL_PROP_DEVICE_QDSS_STM => {
                // This is intentionally zero-sized to indicate lack of support.
                let prop_value =
                    uapi::kgsl_qdss_stm_prop { gpuaddr: 0, size: 0, ..Default::default() };
                kgsl_debug!("KGSL_PROP_DEVICE_QDSS_STM: {:?}", prop_value);
                prop_value.write(&current_task, params.value)
            }
            uapi::KGSL_PROP_MIN_ACCESS_LENGTH => {
                let prop_value = self.adreno_kgsl_params.min_access_length;
                kgsl_debug!("KGSL_PROP_MIN_ACCESS_LENGTH: {:?}", prop_value);
                prop_value.write(&current_task, params.value)
            }
            uapi::KGSL_PROP_UBWC_MODE => {
                let prop_value = self.adreno_kgsl_params.ubwc_mode;
                kgsl_debug!("KGSL_PROP_UBWC_MODE: {:?}", prop_value);
                prop_value.write(&current_task, params.value)
            }
            uapi::KGSL_PROP_DEVICE_QTIMER => {
                // This is intentionally zero-sized to indicate lack of support.
                let prop_value =
                    uapi::kgsl_qtimer_prop { gpuaddr: 0, size: 0, ..Default::default() };
                kgsl_debug!("KGSL_PROP_DEVICE_QTIMER: {:?}", prop_value);
                prop_value.write(&current_task, params.value)
            }
            uapi::KGSL_PROP_SECURE_BUFFER_ALIGNMENT => {
                let prop_value = self.adreno_kgsl_params.secure_buf_alignment;
                kgsl_debug!("KGSL_PROP_SECURE_BUFFER_ALIGNMENT: {:?}", prop_value);
                prop_value.write(&current_task, params.value)
            }
            uapi::KGSL_PROP_SECURE_CTXT_SUPPORT => {
                let prop_value = self.adreno_kgsl_params.secure_ctxt_support;
                kgsl_debug!("KGSL_PROP_SECURE_CTXT_SUPPORT: {:?}", prop_value);
                prop_value.write(&current_task, params.value)
            }
            uapi::KGSL_PROP_SPEED_BIN => {
                let prop_value = 0u64; // Default speed bin.
                kgsl_debug!("KGSL_PROP_SPEED_BIN: {:?}", prop_value);
                prop_value.write(&current_task, params.value)
            }
            uapi::KGSL_PROP_GAMING_BIN => {
                kgsl_debug!("KGSL_PROP_GAMING_BIN returning EINVAL");
                error!(EINVAL, "gaming bin unsupported")
            }
            uapi::KGSL_PROP_GPU_MODEL => {
                if params_size < self.adreno_kgsl_params.gpu_model.len() {
                    return error!(EINVAL);
                }
                let prop_value = self.adreno_kgsl_params.gpu_model;
                kgsl_debug!("KGSL_PROP_GPU_MODEL: {:?}", prop_value);
                let result_ref = UserRef::from(UserAddress::from(params.value));
                current_task.write_object(result_ref, &prop_value)?;
                Ok(SUCCESS)
            }
            uapi::KGSL_PROP_VK_DEVICE_ID => {
                let prop_value = self.adreno_kgsl_params.vk_device_id;
                kgsl_debug!("KGSL_PROP_VK_DEVICE_ID: {:?}", prop_value);
                prop_value.write(&current_task, params.value)
            }
            uapi::KGSL_PROP_IS_LPAC_ENABLED => {
                let prop_value = 0u32; // Asynchronous compute unsupported.
                kgsl_debug!("KGSL_PROP_IS_LPAC_ENABLED: {:?}", prop_value);
                prop_value.write(&current_task, params.value)
            }
            uapi::KGSL_PROP_GPU_VA64_SIZE => {
                let prop_value = self.adreno_kgsl_params.gpu_va64_size;
                kgsl_debug!("KGSL_PROP_GPU_VA64_SIZE: {:?}", prop_value);
                prop_value.write(&current_task, params.value)
            }
            uapi::KGSL_PROP_IS_RAYTRACING_ENABLED => {
                let prop_value = 0u32; // Raytracing unsupported.
                kgsl_debug!("KGSL_PROP_IS_RAYTRACING_ENABLED: {:?}", prop_value);
                prop_value.write(&current_task, params.value)
            }
            uapi::KGSL_PROP_IS_FASTBLEND_ENABLED => {
                let prop_value = 0u32; // Fast blend unsupported.
                kgsl_debug!("KGSL_PROP_IS_FASTBLEND_ENABLED: {:?}", prop_value);
                prop_value.write(&current_task, params.value)
            }
            uapi::KGSL_PROP_UCHE_TRAP_BASE => {
                kgsl_debug!("KGSL_PROP_UCHE_TRAP_BASE returning EINVAL");
                error!(EINVAL, "uche_trap_base unset")
            }
            uapi::KGSL_PROP_GPU_SECURE_VA_SIZE => {
                let prop_value = self.adreno_kgsl_params.gpu_secure_va_size;
                kgsl_debug!("KGSL_PROP_GPU_SECURE_VA_SIZE: {:?}", prop_value);
                prop_value.write(&current_task, params.value)
            }
            _ => {
                track_stub!(TODO("https://fxbug.dev/393160668"), "kgsl property", params.type_);
                log_error!("kgsl: unimplemented GetProperty type {}", kgsl_prop(params.type_));
                error!(ENOTSUP)
            }
        }
    }

    fn kgsl_gpuobj_alloc(
        &self,
        current_task: &CurrentTask,
        arg: SyscallArg,
    ) -> Result<SyscallResult, Errno> {
        let params_ref = maur::kgsl_gpuobj_alloc::new(current_task, arg);
        let mut params = current_task.read_multi_arch_object(params_ref)?;
        kgsl_debug!("kgsl_gpuobj_alloc {:?}", params);

        let buffer = self.connection.create_buffer(params.size).map_err(|_| errno!(ENOMEM))?;
        let size = buffer.size();

        let gpuaddr =
            self.allocator.lock().allocate(size, BUFFER_ALIGNMENT).ok_or_else(|| errno!(ENOMEM))?;

        let id = self.next_gpuobj_id.fetch_add(1, Ordering::Relaxed);
        if id == 0 {
            log_error!("kgsl: gpuobj ids exhausted");
            return error!(ENOMEM);
        }

        let gpuobj = GpuObject { buffer, flags: params.flags, size, mmapsize: size, gpuaddr };

        self.gpuobjs.lock().insert(id, gpuobj);

        params.size = size;
        params.mmapsize = size;
        params.id = id;

        current_task.write_multi_arch_object(params_ref, params)?;
        Ok(SUCCESS)
    }

    fn kgsl_gpuobj_free(
        &self,
        current_task: &CurrentTask,
        arg: SyscallArg,
    ) -> Result<SyscallResult, Errno> {
        let params_ref = maur::kgsl_gpuobj_free::new(current_task, arg);
        let params = current_task.read_multi_arch_object(params_ref)?;
        kgsl_debug!("kgsl_gpuobj_free {:?}", params);

        if let Entry::Occupied(entry) = self.gpuobjs.lock().entry(params.id) {
            self.allocator
                .lock()
                .free(entry.get().gpuaddr, entry.get().size)
                .map_err(|_| errno!(ENXIO))?;
            entry.remove();
            Ok(SUCCESS)
        } else {
            error!(EINVAL)
        }
    }

    fn kgsl_gpuobj_info(
        &self,
        current_task: &CurrentTask,
        arg: SyscallArg,
    ) -> Result<SyscallResult, Errno> {
        let params_ref = maur::kgsl_gpuobj_info::new(current_task, arg);
        let mut params = current_task.read_multi_arch_object(params_ref)?;
        kgsl_debug!("kgsl_gpuobj_info {:?}", params);

        let gpuobjs = self.gpuobjs.lock();
        let gpuobj = gpuobjs.get(&params.id).ok_or_else(|| errno!(EINVAL))?;

        params.gpuaddr = gpuobj.gpuaddr;
        params.size = gpuobj.size;
        params.flags = gpuobj.flags;
        params.va_len = gpuobj.size;
        params.va_addr = 0;

        current_task.write_multi_arch_object(params_ref, params)?;
        Ok(SUCCESS)
    }

    fn kgsl_syncsource_create(
        &self,
        current_task: &CurrentTask,
        arg: SyscallArg,
    ) -> Result<SyscallResult, Errno> {
        let params_ref = maur::kgsl_syncsource_create::new(current_task, arg);
        let mut params = current_task.read_multi_arch_object(params_ref)?;
        kgsl_debug!("kgsl_syncsource_create {:?}", params);

        let semaphore = self.connection.create_semaphore().map_err(|_| errno!(ENOMEM))?;
        let id = self.next_syncsource_id.fetch_add(1, Ordering::Relaxed);
        if id == 0 {
            // fetch_add wraps to zero on overflow. Transitioning to id-map container
            // will avoid this issue.
            log_error!("kgsl: ids exhausted");
            return error!(ENOMEM);
        }
        self.syncsources.lock().insert(id, semaphore);

        params.id = id;

        current_task.write_multi_arch_object(params_ref, params)?;
        Ok(SUCCESS)
    }

    fn kgsl_syncsource_destroy(
        &self,
        current_task: &CurrentTask,
        arg: SyscallArg,
    ) -> Result<SyscallResult, Errno> {
        let params_ref = maur::kgsl_syncsource_destroy::new(current_task, arg);
        let params = current_task.read_multi_arch_object(params_ref)?;
        kgsl_debug!("kgsl_syncsource_destroy {:?}", params);

        if self.syncsources.lock().remove(&params.id).is_some() {
            Ok(SUCCESS)
        } else {
            error!(EINVAL)
        }
    }
}

impl Drop for KgslFile {
    fn drop(&mut self) {}
}

impl FileOps for KgslFile {
    fileops_impl_dataless!();
    fileops_impl_nonseekable!();
    fileops_impl_noop_sync!();

    fn ioctl(
        &self,
        _locked: &mut Locked<Unlocked>,
        _file: &FileObject,
        current_task: &CurrentTask,
        request: u32,
        arg: SyscallArg,
    ) -> Result<SyscallResult, Errno> {
        // Special ioctl to signal container to use kgsl.
        // TODO(b/429239527): remove after transitioned
        const IOCTL_KGSL_ENABLE: u32 = 42;
        if request == IOCTL_KGSL_ENABLE {
            if cfg!(not(feature = "starnix-kgsl-enable")) {
                log_info!("kgsl: suppressing further use of kgsl");
                return error!(ENXIO);
            }
            return Ok(SUCCESS);
        }
        match crate::util::canonicalize_ioctl_request(current_task, request) {
            uapi::IOCTL_KGSL_DEVICE_GETPROPERTY => self.kgsl_device_getproperty(current_task, arg),
            uapi::IOCTL_KGSL_GPUOBJ_ALLOC => self.kgsl_gpuobj_alloc(current_task, arg),
            uapi::IOCTL_KGSL_GPUOBJ_FREE => self.kgsl_gpuobj_free(current_task, arg),
            uapi::IOCTL_KGSL_GPUOBJ_INFO => self.kgsl_gpuobj_info(current_task, arg),
            uapi::IOCTL_KGSL_SYNCSOURCE_CREATE => self.kgsl_syncsource_create(current_task, arg),
            uapi::IOCTL_KGSL_SYNCSOURCE_DESTROY => self.kgsl_syncsource_destroy(current_task, arg),
            _ => {
                track_stub!(TODO("https://fxbug.dev/393160668"), "kgsl ioctl", request);
                log_error!("kgsl: unimplemented ioctl {}", ioctl_kgsl(request));
                error!(ENOTSUP)
            }
        }
    }
}
