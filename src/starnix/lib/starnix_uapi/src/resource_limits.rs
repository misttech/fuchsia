// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::errors::{Errno, error};
use crate::{
    FUTEX_TID_MASK, RLIM_INFINITY, RLIMIT_AS, RLIMIT_CORE, RLIMIT_CPU, RLIMIT_DATA, RLIMIT_FSIZE,
    RLIMIT_LOCKS, RLIMIT_MEMLOCK, RLIMIT_MSGQUEUE, RLIMIT_NICE, RLIMIT_NOFILE, RLIMIT_NPROC,
    RLIMIT_RSS, RLIMIT_RTPRIO, RLIMIT_RTTIME, RLIMIT_SIGPENDING, RLIMIT_STACK, rlimit, uapi,
};
use std::collections::HashMap;

/// A description of a resource.
pub struct ResourceDesc {
    /// The name of the resource.
    pub name: &'static str,

    /// The units in which limits on the resource are expressed.
    pub unit: &'static str,
}

impl ResourceDesc {
    fn new(name: &'static str, unit: &'static str) -> ResourceDesc {
        ResourceDesc { name, unit }
    }
}

#[allow(clippy::upper_case_acronyms)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Resource {
    CPU,
    FSIZE,
    DATA,
    STACK,
    CORE,
    RSS,
    NPROC,
    NOFILE,
    MEMLOCK,
    AS,
    LOCKS,
    SIGPENDING,
    MSGQUEUE,
    NICE,
    RTPRIO,
    RTTIME,
}

impl Resource {
    pub const ALL: [Resource; 16] = [
        Resource::CPU,
        Resource::FSIZE,
        Resource::DATA,
        Resource::STACK,
        Resource::CORE,
        Resource::RSS,
        Resource::NPROC,
        Resource::NOFILE,
        Resource::MEMLOCK,
        Resource::AS,
        Resource::LOCKS,
        Resource::SIGPENDING,
        Resource::MSGQUEUE,
        Resource::NICE,
        Resource::RTPRIO,
        Resource::RTTIME,
    ];

    pub fn from_raw(raw: u32) -> Result<Resource, Errno> {
        Ok(match raw {
            RLIMIT_CPU => Resource::CPU,
            RLIMIT_FSIZE => Resource::FSIZE,
            RLIMIT_DATA => Resource::DATA,
            RLIMIT_STACK => Resource::STACK,
            RLIMIT_CORE => Resource::CORE,
            RLIMIT_RSS => Resource::RSS,
            RLIMIT_NPROC => Resource::NPROC,
            RLIMIT_NOFILE => Resource::NOFILE,
            RLIMIT_MEMLOCK => Resource::MEMLOCK,
            RLIMIT_AS => Resource::AS,
            RLIMIT_LOCKS => Resource::LOCKS,
            RLIMIT_SIGPENDING => Resource::SIGPENDING,
            RLIMIT_MSGQUEUE => Resource::MSGQUEUE,
            RLIMIT_NICE => Resource::NICE,
            RLIMIT_RTPRIO => Resource::RTPRIO,
            RLIMIT_RTTIME => Resource::RTTIME,
            _ => return error!(EINVAL),
        })
    }

    pub fn desc(&self) -> ResourceDesc {
        match self {
            Resource::CPU => ResourceDesc::new("Max cpu time", "seconds"),
            Resource::FSIZE => ResourceDesc::new("Max file size", "bytes"),
            Resource::DATA => ResourceDesc::new("Max data size", "bytes"),
            Resource::STACK => ResourceDesc::new("Max stack size", "bytes"),
            Resource::CORE => ResourceDesc::new("Max core file size", "bytes"),
            Resource::RSS => ResourceDesc::new("Max resident set", "bytes"),
            Resource::NPROC => ResourceDesc::new("Max processes", "processes"),
            Resource::NOFILE => ResourceDesc::new("Max open files", "files"),
            Resource::MEMLOCK => ResourceDesc::new("Max locked memory", "bytes"),
            Resource::AS => ResourceDesc::new("Max address space", "bytes"),
            Resource::LOCKS => ResourceDesc::new("Max file locks", "bytes"),
            Resource::SIGPENDING => ResourceDesc::new("Max pending signals", "signals"),
            Resource::MSGQUEUE => ResourceDesc::new("Max msgqueue size", "bytes"),
            Resource::NICE => ResourceDesc::new("Max nice priority", ""),
            Resource::RTPRIO => ResourceDesc::new("Max realtime priority", ""),
            Resource::RTTIME => ResourceDesc::new("Max realtime timeout", "us"),
        }
    }
}

impl std::str::FromStr for Resource {
    type Err = Errno;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Ok(match name {
            "CPU" => Resource::CPU,
            "FSIZE" => Resource::FSIZE,
            "DATA" => Resource::DATA,
            "STACK" => Resource::STACK,
            "CORE" => Resource::CORE,
            "RSS" => Resource::RSS,
            "NPROC" => Resource::NPROC,
            "NOFILE" => Resource::NOFILE,
            "MEMLOCK" => Resource::MEMLOCK,
            "AS" => Resource::AS,
            "LOCKS" => Resource::LOCKS,
            "SIGPENDING" => Resource::SIGPENDING,
            "MSGQUEUE" => Resource::MSGQUEUE,
            "NICE" => Resource::NICE,
            "RTPRIO" => Resource::RTPRIO,
            "RTTIME" => Resource::RTTIME,
            _ => return error!(EINVAL),
        })
    }
}

#[derive(Debug, Clone)]
pub struct ResourceLimits {
    values: HashMap<Resource, rlimit>,
}

const INFINITE_LIMIT: rlimit =
    rlimit { rlim_cur: RLIM_INFINITY as u64, rlim_max: RLIM_INFINITY as u64 };

