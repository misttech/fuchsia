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

use crate::security::{self, NonPrivateFsNode};
use crate::task::loader::ResolvedProgram;
use crate::task::{CurrentTask, Task};
use crate::vfs::{FsStr, ValueOrSize, XattrOp};
use linux_uapi::{
    VFS_CAP_FLAGS_EFFECTIVE, VFS_CAP_REVISION_2, VFS_CAP_REVISION_3, XATTR_NAME_CAPS, vfs_cap_data,
    vfs_cap_data__bindgen_ty_1, vfs_ns_cap_data, vfs_ns_cap_data__bindgen_ty_1,
};
use starnix_uapi::auth::{CAP_SETFCAP, Capabilities, Credentials, PtraceAccessMode, SecureBits};
use starnix_uapi::errors::Errno;
use starnix_uapi::mount_flags::MountFlags;
use starnix_uapi::{errno, error, uid_t};
use std::sync::atomic::{AtomicBool, Ordering};
use zerocopy::{FromBytes as _, IntoBytes as _};

/// Node security state maintained by the common capabilities LSM.
///
/// Caches whether an [`FsNode`] is known not to have a `security.capability` extended attribute,
/// avoiding `FsNodeOps::get_xattr` and `FsNodeOps::remove_xattr` calls on every `execve(2)`,
/// `write(2)`, `truncate(2)`, and `chown(2)` for unprivileged files.
// TODO(https://fxbug.dev/565936266): Serialize `security.*` extended attribute mutations with a
// per-node lock held across the underlying `FsNodeOps` call, and evolve this state to cache
// `{ Unknown, Known(Option<FileCapabilities>) }` so lock-free reads only transition from
// `Unknown` to `Known`.
#[derive(Debug, Default)]
pub(super) struct FsNodeState {
    /// `true` if `fs_node` has been observed to have no `security.capability` extended attribute
    /// (or resides on a filesystem that does not support extended attributes). Defaults to `false`
    /// so that pre-existing on-disk inodes are checked on first access.
    has_no_capabilities: AtomicBool,
}

/// File capabilities parsed from a `security.capability` extended attribute.
struct FileCapabilities {
    permitted: Capabilities,
    inheritable: Capabilities,
    effective: bool,
    /// Root user ID of the user namespace to which the file capabilities are scoped, or `0` for
    /// [`VFS_CAP_REVISION_2`] attributes (which apply to the initial user namespace).
    ///
    /// From the `VFS_CAP_REVISION_3` section of
    /// <https://man7.org/linux/man-pages/man7/capabilities.7.html>:
    ///
    ///   As with version 2 file capabilities, version 3 capability masks are 64
    ///   bits in size.  But in addition, the root user ID of namespace is encoded
    ///   in the security.capability extended attribute.  (A namespace's root user
    ///   ID is the value that user ID 0 inside that namespace maps to in the
    ///   initial user namespace.)
    rootid: uid_t,
}

impl FileCapabilities {
    const fn empty() -> Self {
        Self {
            permitted: Capabilities::empty(),
            inheritable: Capabilities::empty(),
            effective: false,
            rootid: 0,
        }
    }

    /// Parses and validates a V2 ([`vfs_cap_data`]) or V3 ([`vfs_ns_cap_data`]) extended attribute.
    fn from_bytes(_current_task: &CurrentTask, bytes: &[u8]) -> Result<Self, Errno> {
        // From <https://man7.org/linux/man-pages/man7/capabilities.7.html>:
        //
        //   VFS_CAP_REVISION_2 [...] allows for file capability masks that are 64
        //   bits in size [...].
        //   VFS_CAP_REVISION_3 [...] are analogous to version 2 capabilities,
        //   with the difference that the version 3 security.capability extended
        //   attribute also includes the root user ID of the namespace [...].
        if let Ok(v2) = vfs_cap_data::read_from_bytes(bytes) {
            let effective = validate_cap_magic(u32::from_le(v2.magic_etc), VFS_CAP_REVISION_2)?;
            return Ok(Self {
                permitted: Capabilities::from_abi_v3((
                    u32::from_le(v2.data[0].permitted),
                    u32::from_le(v2.data[1].permitted),
                )),
                inheritable: Capabilities::from_abi_v3((
                    u32::from_le(v2.data[0].inheritable),
                    u32::from_le(v2.data[1].inheritable),
                )),
                effective,
                rootid: 0,
            });
        }
        if let Ok(v3) = vfs_ns_cap_data::read_from_bytes(bytes) {
            let effective = validate_cap_magic(u32::from_le(v3.magic_etc), VFS_CAP_REVISION_3)?;
            return Ok(Self {
                permitted: Capabilities::from_abi_v3((
                    u32::from_le(v3.data[0].permitted),
                    u32::from_le(v3.data[1].permitted),
                )),
                inheritable: Capabilities::from_abi_v3((
                    u32::from_le(v3.data[0].inheritable),
                    u32::from_le(v3.data[1].inheritable),
                )),
                effective,
                rootid: u32::from_le(v3.rootid),
            });
        }
        error!(EINVAL)
    }

