// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! This module provides a subset of LSM hook implementations that check access based on the
//! Linux capability bits held by the caller.
//!
//! See https://fxbug.dev/440048727 for the full set of hooks that we expect the common capabilities
//! LSM to need to implement.
//!
//! The LSM hooks layer calls these hooks from the appropriate `security::` entrypoint, and the
//! SELinux LSM may also delegate to them.  They should never be called into directly.

use crate::security;
use crate::task::loader::ResolvedElf;
use crate::task::{CurrentTask, Task};
use crate::vfs::{FsNode, FsStr, XattrOp};
use linux_uapi::XATTR_NAME_CAPS;
use starnix_uapi::auth::{
    CAP_SETFCAP, CAP_SYS_PTRACE, Capabilities, Credentials, PtraceAccessMode, SecureBits,
};
use starnix_uapi::errno;
use starnix_uapi::errors::Errno;

/// Corresponds to the `capable()` LSM hook.
pub(super) fn capable(
    current_task: &CurrentTask,
    capability: starnix_uapi::auth::Capabilities,
) -> Result<(), Errno> {
    creds_capable(&current_task.current_creds(), capability)
}

pub(super) fn creds_capable(
    creds: &Credentials,
    capability: starnix_uapi::auth::Capabilities,
) -> Result<(), Errno> {
    creds.cap_effective.contains(capability).then_some(()).ok_or_else(|| errno!(EPERM))
}

/// Corresponds to the `inode_setxattr()` LSM hook.
pub(super) fn fs_node_setxattr(
    current_task: &CurrentTask,
    _fs_node: &FsNode,
    name: &FsStr,
    _value: &FsStr,
    _op: XattrOp,
) -> Result<(), Errno> {
    if name == XATTR_NAME_CAPS.to_bytes() {
        return capable(current_task, CAP_SETFCAP);
    }
    Ok(())
}

/// Corresponds to the `inode_removexattr()` LSM hook.
pub(super) fn fs_node_removexattr(
    current_task: &CurrentTask,
    _fs_node: &FsNode,
    name: &FsStr,
) -> Result<(), Errno> {
    if name == XATTR_NAME_CAPS.to_bytes() {
        return capable(current_task, CAP_SETFCAP);
    }
    Ok(())
}

/// Validates that `tracer` has sufficient capabilities to trace `tracee` with the specified `mode`.
fn check_ptrace_access(
    current_task: &CurrentTask,
    tracer: &Task,
    tracee: &Task,
    mode: PtraceAccessMode,
) -> Result<(), Errno> {
    // From the `ptrace.2` man page description of `ptrace_access_check()`:
    //
    // The implementation of this interface in the commoncap LSM performs the following steps:
    // (5.1)  If the access mode includes PTRACE_MODE_FSCREDS, then
    //        use the caller's effective capability set in the
    //        following check; otherwise (the access mode specifies
    //        PTRACE_MODE_REALCREDS, so) use the caller's permitted
    //        capability set.
    let use_effective = mode.contains(PtraceAccessMode::FSCREDS);
    //
    // (5.2)  Deny access if neither of the following is true:
    //
    //     •  The caller and the target process are in the same
    //        user namespace, and the caller's capabilities are a
    //        superset of the target process's permitted
    //        capabilities.
    //     •  The caller has the CAP_SYS_PTRACE capability in the
    //        target process's user namespace.

    // TODO: https://fxbug.dev/322893829 - User namespaces are not yet supported in Starnix.
    let same_user_namespace = true;

    let tracer_creds = tracer.real_creds();
    let tracer_has_at_least_tracee_caps = same_user_namespace && {
        let tracer_caps =
            if use_effective { tracer_creds.cap_effective } else { tracer_creds.cap_permitted };
        tracer_caps.contains(tracee.real_creds().cap_permitted)
    };
    if !tracer_has_at_least_tracee_caps {
        security::check_creds_capable(current_task, &tracer_creds, CAP_SYS_PTRACE)?;
    }
    Ok(())
}

/// Corresponds to the `ptrace_access_check()` LSM hook.
pub(super) fn ptrace_access_check(
    current_task: &CurrentTask,
    tracee_task: &Task,
    mode: PtraceAccessMode,
) -> Result<(), Errno> {
    // Note that `check_ptrace_access()` will use the `current_task`'s real credentials, ignoring
    // any credentials overrides.
    check_ptrace_access(current_task, current_task, tracee_task, mode)
}

/// Corresponds to the `ptrace_traceme()` LSM hook.
pub(super) fn ptrace_traceme(
    current_task: &CurrentTask,
    parent_tracer_task: &Task,
) -> Result<(), Errno> {
    check_ptrace_access(current_task, parent_tracer_task, current_task, PtraceAccessMode::ATTACH)
}

