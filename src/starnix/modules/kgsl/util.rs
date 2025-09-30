// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use starnix_core::task::CurrentTask;
use starnix_uapi::uapi;
use starnix_uapi::user_address::ArchSpecific;

#[rustfmt::skip]
pub fn canonicalize_ioctl_request(current_task: &CurrentTask, request: u32) -> u32 {
    //! Converts kgsl ioctl requests to their canonical (64-bit) values.
    if !current_task.is_arch32() {
        return request
    }
    // Some of the IOCTLS have duplicate values.
    #[allow(unreachable_patterns)]
    match request {
        // Regenerate with the following command:
        // sed -rn 's/.*(IOCTL_KGSL_[^:]*).*/uapi::arch32::\1 => uapi::\1,/p' arm64.rs
        uapi::arch32::IOCTL_KGSL_DEVICE_GETPROPERTY => uapi::IOCTL_KGSL_DEVICE_GETPROPERTY,
        uapi::arch32::IOCTL_KGSL_DEVICE_WAITTIMESTAMP => uapi::IOCTL_KGSL_DEVICE_WAITTIMESTAMP,
        uapi::arch32::IOCTL_KGSL_DEVICE_WAITTIMESTAMP_CTXTID => uapi::IOCTL_KGSL_DEVICE_WAITTIMESTAMP_CTXTID,
        uapi::arch32::IOCTL_KGSL_RINGBUFFER_ISSUEIBCMDS => uapi::IOCTL_KGSL_RINGBUFFER_ISSUEIBCMDS,
        uapi::arch32::IOCTL_KGSL_CMDSTREAM_READTIMESTAMP_OLD => uapi::IOCTL_KGSL_CMDSTREAM_READTIMESTAMP_OLD,
        uapi::arch32::IOCTL_KGSL_CMDSTREAM_READTIMESTAMP => uapi::IOCTL_KGSL_CMDSTREAM_READTIMESTAMP,
        uapi::arch32::IOCTL_KGSL_CMDSTREAM_FREEMEMONTIMESTAMP => uapi::IOCTL_KGSL_CMDSTREAM_FREEMEMONTIMESTAMP,
        uapi::arch32::IOCTL_KGSL_CMDSTREAM_FREEMEMONTIMESTAMP_OLD => uapi::IOCTL_KGSL_CMDSTREAM_FREEMEMONTIMESTAMP_OLD,
        uapi::arch32::IOCTL_KGSL_DRAWCTXT_CREATE => uapi::IOCTL_KGSL_DRAWCTXT_CREATE,
        uapi::arch32::IOCTL_KGSL_DRAWCTXT_DESTROY => uapi::IOCTL_KGSL_DRAWCTXT_DESTROY,
        uapi::arch32::IOCTL_KGSL_MAP_USER_MEM => uapi::IOCTL_KGSL_MAP_USER_MEM,
        uapi::arch32::IOCTL_KGSL_CMDSTREAM_READTIMESTAMP_CTXTID => uapi::IOCTL_KGSL_CMDSTREAM_READTIMESTAMP_CTXTID,
        uapi::arch32::IOCTL_KGSL_CMDSTREAM_FREEMEMONTIMESTAMP_CTXTID => uapi::IOCTL_KGSL_CMDSTREAM_FREEMEMONTIMESTAMP_CTXTID,
        uapi::arch32::IOCTL_KGSL_SHAREDMEM_FROM_PMEM => uapi::IOCTL_KGSL_SHAREDMEM_FROM_PMEM,
        uapi::arch32::IOCTL_KGSL_SHAREDMEM_FREE => uapi::IOCTL_KGSL_SHAREDMEM_FREE,
        uapi::arch32::IOCTL_KGSL_CFF_USER_EVENT => uapi::IOCTL_KGSL_CFF_USER_EVENT,
        uapi::arch32::IOCTL_KGSL_DRAWCTXT_BIND_GMEM_SHADOW => uapi::IOCTL_KGSL_DRAWCTXT_BIND_GMEM_SHADOW,
        uapi::arch32::IOCTL_KGSL_SHAREDMEM_FROM_VMALLOC => uapi::IOCTL_KGSL_SHAREDMEM_FROM_VMALLOC,
        uapi::arch32::IOCTL_KGSL_SHAREDMEM_FLUSH_CACHE => uapi::IOCTL_KGSL_SHAREDMEM_FLUSH_CACHE,
        uapi::arch32::IOCTL_KGSL_DRAWCTXT_SET_BIN_BASE_OFFSET => uapi::IOCTL_KGSL_DRAWCTXT_SET_BIN_BASE_OFFSET,
        uapi::arch32::IOCTL_KGSL_CMDWINDOW_WRITE => uapi::IOCTL_KGSL_CMDWINDOW_WRITE,
        uapi::arch32::IOCTL_KGSL_GPUMEM_ALLOC => uapi::IOCTL_KGSL_GPUMEM_ALLOC,
        uapi::arch32::IOCTL_KGSL_CFF_SYNCMEM => uapi::IOCTL_KGSL_CFF_SYNCMEM,
        uapi::arch32::IOCTL_KGSL_TIMESTAMP_EVENT_OLD => uapi::IOCTL_KGSL_TIMESTAMP_EVENT_OLD,
        uapi::arch32::IOCTL_KGSL_SETPROPERTY => uapi::IOCTL_KGSL_SETPROPERTY,
        uapi::arch32::IOCTL_KGSL_TIMESTAMP_EVENT => uapi::IOCTL_KGSL_TIMESTAMP_EVENT,
        uapi::arch32::IOCTL_KGSL_GPUMEM_ALLOC_ID => uapi::IOCTL_KGSL_GPUMEM_ALLOC_ID,
        uapi::arch32::IOCTL_KGSL_GPUMEM_FREE_ID => uapi::IOCTL_KGSL_GPUMEM_FREE_ID,
        uapi::arch32::IOCTL_KGSL_GPUMEM_GET_INFO => uapi::IOCTL_KGSL_GPUMEM_GET_INFO,
        uapi::arch32::IOCTL_KGSL_GPUMEM_SYNC_CACHE => uapi::IOCTL_KGSL_GPUMEM_SYNC_CACHE,
        uapi::arch32::IOCTL_KGSL_PERFCOUNTER_GET => uapi::IOCTL_KGSL_PERFCOUNTER_GET,
        uapi::arch32::IOCTL_KGSL_PERFCOUNTER_PUT => uapi::IOCTL_KGSL_PERFCOUNTER_PUT,
        uapi::arch32::IOCTL_KGSL_PERFCOUNTER_QUERY => uapi::IOCTL_KGSL_PERFCOUNTER_QUERY,
        uapi::arch32::IOCTL_KGSL_PERFCOUNTER_READ => uapi::IOCTL_KGSL_PERFCOUNTER_READ,
        uapi::arch32::IOCTL_KGSL_GPUMEM_SYNC_CACHE_BULK => uapi::IOCTL_KGSL_GPUMEM_SYNC_CACHE_BULK,
        uapi::arch32::IOCTL_KGSL_SUBMIT_COMMANDS => uapi::IOCTL_KGSL_SUBMIT_COMMANDS,
        uapi::arch32::IOCTL_KGSL_SYNCSOURCE_CREATE => uapi::IOCTL_KGSL_SYNCSOURCE_CREATE,
        uapi::arch32::IOCTL_KGSL_SYNCSOURCE_DESTROY => uapi::IOCTL_KGSL_SYNCSOURCE_DESTROY,
        uapi::arch32::IOCTL_KGSL_SYNCSOURCE_CREATE_FENCE => uapi::IOCTL_KGSL_SYNCSOURCE_CREATE_FENCE,
        uapi::arch32::IOCTL_KGSL_SYNCSOURCE_SIGNAL_FENCE => uapi::IOCTL_KGSL_SYNCSOURCE_SIGNAL_FENCE,
        uapi::arch32::IOCTL_KGSL_CFF_SYNC_GPUOBJ => uapi::IOCTL_KGSL_CFF_SYNC_GPUOBJ,
        uapi::arch32::IOCTL_KGSL_GPUOBJ_ALLOC => uapi::IOCTL_KGSL_GPUOBJ_ALLOC,
        uapi::arch32::IOCTL_KGSL_GPUOBJ_FREE => uapi::IOCTL_KGSL_GPUOBJ_FREE,
        uapi::arch32::IOCTL_KGSL_GPUOBJ_INFO => uapi::IOCTL_KGSL_GPUOBJ_INFO,
        uapi::arch32::IOCTL_KGSL_GPUOBJ_IMPORT => uapi::IOCTL_KGSL_GPUOBJ_IMPORT,
        uapi::arch32::IOCTL_KGSL_GPUOBJ_SYNC => uapi::IOCTL_KGSL_GPUOBJ_SYNC,
        uapi::arch32::IOCTL_KGSL_GPU_COMMAND => uapi::IOCTL_KGSL_GPU_COMMAND,
        uapi::arch32::IOCTL_KGSL_PREEMPTIONCOUNTER_QUERY => uapi::IOCTL_KGSL_PREEMPTIONCOUNTER_QUERY,
        uapi::arch32::IOCTL_KGSL_GPUOBJ_SET_INFO => uapi::IOCTL_KGSL_GPUOBJ_SET_INFO,
        uapi::arch32::IOCTL_KGSL_SPARSE_PHYS_ALLOC => uapi::IOCTL_KGSL_SPARSE_PHYS_ALLOC,
        uapi::arch32::IOCTL_KGSL_SPARSE_PHYS_FREE => uapi::IOCTL_KGSL_SPARSE_PHYS_FREE,
        uapi::arch32::IOCTL_KGSL_SPARSE_VIRT_ALLOC => uapi::IOCTL_KGSL_SPARSE_VIRT_ALLOC,
        uapi::arch32::IOCTL_KGSL_SPARSE_VIRT_FREE => uapi::IOCTL_KGSL_SPARSE_VIRT_FREE,
        uapi::arch32::IOCTL_KGSL_SPARSE_BIND => uapi::IOCTL_KGSL_SPARSE_BIND,
        uapi::arch32::IOCTL_KGSL_GPU_SPARSE_COMMAND => uapi::IOCTL_KGSL_GPU_SPARSE_COMMAND,
        uapi::arch32::IOCTL_KGSL_GPUMEM_BIND_RANGES => uapi::IOCTL_KGSL_GPUMEM_BIND_RANGES,
        uapi::arch32::IOCTL_KGSL_GPU_AUX_COMMAND => uapi::IOCTL_KGSL_GPU_AUX_COMMAND,
        uapi::arch32::IOCTL_KGSL_TIMELINE_CREATE => uapi::IOCTL_KGSL_TIMELINE_CREATE,
        uapi::arch32::IOCTL_KGSL_TIMELINE_WAIT => uapi::IOCTL_KGSL_TIMELINE_WAIT,
        uapi::arch32::IOCTL_KGSL_TIMELINE_QUERY => uapi::IOCTL_KGSL_TIMELINE_QUERY,
        uapi::arch32::IOCTL_KGSL_TIMELINE_SIGNAL => uapi::IOCTL_KGSL_TIMELINE_SIGNAL,
        uapi::arch32::IOCTL_KGSL_TIMELINE_FENCE_GET => uapi::IOCTL_KGSL_TIMELINE_FENCE_GET,
        uapi::arch32::IOCTL_KGSL_TIMELINE_DESTROY => uapi::IOCTL_KGSL_TIMELINE_DESTROY,
        uapi::arch32::IOCTL_KGSL_GET_FAULT_REPORT => uapi::IOCTL_KGSL_GET_FAULT_REPORT,
        uapi::arch32::IOCTL_KGSL_RECURRING_COMMAND => uapi::IOCTL_KGSL_RECURRING_COMMAND,
        uapi::arch32::IOCTL_KGSL_READ_CALIBRATED_TIMESTAMPS => uapi::IOCTL_KGSL_READ_CALIBRATED_TIMESTAMPS,
        _ => request,
    }
}

pub mod maur {
    //! Helper for MultiArchUserRef types.
    use starnix_uapi::uapi;
    use starnix_uapi::user_address::MultiArchUserRef;
    macro_rules! create_multi_arch_types {
        ($($name:ident),+ $(,)?) => {
            $(
                #[allow(non_camel_case_types)]
                pub type $name = MultiArchUserRef<uapi::$name, uapi::arch32::$name>;
            )+
        };
    }

    create_multi_arch_types!(kgsl_devinfo, kgsl_device_getproperty);
}