    /// Serializes the file capabilities for `current_task`'s user namespace.
    ///
    /// If `rootid` maps to UID 0 in `current_task`'s user namespace, emits a 20-byte V2
    /// ([`vfs_cap_data`]) attribute; otherwise emits a 24-byte V3 ([`vfs_ns_cap_data`]) attribute.
    fn into_bytes(&self, _current_task: &CurrentTask) -> Result<Box<[u8]>, Errno> {
        // From <https://man7.org/linux/man-pages/man7/capabilities.7.html>:
        //
        //   When a version 3 security.capability attribute is retrieved [...] by
        //   a process that resides inside a user namespace that was created by
        //   the root user ID [...], the returned attribute is [...] simplified
        //   to appear as a version 2 attribute [...].
        //
        // TODO(https://fxbug.dev/322893829): Map rootid into current_task's user namespace (and
        // return EOVERFLOW if unmapped) once user namespaces are supported.
        let effective_flag = if self.effective { VFS_CAP_FLAGS_EFFECTIVE } else { 0 };
        let (permitted_low, permitted_high) = self.permitted.as_abi_v3();
        let (inheritable_low, inheritable_high) = self.inheritable.as_abi_v3();
        if self.rootid == 0 {
            let v2 = vfs_cap_data {
                magic_etc: (VFS_CAP_REVISION_2 | effective_flag).to_le(),
                data: [
                    vfs_cap_data__bindgen_ty_1 {
                        permitted: permitted_low.to_le(),
                        inheritable: inheritable_low.to_le(),
                    },
                    vfs_cap_data__bindgen_ty_1 {
                        permitted: permitted_high.to_le(),
                        inheritable: inheritable_high.to_le(),
                    },
                ],
            };
            Ok(v2.as_bytes().into())
        } else {
            let v3 = vfs_ns_cap_data {
                magic_etc: (VFS_CAP_REVISION_3 | effective_flag).to_le(),
                data: [
                    vfs_ns_cap_data__bindgen_ty_1 {
                        permitted: permitted_low.to_le(),
                        inheritable: inheritable_low.to_le(),
                    },
                    vfs_ns_cap_data__bindgen_ty_1 {
                        permitted: permitted_high.to_le(),
                        inheritable: inheritable_high.to_le(),
                    },
                ],
                rootid: self.rootid.to_le(),
            };
            Ok(v3.as_bytes().into())
        }
    }
}

/// Validates `magic_etc` against `expected_revision` and checks for unsupported flag bits.
///
/// Returns the `effective` flag bit ([`VFS_CAP_FLAGS_EFFECTIVE`]) if valid.
fn validate_cap_magic(magic: u32, expected_revision: u32) -> Result<bool, Errno> {
    if (magic & !VFS_CAP_FLAGS_EFFECTIVE) != expected_revision {
        return error!(EINVAL);
    }
    Ok((magic & VFS_CAP_FLAGS_EFFECTIVE) != 0)
}