/// Default hard limit for [`RLIMIT_NOFILE`] (rather than [`uapi::INR_OPEN_MAX`]).
const DEFAULT_NOFILE_MAX: u64 = 524288;

/// Calculates system default for `/proc/sys/kernel/threads-max` from total RAM in bytes.
///
/// Per `proc_sys_kernel(5)`, `/proc/sys/kernel/threads-max`:
///   "This file specifies the system-wide limit on the number of threads (tasks)
///   that can be created on the system."
///   ...
///   "If the thread structures would occupy too much (more than 1/8th) of the
///   available RAM pages, threads-max is reduced accordingly."
///
/// Align with Linux, which budgets 16 KiB per thread stack regardless of the
/// system page size, limiting total thread stacks to 1/8th of available RAM and
/// clamping `threads-max` to `[20, `[`FUTEX_TID_MASK`]`]`.
pub fn default_threads_max(total_ram_bytes: u64) -> u64 {
    const MIN_THREADS: u64 = 20;
    const THREAD_STACK_SIZE: u64 = 16384;
    (total_ram_bytes / (8 * THREAD_STACK_SIZE)).clamp(MIN_THREADS, FUTEX_TID_MASK as u64)
}

const DEFAULT_LIMITS: [(Resource, rlimit); 7] = [
    (Resource::STACK, rlimit { rlim_cur: uapi::_STK_LIM as u64, rlim_max: RLIM_INFINITY as u64 }),
    (Resource::CORE, rlimit { rlim_cur: 0, rlim_max: RLIM_INFINITY as u64 }),
    (
        Resource::NOFILE,
        rlimit { rlim_cur: uapi::INR_OPEN_CUR as u64, rlim_max: DEFAULT_NOFILE_MAX },
    ),
    (
        Resource::MEMLOCK,
        rlimit { rlim_cur: uapi::MLOCK_LIMIT as u64, rlim_max: uapi::MLOCK_LIMIT as u64 },
    ),
    (
        Resource::MSGQUEUE,
        rlimit { rlim_cur: uapi::MQ_BYTES_MAX as u64, rlim_max: uapi::MQ_BYTES_MAX as u64 },
    ),
    (Resource::NICE, rlimit { rlim_cur: 0, rlim_max: 0 }),
    (Resource::RTPRIO, rlimit { rlim_cur: 0, rlim_max: 0 }),
];

// NB: ResourceLimits objects are accessed using a lock.  To avoid the risk of
// deadlock, we try to avoid lock acquisition in ResourceLimits.
impl ResourceLimits {
    /// Initializes default resource limits, scaling [`RLIMIT_NPROC`] and [`RLIMIT_SIGPENDING`]
    /// to half of [`default_threads_max`] for `total_ram_bytes`.
    pub fn from_total_ram(total_ram_bytes: u64) -> Self {
        let mut limits = Self { values: Default::default() };
        for (resource, value) in DEFAULT_LIMITS {
            limits.set(resource, value);
        }
        let scaled_limit = default_threads_max(total_ram_bytes) / 2;
        limits.set(Resource::NPROC, rlimit { rlim_cur: scaled_limit, rlim_max: scaled_limit });
        limits.set(Resource::SIGPENDING, rlimit { rlim_cur: scaled_limit, rlim_max: scaled_limit });
        limits
    }

    pub fn get(&self, resource: Resource) -> rlimit {
        *self.values.get(&resource).unwrap_or(&INFINITE_LIMIT)
    }

    pub fn set(&mut self, resource: Resource, value: rlimit) {
        self.values.insert(resource, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[::fuchsia::test]
    fn test_resource_from_str() {
        for resource in Resource::ALL {
            let name = format!("{resource:?}");
            assert_eq!(name.parse::<Resource>(), Ok(resource));
        }
        assert!("".parse::<Resource>().is_err());
        assert!("RLIMIT_NOFILE".parse::<Resource>().is_err());
        assert!("UNKNOWN".parse::<Resource>().is_err());
    }

    #[::fuchsia::test]
    fn test_threads_max_and_scaled_limits() {
        // 4 GiB RAM / (8 * 16 KiB per thread) = 32,768 threads-max.
        // nproc / sigpending = 32,768 / 2 = 16,384.
        let ram_4gb = 4 * 1024 * 1024 * 1024;
        assert_eq!(default_threads_max(ram_4gb), 32768);

        let limits = ResourceLimits::from_total_ram(ram_4gb);
        let nproc = limits.get(Resource::NPROC);
        assert_eq!(nproc.rlim_cur, 16384);
        assert_eq!(nproc.rlim_max, 16384);

        let sigpending = limits.get(Resource::SIGPENDING);
        assert_eq!(sigpending.rlim_cur, 16384);
        assert_eq!(sigpending.rlim_max, 16384);

        let nofile = limits.get(Resource::NOFILE);
        assert_eq!(nofile.rlim_cur, 1024);
        assert_eq!(nofile.rlim_max, 524288);

        let memlock = limits.get(Resource::MEMLOCK);
        assert_eq!(memlock.rlim_cur, 8 * 1024 * 1024);
        assert_eq!(memlock.rlim_max, 8 * 1024 * 1024);

        let stack = limits.get(Resource::STACK);
        assert_eq!(stack.rlim_cur, 8 * 1024 * 1024);
        assert_eq!(stack.rlim_max, RLIM_INFINITY as u64);
    }

    #[::fuchsia::test]
    fn test_threads_max_clamping() {
        assert_eq!(default_threads_max(0), 20);
        assert_eq!(default_threads_max(1024), 20);
        assert_eq!(default_threads_max(u64::MAX), FUTEX_TID_MASK as u64);
    }
}