/// Updates credentials based on the executable file (e.g., SUID/SGID, capabilities).
///
/// Corresponds to the `bprm_creds_from_file` LSM hook.
pub(super) fn bprm_creds_from_file(
    current_task: &CurrentTask,
    elf_state: &mut ResolvedElf,
) -> Result<(), Errno> {
    let prev = current_task.current_creds();
    let securebits = elf_state.creds.securebits;

    // From <https://man7.org/linux/man-pages/man7/capabilities.7.html>:
    //
    //   During an execve(2), the kernel calculates the new capabilities
    //   of the process using the following algorithm:
    //   P'(ambient)     = (file is privileged) ? 0 : P(ambient)
    //   P'(permitted)   = (P(inheritable) & F(inheritable)) |
    //                     (F(permitted) & P(bounding)) | P'(ambient)
    //   P'(effective)   = F(effective) ? P'(permitted) : P'(ambient)
    //   P'(inheritable) = P(inheritable)    [i.e., unchanged]
    //   P'(bounding)    = P(bounding)       [i.e., unchanged]
    // where:
    //   P()    denotes the value of a thread capability set before
    //          the execve(2)
    //   P'()   denotes the value of a thread capability set after the
    //          execve(2)
    //   F()    denotes a file capability set

    // TODO(https://fxbug.dev/328629782): Add support for file capabilities.

    // From <https://man7.org/linux/man-pages/man7/capabilities.7.html>:
    //
    //   After having performed any changes to the process effective ID
    //   that were triggered by the set-user-ID mode bit of the binary-
    //   e.g., switching the effective user ID to 0 (root) because a set-
    //   user-ID-root program was executed-the kernel calculates the file
    //   capability sets as follows:

    // From <https://man7.org/linux/man-pages/man7/capabilities.7.html>:
    //
    //   (1)  If the real or effective user ID of the process is 0 (root),
    //        then the file inheritable and permitted sets are ignored;
    //        instead they are notionally considered to be all ones (i.e.,
    //        all capabilities enabled).
    let (file_permitted, file_inheritable) = if (elf_state.creds.uid == 0
        || elf_state.creds.euid == 0)
        && !securebits.contains(SecureBits::NOROOT)
    {
        (Capabilities::all(), Capabilities::all())
    } else {
        (Capabilities::empty(), Capabilities::empty())
    };

    // From <https://man7.org/linux/man-pages/man7/capabilities.7.html>:
    //
    //   (2)  If the effective user ID of the process is 0 (root) or the
    //        file effective bit is in fact enabled, then the file
    //        effective bit is notionally defined to be one (enabled).
    let file_effective = elf_state.creds.euid == 0 && !securebits.contains(SecureBits::NOROOT);

    // TODO(https://fxbug.dev/328629782): File capabilities are honored for set-user-ID-root
    // binaries with capabilities executed by non-root users. See "Set-user-ID-root programs
    // that have file capabilities" in the man page.

    // From <https://man7.org/linux/man-pages/man7/capabilities.7.html>:
    //
    //   Ambient capabilities are cleared on execve(2) if the process's real
    //   or effective user ID is changed (e.g., executing a set-user-ID
    //   program) or if the program has file capabilities.
    //
    // In practice Linux appears to clear the ambient set if the exec is "secure", whether
    // because the caller's real & effective UIDs/GIDs differ, the new real & effective
    // UIDs/GIDs differ due to SUID/SGID, of the file has a capabilities attribute.
    if elf_state.secure_exec {
        elf_state.creds.cap_ambient = Capabilities::empty();
    }

    //   P'(permitted)   = (P(inheritable) & F(inheritable)) |
    //                     (F(permitted) & P(bounding)) | P'(ambient)
    elf_state.creds.cap_permitted = (elf_state.creds.cap_inheritable & file_inheritable)
        | (file_permitted & elf_state.creds.cap_bounding)
        | elf_state.creds.cap_ambient;

    //   P'(effective)   = F(effective) ? P'(permitted) : P'(ambient)
    elf_state.creds.cap_effective =
        if file_effective { elf_state.creds.cap_permitted } else { elf_state.creds.cap_ambient };

    elf_state.creds.securebits.remove(SecureBits::KEEP_CAPS);

    // Handle no_new_privs
    if current_task.read().no_new_privs() {
        elf_state.creds.cap_permitted &= prev.cap_permitted;
        elf_state.creds.cap_effective &= elf_state.creds.cap_permitted;
    }

    Ok(())
}