/// Reads and validates a `security.capability` extended attribute from [`NonPrivateFsNode`].
///
/// Only V2 ([`VFS_CAP_REVISION_2`], 20 bytes) and V3 ([`VFS_CAP_REVISION_3`], 24 bytes) capability
/// attributes are supported; V1 ([`linux_uapi::VFS_CAP_REVISION_1`], 12 bytes) is rejected with
/// `EINVAL`. Returns `Ok(None)` if no capability extended attribute is present on the node, or if
/// the filesystem does not support extended attributes.
fn read_file_capabilities(
    current_task: &CurrentTask,
    fs_node: NonPrivateFsNode<'_>,
) -> Result<Option<FileCapabilities>, Errno> {
    if fs_node.common_cap_state().has_no_capabilities.load(Ordering::Relaxed) {
        return Ok(None);
    }

    let raw_value = match fs_node.ops().get_xattr(
        &fs_node,
        current_task,
        XATTR_NAME_CAPS.to_bytes().into(),
        std::mem::size_of::<vfs_ns_cap_data>(),
    ) {
        Ok(ValueOrSize::Value(bytes)) => bytes,
        Ok(ValueOrSize::Size(_)) => return error!(EINVAL),
        Err(err) if err == errno!(ERANGE) => return error!(EINVAL),
        Err(err) if err == errno!(ENODATA) || err == errno!(ENOTSUP) || err == errno!(ENOSYS) => {
            // TODO(https://fxbug.dev/565936266): Synchronize with concurrent `setxattr` calls.
            fs_node.common_cap_state().has_no_capabilities.store(true, Ordering::Relaxed);
            return Ok(None);
        }
        Err(err) => return Err(err),
    };

    let caps = FileCapabilities::from_bytes(current_task, &raw_value)?;
    Ok(Some(caps))
}

/// Corresponds to the `capable()` LSM hook.
pub(super) fn capable(current_task: &CurrentTask, capability: Capabilities) -> Result<(), Errno> {
    creds_capable(&current_task.current_creds(), capability)
}

pub(super) fn creds_capable(creds: &Credentials, capability: Capabilities) -> Result<(), Errno> {
    creds.cap_effective.contains(capability).then_some(()).ok_or_else(|| errno!(EPERM))
}

/// Corresponds to the `inode_setxattr()` LSM hook.
pub(super) fn fs_node_setxattr(
    current_task: &CurrentTask,
    _fs_node: NonPrivateFsNode<'_>,
    name: &FsStr,
    _value: &[u8],
    _op: XattrOp,
) -> Result<(), Errno> {
    // From <https://man7.org/linux/man-pages/man7/capabilities.7.html>:
    //
    //   The file capability sets are stored in an extended attribute [...] named
    //   security.capability.  Writing to this extended attribute requires the
    //   CAP_SETFCAP capability.
    if name == XATTR_NAME_CAPS.to_bytes() {
        return capable(current_task, CAP_SETFCAP);
    }
    Ok(())
}

/// Reads a `security.capability` extended attribute from [`NonPrivateFsNode`], coercing V3
/// attributes whose `rootid` maps to UID 0 in the reader's user namespace to V2. Returns `Ok(None)`
/// for non-capability attributes.
///
/// Corresponds to the `inode_getsecurity()` LSM hook.
pub(super) fn fs_node_getsecurity(
    current_task: &CurrentTask,
    fs_node: NonPrivateFsNode<'_>,
    name: &FsStr,
    max_size: usize,
) -> Result<Option<ValueOrSize<Box<[u8]>>>, Errno> {
    if name != XATTR_NAME_CAPS.to_bytes() {
        return Ok(None);
    }

    let caps = read_file_capabilities(current_task, fs_node)?.ok_or_else(|| errno!(ENODATA))?;
    let serialized = caps.into_bytes(current_task)?;
    if max_size == 0 {
        return Ok(Some(ValueOrSize::Size(serialized.len())));
    }
    if max_size < serialized.len() {
        return error!(ERANGE);
    }
    Ok(Some(ValueOrSize::Value(serialized)))
}

/// Validates and normalizes a `security.capability` extended attribute via [`FileCapabilities`]
/// before storing it on [`NonPrivateFsNode`], coercing V3 attributes with `rootid == 0` in the
/// initial user namespace to V2. Returns `Ok(None)` for non-capability attributes.
///
/// Corresponds to the `inode_setsecurity()` LSM hook.
pub(super) fn fs_node_setsecurity(
    current_task: &CurrentTask,
    fs_node: NonPrivateFsNode<'_>,
    name: &FsStr,
    value: &[u8],
    op: XattrOp,
) -> Result<Option<()>, Errno> {
    if name == XATTR_NAME_CAPS.to_bytes() {
        let caps = FileCapabilities::from_bytes(current_task, value)?;
        let serialized = caps.into_bytes(current_task)?;
        fs_node.ops().set_xattr(&fs_node, current_task, name, &serialized, op)?;
        fs_node.common_cap_state().has_no_capabilities.store(false, Ordering::Relaxed);
        return Ok(Some(()));
    }
    Ok(None)
}

/// Checks whether the generic [`starnix_uapi::auth::CAP_SYS_ADMIN`] check should be skipped
/// for `name`.
pub(super) fn fs_node_xattr_skipcap(name: &FsStr) -> bool {
    // Access to security.capability is controlled by the commoncap LSM hooks rather than the
    // generic VFS CAP_SYS_ADMIN check.
    name == XATTR_NAME_CAPS.to_bytes()
}

/// Corresponds to the `inode_removexattr()` LSM hook.
pub(super) fn fs_node_removexattr(
    current_task: &CurrentTask,
    _fs_node: NonPrivateFsNode<'_>,
    name: &FsStr,
) -> Result<(), Errno> {
    if name == XATTR_NAME_CAPS.to_bytes() {
        return capable(current_task, CAP_SETFCAP);
    }
    Ok(())
}

/// Checks whether `fs_node` has privileges (such as file capabilities) that must be cleared on
/// modification.
///
/// Corresponds to the `inode_need_killpriv()` LSM hook.
pub(super) fn fs_node_needs_killpriv(
    current_task: &CurrentTask,
    fs_node: NonPrivateFsNode<'_>,
) -> Result<bool, Errno> {
    Ok(read_file_capabilities(current_task, fs_node)?.is_some())
}

/// Removes any `security.capability` extended attribute from `fs_node`.
///
/// Corresponds to the `inode_killpriv()` LSM hook.
pub(super) fn fs_node_killpriv(
    current_task: &CurrentTask,
    fs_node: NonPrivateFsNode<'_>,
) -> Result<(), Errno> {
    match fs_node.ops().remove_xattr(&fs_node, current_task, XATTR_NAME_CAPS.to_bytes().into()) {
        Ok(()) => {
            fs_node.common_cap_state().has_no_capabilities.store(true, Ordering::Relaxed);
            Ok(())
        }
        Err(err) if err == errno!(ENODATA) || err == errno!(ENOTSUP) || err == errno!(ENOSYS) => {
            fs_node.common_cap_state().has_no_capabilities.store(true, Ordering::Relaxed);
            Ok(())
        }
        Err(err) => Err(err),
    }
}

/// Validates that `tracer` has sufficient capabilities to trace `tracee` with the specified `mode`.
fn check_ptrace_access(
    current_task: &CurrentTask,
    tracer: &Task,
    tracee: &Task,
    mode: PtraceAccessMode,
) -> Result<(), Errno> {
    // From <https://man7.org/linux/man-pages/man2/ptrace.2.html> description of
    // `ptrace_access_check()`:
    //
    //   The implementation of this interface in the commoncap LSM performs the
    //   following steps:
    //   (5.1)  If the access mode includes PTRACE_MODE_FSCREDS, then use the
    //          caller's effective capability set in the following check;
    //          otherwise (the access mode specifies PTRACE_MODE_REALCREDS, so)
    //          use the caller's permitted capability set.
    let use_effective = mode.contains(PtraceAccessMode::FSCREDS);
    //   (5.2)  Deny access if neither of the following is true:
    //          •  The caller and the target process are in the same user
    //             namespace, and the caller's capabilities are a superset of
    //             the target process's permitted capabilities.
    //          •  The caller has the CAP_SYS_PTRACE capability in the target
    //             process's user namespace.

    // TODO: https://fxbug.dev/322893829 - User namespaces are not yet supported in Starnix.
    let same_user_namespace = true;

    let tracer_creds = tracer.real_creds();
    let tracer_has_at_least_tracee_caps = same_user_namespace && {
        let tracer_caps =
            if use_effective { tracer_creds.cap_effective } else { tracer_creds.cap_permitted };
        tracer_caps.contains(tracee.real_creds().cap_permitted)
    };
    if !tracer_has_at_least_tracee_caps {
        security::check_creds_ptrace_capable(current_task, &tracer_creds, mode)?;
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
/// Corresponds to the `bprm_creds_from_file()` LSM hook.
pub(super) fn bprm_creds_from_file(
    current_task: &CurrentTask,
    resolved_program: &mut ResolvedProgram,
    mask_caps: bool,
) -> Result<(), Errno> {
    let securebits = resolved_program.creds.securebits;

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

    // From <https://man7.org/linux/man-pages/man2/execve.2.html>:
    //
    //   The set-user-ID and set-group-ID mode bits of a program are ignored if
    //   any of the following is true:
    //   ...
    //   •  the underlying filesystem is mounted nosuid (the MS_NOSUID flag for
    //      mount(2)); or
    //   ...
    //   The capabilities of the program file (see capabilities(7)) are also
    //   ignored if any of the above are true.
    let nosuid = resolved_program.file.name().mount.flags().contains(MountFlags::NOSUID);
    let file_caps = if nosuid {
        None
    } else if let Some(executable_node) =
        NonPrivateFsNode::assert_from(resolved_program.file.node())
    {
        // From <https://man7.org/linux/man-pages/man7/capabilities.7.html>:
        //
        //   [...] a binary with VFS_CAP_REVISION_3 file capabilities confers
        //   capabilities to a process during execve().  However, capabilities
        //   are conferred only if the binary is executed by a process that
        //   resides in a user namespace whose UID 0 maps to the root user ID
        //   that is saved in the extended attribute [...].
        //
        // TODO(https://fxbug.dev/322893829): Check against caller's user namespace root once supported.
        read_file_capabilities(current_task, executable_node)?.filter(|caps| caps.rootid == 0)
    } else {
        None
    };
    let has_file_caps = file_caps.is_some();

    // If the binary has the file effective bit set (capability-dumb binary), capture the
    // required permitted set before applying root overrides.
    let required_permitted = file_caps.as_ref().filter(|c| c.effective).map(|c| c.permitted);

    // From <https://man7.org/linux/man-pages/man7/capabilities.7.html>:
    //
    //   After having performed any changes to the process effective ID that were
    //   triggered by the set-user-ID mode bit of the binary, the kernel
    //   calculates the file capability sets as follows:
    //
    //   (1)  If the real or effective user ID of the process is 0 (root), then
    //        the file inheritable and permitted sets are ignored; instead they
    //        are notionally considered to be all ones (i.e., all capabilities
    //        enabled).  (There is one exception to this behavior, described in
    //        Set-user-ID-root programs that have file capabilities below.)
    //   (2)  If the effective user ID of the process is 0 (root) or the file
    //        effective bit is in fact enabled, then the file effective bit is
    //        notionally defined to be one (enabled).
    //
    //   Set-user-ID-root programs that have file capabilities
    //
    //   There is one exception to the behavior described in Capabilities and
    //   execution of programs by root above.  If (a) the binary that is being
    //   executed has capabilities attached and (b) the real user ID of the
    //   process is not 0 (root) and (c) the effective user ID of the process is
    //   0 (root), then the file capability bits are honored (i.e., they are not
    //   notionally considered to be all ones).
    let mut file_caps = file_caps.unwrap_or_else(FileCapabilities::empty);
    let is_root =
        resolved_program.creds.uid == 0 || (!has_file_caps && resolved_program.creds.euid == 0);
    if is_root && !securebits.contains(SecureBits::NOROOT) {
        file_caps.permitted = Capabilities::all();
        file_caps.inheritable = Capabilities::all();
        if resolved_program.creds.euid == 0 {
            file_caps.effective = true;
        }
    }

    // From <https://man7.org/linux/man-pages/man7/capabilities.7.html>:
    //
    //   Executing a program that changes UID or GID due to the set-user-ID or
    //   set-group-ID bits or executing a program that has any file capabilities
    //   set will clear the ambient set.
    //
    // Observed:
    //
    //   The ambient set is also cleared whenever `uid != euid` or `gid != egid`
    //   prior to `execve(2)`, even when executing an unprivileged binary.
    //
    // Note that `resolved_program.secure_exec` may already be set by an LSM (e.g.
    // SELinux denying `noatsecure`), which does not clear `cap_ambient`.
    let pre_exec_creds = current_task.current_creds();
    let file_is_privileged = resolved_program.creds.euid != pre_exec_creds.euid
        || resolved_program.creds.egid != pre_exec_creds.egid;
    let is_dac_secure_exec = file_is_privileged
        || resolved_program.creds.uid != resolved_program.creds.euid
        || resolved_program.creds.gid != resolved_program.creds.egid;

    if is_dac_secure_exec || has_file_caps {
        resolved_program.creds.cap_ambient = Capabilities::empty();
    }
    resolved_program.secure_exec |= is_dac_secure_exec;

    //   P'(permitted)   = (P(inheritable) & F(inheritable)) |
    //                     (F(permitted) & P(bounding)) | P'(ambient)
    resolved_program.creds.cap_permitted = (resolved_program.creds.cap_inheritable
        & file_caps.inheritable)
        | (file_caps.permitted & resolved_program.creds.cap_bounding)
        | resolved_program.creds.cap_ambient;

    // From <https://man7.org/linux/man-pages/man2/execve.2.html>:
    //
    //   The set-user-ID and set-group-ID mode bits of a program are ignored if
    //   any of the following is true:
    //   •  the no_new_privs attribute is set for the calling thread (see
    //      prctl(2));
    //   ...
    //   •  the process is being traced, the tracer does not have the
    //      CAP_SYS_PTRACE capability in the user namespace of the tracee's
    //      process, and the tracing process does not share its user namespace
    //      with the tracee.
    //
    //   The capabilities of the program file (see capabilities(7)) are also
    //   ignored if any of the above are true.
    //
    // Observed:
    //
    //   Unlike `MS_NOSUID` (which ignores `file_caps`), `no_new_privs`,
    //   unprivileged `ptrace`, and shared filesystem context (`CLONE_FS`)
    //   intersect `P'(permitted)` with `P(permitted)` and suppress `EPERM`,
    //   retaining file capabilities already held by the caller.
    if mask_caps {
        resolved_program.creds.cap_permitted &= pre_exec_creds.cap_permitted;
    }

    // From <https://man7.org/linux/man-pages/man7/capabilities.7.html>:
    //
    //   The kernel recognizes a file which has the file effective bit set as
    //   capability-dumb for the purpose of the check described here.  When
    //   executing a capability-dumb binary, the kernel checks if the process
    //   obtained all permitted capabilities that were specified in the file
    //   permitted set, after the capability transformations described above have
    //   been performed.  If the process did not obtain the full set of file
    //   permitted capabilities, then execve(2) fails with the error EPERM.
    if !mask_caps
        && let Some(required) = required_permitted
        && !resolved_program.creds.cap_permitted.contains(required)
    {
        return error!(EPERM);
    }

    //   P'(effective)   = F(effective) ? P'(permitted) : P'(ambient)
    resolved_program.creds.cap_effective = if file_caps.effective {
        resolved_program.creds.cap_permitted
    } else {
        resolved_program.creds.cap_ambient
    };
    debug_assert!(
        resolved_program.creds.cap_permitted.contains(resolved_program.creds.cap_effective)
    );

    resolved_program.creds.securebits.remove(SecureBits::KEEP_CAPS);

    // From <https://man7.org/linux/man-pages/man3/getauxval.3.html>:
    //
    //   AT_SECURE
    //          Has a nonzero value if this executable should be treated
    //          securely.  Most commonly, a nonzero value indicates that the
    //          process is executing a set-user-ID or set-group-ID binary
    //          (so that its real and effective UIDs or GIDs differ from one
    //          another), or that it gained capabilities by executing a
    //          binary file that has capabilities (see capabilities(7)).
    if resolved_program.creds.uid != 0
        && (file_caps.effective
            || !resolved_program.creds.cap_ambient.contains(resolved_program.creds.cap_permitted))
    {
        resolved_program.secure_exec = true;
    }

    Ok(())
}
