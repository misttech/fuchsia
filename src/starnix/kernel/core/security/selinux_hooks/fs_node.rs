// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// TODO(https://github.com/rust-lang/rust/issues/39371): remove
#![allow(non_upper_case_globals)]

use super::super::{FsNodeSecurityXattr, check_task_capable};
use super::{
    Auditable, FileSystem, FsNodeLabel, FsNodeSidAndClass, NonPrivateDirEntry, NonPrivateFsNode,
    PermissionFlags, build_permission_check, check_permission, current_task_state,
    fs_node_effective_sid_and_class, fs_node_ensure_class, fs_node_set_label_with_task,
    has_fs_node_permissions, has_fs_node_permissions_dontaudit, permissions_from_flags,
    set_cached_sid,
};
use crate::task::CurrentTask;
use crate::vfs::{DirEntryHandle, FsStr, FsString, PathBuilder, UnlinkKind, ValueOrSize, XattrOp};
use fuchsia_rcu::RcuReadScope;
use selinux::policy::{AccessVector, FsUseType};
use selinux::{
    ClassPermission, CommonFilePermission, CommonFsNodePermission, DirPermission, FileClass,
    FileSystemLabel, FileSystemLabelingScheme, FileSystemPermission, ForClass, FsNodeClass,
    InitialSid, PolicyCap, SecurityId, SecurityServer, SocketClass, TaskAttrs,
};
use starnix_logging::{CATEGORY_STARNIX_SECURITY, log_debug, log_warn, track_stub};
use starnix_uapi::arc_key::WeakKey;
use starnix_uapi::auth::{CAP_FOWNER, Credentials};
use starnix_uapi::device_id::DeviceId;
use starnix_uapi::errors::{ENODATA, EOPNOTSUPP, Errno};
use starnix_uapi::file_mode::FileMode;
use starnix_uapi::inotify_mask::InotifyMask;
use starnix_uapi::{XATTR_NAME_SELINUX, errno, error};
use syncio::zxio_node_attr_has_t;

/// Maximum supported size for the extended attribute value used to store SELinux security
/// contexts in a filesystem node extended attributes.
const SECURITY_SELINUX_XATTR_VALUE_MAX_SIZE: usize = 4096;

/// Returns the relative path from the root of the file system containing this `DirEntry`.
fn get_fs_relative_path(dir_entry: &DirEntryHandle) -> FsString {
    let mut path_builder = PathBuilder::new();

    let scope = RcuReadScope::new();
    let mut current_dir = dir_entry.clone();
    while let Some(parent) = current_dir.parent() {
        path_builder.prepend_element(current_dir.local_name(&scope));
        current_dir = parent;
    }
    path_builder.build_absolute()
}

/// Verifies that the file system labelling is `FsUse`, and if so then it attempts to
/// apply the given context string to the node.
pub(in crate::security) fn fs_node_notify_security_context(
    security_server: &SecurityServer,
    fs_node: NonPrivateFsNode<'_>,
    security_context: &FsStr,
) {
    let fs = fs_node.fs();
    if !fs.security_state.state.supports_xattr() {
        return;
    }
    let sid = security_server
        .security_context_to_sid(security_context.into())
        .unwrap_or_else(|_| InitialSid::Unlabeled.into());
    set_cached_sid(fs_node, sid);
}

/// Called by the VFS to initialize the security state for an `FsNode` that
/// is being linked at `dir_entry`. If `read_xattr` is `false`, xattrs will
/// not be read - this makes sense for entries containing anonymous nodes, that
/// will not have an associated filesystem entry.
pub(in crate::security) fn fs_node_init_with_dentry(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    dir_entry: NonPrivateDirEntry<'_>,
    read_xattr: bool,
) -> Result<(), Errno> {
    // Attempt to derive a specific security class for the `FsNode`, based on its file mode.
    // TODO: This ensures a correct class for nodes with a wrong `FileMode` at
    // creation, but should not really be required.
    let fs_node = dir_entry.node();
    fs_node_ensure_class(fs_node)?;

    // This hook is called every time an `FsNode` is linked to a `DirEntry`, so it is expected that
    // the `FsNode` may already have been labeled.
    let label_class = fs_node.selinux_state().read();
    if !matches!(label_class.label, FsNodeLabel::Uninitialized) {
        return Ok(());
    }

    // If the parent has a from-task label then propagate it to the new node,  rather than applying
    // the filesystem's labeling scheme. This allows nodes in per-process and per-task directories
    // in "proc" to inherit the task's label.
    let parent = dir_entry.parent();
    if let Some(ref parent) = parent
        && let Some(parent_node) = NonPrivateFsNode::new(&parent.node)
    {
        let parent_label = (*parent_node.selinux_state().read()).clone();
        if let FsNodeLabel::FromTask { task_state } = &parent_label.label {
            fs_node_set_label_with_task(fs_node, task_state);
            return Ok(());
        }
    }

    // Obtain labeling information for the `FileSystem`. If none has been resolved yet then queue the
    // `dir_entry` to be labeled later.
    let fs = fs_node.fs();
    let label = if let Some(label) = fs.security_state.state.label() {
        label
    } else {
        log_debug!("Queuing FsNode for {:?} for labeling", dir_entry);
        fs.security_state
            .state
            .pending_entries
            .lock()
            .insert(WeakKey::from(dir_entry.as_dir_entry()));

        // Labelling may have completed while we were inserting the `DirEntry` so check again.
        let Some(label) = fs.security_state.state.label() else { return Ok(()) };
        label
    };

    let sid = match label.scheme {
        // mountpoint-labelling labels every node from the "context=" mount option.
        FileSystemLabelingScheme::Mountpoint { sid } => sid,
        // fs_use_xattr-labelling defers to the security attribute on the file node, with fall-back
        // behaviours for missing and invalid labels.
        FileSystemLabelingScheme::FsUse { fs_use_type, default_sid, .. } => {
            match (fs_use_type, read_xattr) {
                (FsUseType::Xattr, true) => {
                    // Determine the SID from the "security.selinux" attribute.
                    // TODO: Ensure that this `get_xattr` bypasses access-checks, so that label
                    // assignment cannot fail.
                    let attr = fs_node.ops().get_xattr(
                        &fs_node,
                        current_task,
                        XATTR_NAME_SELINUX.to_bytes().into(),
                        SECURITY_SELINUX_XATTR_VALUE_MAX_SIZE,
                    );

                    let maybe_sid = match attr {
                        Ok(ValueOrSize::Value(security_context)) => Some(
                            security_server
                                .security_context_to_sid((&security_context).into())
                                .unwrap_or_else(|_| InitialSid::Unlabeled.into()),
                        ),
                        Ok(ValueOrSize::Size(_)) => None,
                        Err(err) => match err.code {
                            ENODATA if parent.is_none() => {
                                // The root node of xattr-labeled filesystems should be labeled at
                                // creation in principle. Distinguishing creation of the root of the
                                // filesystem from re-instantiation of the `FsNode` representing an
                                // existing root is tricky, so this logic attempts to set a label
                                // if the root node lacks one, and the filesystem has "rootcontext="
                                // set in its mount parameters.
                                let maybe_root_sid = label.mount_sids.root_context;
                                if let Some(root_sid) = maybe_root_sid {
                                    let root_context =
                                        security_server.sid_to_security_context(root_sid).unwrap();
                                    fs_node.ops().set_xattr(
                                        &fs_node,
                                        current_task,
                                        XATTR_NAME_SELINUX.to_bytes().into(),
                                        root_context.as_slice().into(),
                                        XattrOp::Create,
                                    )?;
                                }

                                // Apply the appropriate in-memory label to the `FsNode`.
                                let node_sid = maybe_root_sid.unwrap_or(default_sid);
                                Some(node_sid)
                            }
                            ENODATA | EOPNOTSUPP => None,
                            _ => {
                                return Err(err);
                            }
                        },
                    };
                    maybe_sid.unwrap_or_else(|| {
                        // The node does not have a label, so apply the filesystem's default SID.
                        log_warn!(
                            "Unlabeled node {dir_entry:?} in {}:{:?} ({fs_use_type:?}-labeled) filesystem",
                            fs.name(),
                            fs.options.source,
                        );
                        default_sid
                    })
                }
                (FsUseType::Xattr, false) => {
                    log_warn!(
                        "Node {:?} in filesystem {} ({:?}-labeled) created in a context where the \
                        FileOpsCore lock cannot be taken.",
                        dir_entry,
                        fs.name(),
                        fs_use_type
                    );
                    InitialSid::Unlabeled.into()
                }
                _ => {
                    // Ephemeral nodes are then labeled by applying SID computation between their
                    // SID of the task that created them, and their parent file node's label (or
                    // the filesystem sid if they don't have a parent node).
                    // TODO: https://fxbug.dev/393086830 - Use the SID from the creating task,
                    // rather than current_task!
                    let scope = RcuReadScope::new();
                    let parent_node = parent
                        .as_ref()
                        .and_then(|parent| NonPrivateFsNode::assert_from(&parent.node));
                    return fs_node_init_on_create(
                        security_server,
                        current_task,
                        fs_node,
                        parent_node,
                        dir_entry.local_name(&scope),
                    )
                    .map(|_| ());
                }
            }
        }
        FileSystemLabelingScheme::GenFsCon { .. } => {
            let fs_type = fs_node.fs().name();
            let fs_node_class = fs_node.selinux_state().read().class();
            let sub_path = get_fs_relative_path(dir_entry.as_dir_entry());
            security_server
                .genfscon_label_for_fs_and_path(
                    fs_type.into(),
                    sub_path.as_slice().into(),
                    Some(fs_node_class.into()),
                )
                .map_err(|_| {
                    // This call fails if no policy has been loaded, or the `fs_type` is not set
                    // for `genfscon` treatment in the policy, neither of which should be the case
                    // here.  If `fs_type` was resolved to `genfscon` and then a policy loaded that
                    // removed/changed rules for that `fs_type` then it is possible to reach here,
                    // so error out rather than panicking the kernel.
                    errno!(EINVAL)
                })?
        }
    };

    set_cached_sid(fs_node, sid);

    Ok(())
}

// TODO: https://fxbug.dev/455771186 - Clean up with-DirEntry initialization and remove this.
pub(in crate::security) fn fs_node_init_with_dentry_deferred(dir_entry: NonPrivateDirEntry<'_>) {
    // This API is only for use when creating artificial file-system nodes prior to policy-load /
    // filesystem label initialization.
    let fs = dir_entry.node().fs();
    assert!(fs.security_state.state.label().is_none());
    log_debug!("Queuing FsNode for {:?} for labeling", dir_entry);
    fs.security_state.state.pending_entries.lock().insert(WeakKey::from(dir_entry.as_dir_entry()));
}

/// Returns an [`FsNodeSecurityXattr`] for the security context of `sid`.
fn make_fs_node_security_xattr(
    security_server: &SecurityServer,
    sid: SecurityId,
) -> Result<FsNodeSecurityXattr, Errno> {
    security_server
        .sid_to_security_context(sid)
        .map(|value| FsNodeSecurityXattr {
            name: XATTR_NAME_SELINUX.to_bytes().into(),
            value: value.into(),
        })
        .ok_or_else(|| errno!(EINVAL))
}

fn file_class_from_file_mode(mode: FileMode) -> Result<FileClass, Errno> {
    let file_type = mode.bits() & starnix_uapi::S_IFMT;
    match file_type {
        starnix_uapi::S_IFLNK => Ok(FileClass::LnkFile),
        starnix_uapi::S_IFDIR => Ok(FileClass::Dir),
        starnix_uapi::S_IFREG => Ok(FileClass::File),
        starnix_uapi::S_IFCHR => Ok(FileClass::ChrFile),
        starnix_uapi::S_IFBLK => Ok(FileClass::BlkFile),
        starnix_uapi::S_IFIFO => Ok(FileClass::FifoFile),
        starnix_uapi::S_IFSOCK => Ok(FileClass::SockFile),
        0 => {
            track_stub!(TODO("https://fxbug.dev/378864191"), "File with zero IFMT?");
            Ok(FileClass::File)
        }
        _ => error!(EINVAL, format!("mode: {:?}", mode)),
    }
}

/// Returns the SID to apply to an `FsNode` of socket-like `new_socket_class`.
/// Panics if called before any policy has been loaded.
fn compute_new_socket_sid(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    new_socket_class: SocketClass,
    name: &FsStr,
) -> Result<SecurityId, Errno> {
    let TaskAttrs { current_sid, sockcreate_sid, .. } = *current_task_state(current_task);
    if let Some(sid) = sockcreate_sid {
        return Ok(sid);
    }

    // `compute_create_sid` fails if the computed security context is invalid under the policy
    // (for example, an unauthorized role/type combination or a non-overlapping `glblub` range).
    let permission_check = build_permission_check(current_task, security_server);
    permission_check
        .compute_create_sid(current_sid, current_sid, new_socket_class.into(), name.as_ref())
        .map_err(|_| errno!(EPERM))
}

/// Returns the SID to apply to an `FsNode` of file-like `new_file_class`.
/// Panics if called before any policy has been loaded.
fn compute_new_file_sid(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    fs_label: &FileSystemLabel,
    parent: Option<NonPrivateFsNode<'_>>,
    new_file_class: FileClass,
    name: &FsStr,
) -> Result<SecurityId, Errno> {
    // If the `FileSystem` was mounted with "context=" then xattrs are not used for labeling, and
    // all `FsNode`s receive the same SID.
    match fs_label.scheme {
        FileSystemLabelingScheme::Mountpoint { sid } => return Ok(sid),
        _ => (),
    }

    // If `parent` is not set the calculation is for a new `FileSystem` root node, and the
    // "rootcontext=" mount option, if specified, overrides any policy-based calculation.
    if parent.is_none() {
        let root_sid = fs_label.mount_sids.root_context;
        if let Some(root_sid) = root_sid {
            return Ok(root_sid);
        }
    }

    let TaskAttrs { current_sid, fscreate_sid, .. } = *current_task_state(current_task);

    // If the task has an "fscreate" context set then use that deferring to the policy, except in
    // the gensfscon case where the fscreate context is ignored.
    if let Some(fscreate_sid) = fscreate_sid {
        if !matches!(fs_label.scheme, FileSystemLabelingScheme::GenFsCon { .. }) {
            return Ok(fscreate_sid);
        }
    }

    // If the `FileSystem` is configured with `fs_use_task` labeling then apply the task's label
    // directly, without applying policy-defined transitions.
    if matches!(
        fs_label.scheme,
        FileSystemLabelingScheme::FsUse { fs_use_type: FsUseType::Task, .. }
    ) {
        // TODO: https://fxbug.dev/393086830 The root node of a "tmpfs" instance mounted
        // with `fs_use_task` appears to be labeled with the "kernel" SID, instead of the
        // SID of the mounting task.
        return Ok(current_sid);
    }

    // All other cases take into account role & type transitions following the general "create"
    // Security Context derivation rules. These cases include:
    // - `fs_use_xattr` and `fs_use_trans` labeling schemes.
    // - Filesystems with no labeling scheme explicitly specified by policy.
    //
    // Policy rules are also applied here for `genfscon` labeling, for the purposes of "create"
    // permission checks, despite the fact that the `genfscon`-defined label will actually be
    // applied to the node (see `fs_node_init_with_dentry()`).
    let target_sid = if let Some(parent) = parent {
        // If the node has a parent then that is the target for the computation.
        fs_node_effective_sid_and_class(parent).sid
    } else {
        // If the node is the root of the filesystem then the target is the filesystem's
        // SID.
        fs_label.sid
    };

    // `compute_create_sid` fails if the computed security context is invalid under the policy
    // (for example, an unauthorized role/type combination or a non-overlapping `glblub` range).
    let permission_check = build_permission_check(current_task, security_server);
    permission_check
        .compute_create_sid(current_sid, target_sid, new_file_class.into(), name.as_ref())
        .map_err(|_| errno!(EPERM))
}

/// Returns the SID with which an `FsNode` of `new_node_class` would be labeled, if created by
/// `current_task` under the specified `parent` node.
/// Policy-defined labeling rules, including transitions, are taken into account.
///
/// Note that this cannot be called prior to a policy being loaded, and the file system label
/// resolved, since those are prerequisites for the `FileSystemLabel` being available.
pub(in crate::security) fn compute_new_fs_node_sid(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    fs_label: &FileSystemLabel,
    parent: Option<NonPrivateFsNode<'_>>,
    new_node_class: FsNodeClass,
    name: &FsStr,
) -> Result<SecurityId, Errno> {
    Ok(match new_node_class {
        FsNodeClass::Socket(new_socket_class) => {
            compute_new_socket_sid(security_server, current_task, new_socket_class, name)?
        }
        FsNodeClass::File(new_file_class) => compute_new_file_sid(
            security_server,
            current_task,
            fs_label,
            parent,
            new_file_class,
            name,
        )?,
    })
}

/// Called by file-system implementations when creating the `FsNode` for a new file.
pub(in crate::security) fn fs_node_init_on_create(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    new_node: NonPrivateFsNode<'_>,
    parent: Option<NonPrivateFsNode<'_>>,
    name: &FsStr,
) -> Result<Option<FsNodeSecurityXattr>, Errno> {
    // By definition this is a new `FsNode` so should not have already been labeled.
    let label_class = new_node.selinux_state().read();
    let is_uninitialized = matches!(label_class.label, FsNodeLabel::Uninitialized);
    assert!(is_uninitialized, "init_on_create() for {:?} with label {:?}", new_node, *label_class);

    // If the `new_node` does not already have a specific security class selected then choose one
    // based on its file mode.
    let new_node_class = fs_node_ensure_class(new_node)?;

    // If the file system is not yet labeled (i.e. no policy has been loaded) then no label can
    // be applied yet.
    let fs = new_node.fs();
    let Some(fs_label) = fs.security_state.state.label() else {
        return Ok(None);
    };

    // Determine the SID with which to label the `new_node` with, dependent on the file
    // class, etc. This will only fail if the filesystem containing the nodes does not yet
    // have labeling information resolved.
    let sid = compute_new_fs_node_sid(
        security_server,
        current_task,
        fs_label,
        parent,
        new_node_class,
        name.into(),
    )?;

    let (sid, xattr) = match fs_label.scheme {
        FileSystemLabelingScheme::FsUse { fs_use_type, .. } => {
            let xattr = (fs_use_type == FsUseType::Xattr)
                .then(|| make_fs_node_security_xattr(security_server, sid))
                .transpose()?;
            (sid, xattr)
        }
        FileSystemLabelingScheme::Mountpoint { .. } => (sid, None),
        FileSystemLabelingScheme::GenFsCon { .. } => {
            // Defer labeling to `fs_node_init_with_dentry()`, so that the path of the new
            // node can be taken into account.
            return Ok(None);
        }
    };

    set_cached_sid(new_node, sid);

    Ok(xattr)
}

pub(in crate::security) fn dentry_create_files_as(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    parent: NonPrivateFsNode<'_>,
    new_node_mode: FileMode,
    new_node_name: &FsStr,
    new_creds: &mut Credentials,
) -> Result<(), Errno> {
    // Determine the security class of the new node, and the SID with which it would be labeled.
    let fs = parent.fs();
    let new_node_class = file_class_from_file_mode(new_node_mode)?.into();
    let new_node_sid = if let Some(fs_label) = fs.security_state.state.label() {
        compute_new_fs_node_sid(
            security_server,
            current_task,
            fs_label,
            Some(parent),
            new_node_class,
            new_node_name.into(),
        )?
    } else {
        InitialSid::File.into()
    };

    new_creds.security_state.fscreate_sid = Some(new_node_sid);

    Ok(())
}

/// Labels [`FsNode`]s not linked into any filesystem directory structure (e.g., eventfds).
pub(in crate::security) fn fs_node_init_anon(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    new_node: NonPrivateFsNode<'_>,
    node_type: &str,
) -> Result<(), Errno> {
    let (node_class, node_type): (FsNodeClass, _) = if node_type == "[memfd]" {
        // If the "memfd_class" policy capability is enabled then mem-FD nodes use the anon_inode
        // labeling scheme, but receive their own dedicated security class.
        if !security_server.is_policycap_enabled(PolicyCap::MemfdClass) {
            return Ok(());
        }
        (FileClass::MemFdFile.into(), "")
    } else {
        (FileClass::AnonFsNode.into(), node_type)
    };

    let sid = if current_task.kernel().security_state.state.as_ref().unwrap().has_policy() {
        let task_sid = current_task_state(current_task).current_sid;
        let new_sid = build_permission_check(current_task, security_server)
            .compute_create_sid(task_sid, task_sid, node_class.into(), node_type.as_ref())
            .expect("Compute label for anon_inode");
        check_permission(
            &build_permission_check(current_task, security_server),
            current_task,
            task_sid,
            new_sid,
            CommonFsNodePermission::Create.for_class(node_class),
            Auditable::Name(node_type.into()),
        )?;
        new_sid
    } else {
        // If no policy has been loaded then `anon_inode`s receive the "unlabeled" context.
        InitialSid::Unlabeled.into()
    };

    new_node.selinux_state().update(FsNodeLabel::SecurityId { sid }, node_class);

    Ok(())
}

/// Helper used by filesystem node creation checks to validate that `current_task` has necessary
/// permissions to create a new node under the specified `parent`.
fn may_create(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    parent: NonPrivateFsNode<'_>,
    new_file_mode: FileMode, // Only used to determine the file class.
    name: &FsStr,
) -> Result<(), Errno> {
    let permission_check = build_permission_check(current_task, security_server);

    // Verify that the caller has permissions required to add new entries to the target
    // directory node.
    let current_sid = current_task_state(current_task).current_sid;
    let parent_sid = fs_node_effective_sid_and_class(parent).sid;
    let fs = parent.fs();

    let audit_context =
        &[current_task.into(), parent.into(), fs.as_ref().into(), Auditable::Name(name)];
    check_permission(
        &permission_check,
        current_task,
        current_sid,
        parent_sid,
        DirPermission::Search,
        audit_context.into(),
    )?;
    check_permission(
        &permission_check,
        current_task,
        current_sid,
        parent_sid,
        DirPermission::AddName,
        audit_context.into(),
    )?;

    // Verify that the caller has permission to create new nodes of the desired type.
    let new_file_class = file_class_from_file_mode(new_file_mode)?.into();
    let new_file_sid = if let Some(fs_label) = fs.security_state.state.label() {
        compute_new_fs_node_sid(
            security_server,
            current_task,
            fs_label,
            Some(parent),
            new_file_class,
            name.into(),
        )?
    } else {
        InitialSid::File.into()
    };

    let audit_context = &[current_task.into(), fs.as_ref().into(), Auditable::Name(name)];
    check_permission(
        &permission_check,
        current_task,
        current_sid,
        new_file_sid,
        CommonFsNodePermission::Create.for_class(new_file_class),
        audit_context.into(),
    )?;

    // Verify that the new node's label is permitted to be created in the target filesystem.
    let Some(fs_label) = fs.security_state.state.label() else {
        track_stub!(
            TODO("https://fxbug.dev/367585803"),
            "may_create() should not be called until policy load has completed"
        );
        return error!(EPERM);
    };

    check_permission(
        &permission_check,
        current_task,
        new_file_sid,
        fs_label.sid,
        FileSystemPermission::Associate,
        audit_context.into(),
    )?;

    Ok(())
}

/// Helper that checks whether the `current_task` can create a new link to the `existing` file or
/// directory in the `parent` directory. Called by [`check_fs_node_link_access`].
fn may_link(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    parent: NonPrivateFsNode<'_>,
    existing_node: NonPrivateFsNode<'_>,
) -> Result<(), Errno> {
    let audit_context = current_task.into();

    let permission_check = build_permission_check(current_task, security_server);
    let current_sid = current_task_state(current_task).current_sid;
    let parent_sid = fs_node_effective_sid_and_class(parent).sid;
    let FsNodeSidAndClass { sid: file_sid, class: file_class } =
        fs_node_effective_sid_and_class(existing_node);

    let FsNodeClass::File(file_class) = file_class else {
        panic!("may_link called on non-file-like class")
    };
    check_permission(
        &permission_check,
        current_task,
        current_sid,
        parent_sid,
        DirPermission::Search,
        audit_context,
    )?;
    check_permission(
        &permission_check,
        current_task,
        current_sid,
        parent_sid,
        DirPermission::AddName,
        audit_context,
    )?;
    check_permission(
        &permission_check,
        current_task,
        current_sid,
        file_sid,
        CommonFilePermission::Link.for_class(file_class),
        audit_context,
    )?;

    Ok(())
}

/// Helper that checks whether the `current_task` can unlink or rmdir an `fs_node` from its
/// `parent` directory.
/// If [`operation`] is [`UnlinkKind::Directory`] this will check permissions for rmdir;
/// otherwise for unlink.
/// Called by [`check_fs_node_unlink_access`] and [`check_fs_node_rmdir_access`] .
fn may_unlink_or_rmdir(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    parent: NonPrivateFsNode<'_>,
    fs_node: NonPrivateFsNode<'_>,
    name: &FsStr,
    operation: UnlinkKind,
) -> Result<(), Errno> {
    let audit_context = &[current_task.into(), Auditable::Name(name)];

    let permission_check = build_permission_check(current_task, security_server);
    let current_sid = current_task_state(current_task).current_sid;
    let parent_sid = fs_node_effective_sid_and_class(parent).sid;

    check_permission(
        &permission_check,
        current_task,
        current_sid,
        parent_sid,
        DirPermission::Search,
        audit_context.into(),
    )?;
    check_permission(
        &permission_check,
        current_task,
        current_sid,
        parent_sid,
        DirPermission::RemoveName,
        audit_context.into(),
    )?;

    let FsNodeSidAndClass { sid: file_sid, class: file_class } =
        fs_node_effective_sid_and_class(fs_node);
    let FsNodeClass::File(file_class) = file_class else {
        panic!("may_unlink_or_rmdir called on non-file-like class")
    };

    match operation {
        UnlinkKind::NonDirectory => check_permission(
            &permission_check,
            current_task,
            current_sid,
            file_sid,
            CommonFilePermission::Unlink.for_class(file_class),
            audit_context.into(),
        ),
        UnlinkKind::Directory => check_permission(
            &permission_check,
            current_task,
            current_sid,
            file_sid,
            DirPermission::RemoveDir,
            audit_context.into(),
        ),
    }
}

/// Validate that `current_task` has permission to create a regular file in the `parent` directory,
/// with the specified file `mode`.
pub(in crate::security) fn check_fs_node_create_access(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    parent: NonPrivateFsNode<'_>,
    mode: FileMode,
    name: &FsStr,
) -> Result<(), Errno> {
    may_create(security_server, current_task, parent, mode, name)
}

/// Validate that `current_task` has permission to create a symlink to `old_path` in the `parent`
/// directory.
pub(in crate::security) fn check_fs_node_symlink_access(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    parent: NonPrivateFsNode<'_>,
    name: &FsStr,
    _old_path: &FsStr,
) -> Result<(), Errno> {
    may_create(security_server, current_task, parent, FileMode::IFLNK, name)
}

/// Validate that `current_task` has permission to create a new directory in the `parent` directory,
/// with the specified file `mode`.
pub(in crate::security) fn check_fs_node_mkdir_access(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    parent: NonPrivateFsNode<'_>,
    mode: FileMode,
    name: &FsStr,
) -> Result<(), Errno> {
    may_create(security_server, current_task, parent, mode, name)
}

/// Validate that `current_task` has permission to create a new special file, socket or pipe, in the
/// `parent` directory, and with the specified file `mode` and `device_id`.
pub(in crate::security) fn check_fs_node_mknod_access(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    parent: NonPrivateFsNode<'_>,
    mode: FileMode,
    name: &FsStr,
    _device_id: DeviceId,
) -> Result<(), Errno> {
    may_create(security_server, current_task, parent, mode, name)
}

/// Validate that `current_task` has the permission to create a new hard link to a file.
pub(in crate::security) fn check_fs_node_link_access(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    target_directory: NonPrivateFsNode<'_>,
    existing_node: NonPrivateFsNode<'_>,
) -> Result<(), Errno> {
    may_link(security_server, current_task, target_directory, existing_node)
}

/// Validate that `current_task` has the permission to remove a hard link to a file.
pub(in crate::security) fn check_fs_node_unlink_access(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    parent: NonPrivateFsNode<'_>,
    child: NonPrivateFsNode<'_>,
    name: &FsStr,
) -> Result<(), Errno> {
    assert!(!child.is_dir());

    may_unlink_or_rmdir(
        security_server,
        current_task,
        parent,
        child,
        name,
        UnlinkKind::NonDirectory,
    )
}

/// Validate that `current_task` has the permission to remove a directory.
pub(in crate::security) fn check_fs_node_rmdir_access(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    parent: NonPrivateFsNode<'_>,
    child: NonPrivateFsNode<'_>,
    name: &FsStr,
) -> Result<(), Errno> {
    assert!(child.is_dir());

    may_unlink_or_rmdir(security_server, current_task, parent, child, name, UnlinkKind::Directory)
}

/// Validates that `current_task` has the permissions to move `moving_node`.
pub(in crate::security) fn check_fs_node_rename_access(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    old_parent: NonPrivateFsNode<'_>,
    moving_node: NonPrivateFsNode<'_>,
    new_parent: NonPrivateFsNode<'_>,
    replaced_node: Option<NonPrivateFsNode<'_>>,
    old_basename: &FsStr,
    new_basename: &FsStr,
) -> Result<(), Errno> {
    let permission_check = build_permission_check(current_task, security_server);
    let current_sid = current_task_state(current_task).current_sid;
    let old_parent_sid = fs_node_effective_sid_and_class(old_parent).sid;

    check_permission(
        &permission_check,
        current_task,
        current_sid,
        old_parent_sid,
        DirPermission::Search,
        current_task.into(),
    )?;

    let audit_context_old_name = &[current_task.into(), Auditable::Name(old_basename)];
    check_permission(
        &permission_check,
        current_task,
        current_sid,
        old_parent_sid,
        DirPermission::RemoveName,
        audit_context_old_name.into(),
    )?;

    let FsNodeSidAndClass { sid: file_sid, class: file_class } =
        fs_node_effective_sid_and_class(moving_node);
    let FsNodeClass::File(file_class) = file_class else {
        panic!("fs_node_rename called on non-file-like class")
    };

    check_permission(
        &permission_check,
        current_task,
        current_sid,
        file_sid,
        CommonFilePermission::Rename.for_class(file_class),
        audit_context_old_name.into(),
    )?;

    let audit_context_new_name = &[current_task.into(), Auditable::Name(new_basename)];
    let new_parent_sid = fs_node_effective_sid_and_class(new_parent).sid;
    check_permission(
        &permission_check,
        current_task,
        current_sid,
        new_parent_sid,
        DirPermission::AddName,
        audit_context_new_name.into(),
    )?;

    // If a file already exists with the new name, then verify that the existing file can be
    // removed.
    if let Some(replaced_node) = replaced_node {
        let replaced_node_class = fs_node_effective_sid_and_class(replaced_node).class;
        may_unlink_or_rmdir(
            security_server,
            current_task,
            new_parent,
            replaced_node,
            new_basename,
            if replaced_node_class == FileClass::Dir.into() {
                UnlinkKind::Directory
            } else {
                UnlinkKind::NonDirectory
            },
        )?;
    }

    if !std::ptr::eq(old_parent.as_fs_node(), new_parent.as_fs_node()) {
        // If the parent nodes are the same directory, we have already verified the search
        // permission during the `old_parent_sid` verification.
        check_permission(
            &permission_check,
            current_task,
            current_sid,
            new_parent_sid,
            DirPermission::Search,
            current_task.into(),
        )?;

        // If the file is a directory and its parent directory is being changed by the rename,
        // we additionally check for the reparent permission. Note that the `reparent` permission is
        // only defined for directories.
        if file_class == FileClass::Dir.into() {
            check_permission(
                &permission_check,
                current_task,
                current_sid,
                file_sid,
                DirPermission::Reparent,
                current_task.into(),
            )?;
        }
    }

    Ok(())
}

/// Validates that `current_task` has the permissions to read the symbolic link `fs_node`.
pub(in crate::security) fn check_fs_node_read_link_access(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    fs_node: NonPrivateFsNode<'_>,
) -> Result<(), Errno> {
    let current_sid = current_task_state(current_task).current_sid;
    has_fs_node_permissions(
        &build_permission_check(current_task, security_server),
        current_task,
        current_sid,
        fs_node,
        &[CommonFsNodePermission::Read],
        current_task.into(),
    )
}

/// Returns true if there exits a `dontaudit` rule for `current_task` access to `fs_node`, which
/// includes the `audit_access` pseudo-permission.
pub(in crate::security) fn has_dontaudit_access(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    fs_node: NonPrivateFsNode<'_>,
) -> bool {
    fuchsia_trace::duration!(CATEGORY_STARNIX_SECURITY, "security.selinux.has_dontaudit_access");

    let FsNodeSidAndClass { sid, class } = fs_node_effective_sid_and_class(fs_node);
    let permission_check = build_permission_check(current_task, security_server);
    let access_vector = CommonFsNodePermission::AuditAccess.for_class(class).as_access_vector();
    let current_sid = current_task_state(current_task).current_sid;
    let decision = permission_check.compute_access_decision(current_sid, sid, class.into());
    access_vector & decision.audit == AccessVector::NONE
}

/// Validates that the `current_task` has the permissions to access `fs_node`.
pub(in crate::security) fn fs_node_permission(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    fs_node: NonPrivateFsNode<'_>,
    permission_flags: PermissionFlags,
    audit_context: Auditable<'_>,
) -> Result<(), Errno> {
    let current_sid = current_task_state(current_task).current_sid;
    let fs_node_class = fs_node_effective_sid_and_class(fs_node).class;
    let audit_context = [current_task.into(), audit_context];

    if permission_flags.contains(PermissionFlags::ACCESS) {
        let dont_audit = has_dontaudit_access(security_server, current_task, fs_node);
        if dont_audit {
            return has_fs_node_permissions_dontaudit(
                &build_permission_check(current_task, security_server),
                current_task,
                current_sid,
                fs_node,
                &permissions_from_flags(permission_flags, fs_node_class),
            );
        }
    }

    has_fs_node_permissions(
        &build_permission_check(current_task, security_server),
        current_task,
        current_sid,
        fs_node,
        &permissions_from_flags(permission_flags, fs_node_class),
        (&audit_context).into(),
    )
}

pub(in crate::security) fn check_fs_node_getattr_access(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    fs_node: NonPrivateFsNode<'_>,
) -> Result<(), Errno> {
    let current_sid = current_task_state(current_task).current_sid;
    has_fs_node_permissions(
        &build_permission_check(current_task, security_server),
        current_task,
        current_sid,
        fs_node,
        &[CommonFsNodePermission::GetAttr],
        current_task.into(),
    )
}

/// Checks whether `current_task` can set a watch on `fs_node`, to be notified of the events in
/// `mask`.
///
/// Starnix only supports inotify, which only allows watches to be set on individual nodes. The
/// "watch_mount", "watch_sb" and "watch_with_perm" permissions are only relevant to fanotify
/// mount/superblock marks and permission events, so they are not checked here.
pub(in crate::security) fn path_notify(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    fs_node: NonPrivateFsNode<'_>,
    mask: InotifyMask,
) -> Result<(), Errno> {
    let FsNodeSidAndClass { sid: target_sid, class } = fs_node_effective_sid_and_class(fs_node);
    let FsNodeClass::File(file_class) = class else {
        // The "watch" permissions are only defined for file-like classes, but watches can still be
        // requested on socket-like nodes, e.g. by watching "/proc/<pid>/fd/<n>". SELinux on Linux
        // denies such watches unless permissions that are unrelated to watching (or undefined)
        // happen to be granted on the socket class, so deny them.
        return error!(EACCES);
    };

    let permission_check = build_permission_check(current_task, security_server);
    let current_sid = current_task_state(current_task).current_sid;
    let fs = fs_node.fs();
    let audit_context = [current_task.into(), fs_node.into(), fs.as_ref().into()];
    // The "watch" permission is required to set any watch, regardless of the events in `mask`.
    check_permission(
        &permission_check,
        current_task,
        current_sid,
        target_sid,
        CommonFilePermission::Watch.for_class(file_class),
        (&audit_context).into(),
    )?;
    // Watches that will report read-exclusive events additionally require the "watch_reads"
    // permission. `OPEN` is not read-exclusive, since files may also be opened for writing.
    if mask.intersects(InotifyMask::ACCESS | InotifyMask::CLOSE_NOWRITE) {
        check_permission(
            &permission_check,
            current_task,
            current_sid,
            target_sid,
            CommonFilePermission::WatchReads.for_class(file_class),
            (&audit_context).into(),
        )?;
    }
    Ok(())
}

/// Checks whether `current_task` can set attributes on `node`.
pub(in crate::security) fn check_fs_node_setattr_access(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    fs_node: NonPrivateFsNode<'_>,
    attributes: &zxio_node_attr_has_t,
) -> Result<(), Errno> {
    let current_sid = current_task_state(current_task).current_sid;

    let permissions = if attributes.mode
        || attributes.uid
        || attributes.gid
        || attributes.access_time
        || attributes.modification_time
        || attributes.change_time
        || attributes.casefold
    {
        [CommonFsNodePermission::SetAttr]
    } else {
        [CommonFsNodePermission::Write]
    };

    has_fs_node_permissions(
        &build_permission_check(current_task, security_server),
        current_task,
        current_sid,
        fs_node,
        &permissions,
        current_task.into(),
    )
}

pub(in crate::security) fn fs_node_xattr_skipcap(name: &FsStr) -> bool {
    name == XATTR_NAME_SELINUX.to_bytes()
}

pub(in crate::security) fn check_fs_node_setxattr_access(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    fs_node: NonPrivateFsNode<'_>,
    name: &FsStr,
    value: &FsStr,
    _op: XattrOp,
) -> Result<(), Errno> {
    let current_sid = current_task_state(current_task).current_sid;

    // If any xattr other than the SELinux label is being set then require the "setattr" permission.
    // If the xattr is in the security.* namespace then the calling logic will already have checked
    // for CAP_SYS_ADMIN; the capability check is also skipped for the SELinux xattr, via the
    // `fs_node_xattr_skipcap()` check.
    if name != XATTR_NAME_SELINUX.to_bytes() {
        return has_fs_node_permissions(
            &build_permission_check(current_task, security_server),
            current_task,
            current_sid,
            fs_node,
            &[CommonFsNodePermission::SetAttr],
            current_task.into(),
        );
    }

    // The "security.selinux" attribute is being modified. If re-labeling is not supported by the
    // filesystem/labeling scheme then report as such.
    let fs = fs_node.fs();
    let Some(fs_label) = fs.security_state.state.label() else {
        return Ok(());
    };
    if !fs.security_state.state.supports_relabel() {
        return error!(ENOTSUP);
    }

    // Check whether the caller "owns" the file, or has the CAP_FOWNER capability.
    let file_uid = fs_node.info().uid;
    if current_task.current_creds().uid != file_uid {
        check_task_capable(current_task, CAP_FOWNER)?;
    }

    // TODO: https://fxbug.dev/367585803 - Lock the `fs_node` security label here, and return a
    // guard from this hook, for the caller to hold until after setxattr/setsecurity, to ensure
    // consistency.

    // Verify that the requested modification is permitted by the loaded policy.
    if security_server.is_enforcing() {
        let audit_context = &[current_task.into(), fs_node.into(), fs.as_ref().into()];
        let audit_context = audit_context.into();

        let new_sid =
            security_server.security_context_to_sid(value.into()).map_err(|_| errno!(EINVAL))?;
        let task_sid = current_task_state(current_task).current_sid;
        let FsNodeSidAndClass { sid: old_sid, class: fs_node_class } =
            fs_node_effective_sid_and_class(fs_node);

        let permission_check = build_permission_check(current_task, security_server);
        check_permission(
            &permission_check,
            current_task,
            task_sid,
            old_sid,
            CommonFsNodePermission::RelabelFrom.for_class(fs_node_class),
            audit_context,
        )?;
        check_permission(
            &permission_check,
            current_task,
            task_sid,
            new_sid,
            CommonFsNodePermission::RelabelTo.for_class(fs_node_class),
            audit_context,
        )?;
        check_permission(
            &permission_check,
            current_task,
            new_sid,
            fs_label.sid,
            FileSystemPermission::Associate,
            audit_context,
        )?;
    }

    Ok(())
}

pub(in crate::security) fn check_fs_node_getxattr_access(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    fs_node: NonPrivateFsNode<'_>,
    _name: &FsStr,
) -> Result<(), Errno> {
    let current_sid = current_task_state(current_task).current_sid;
    has_fs_node_permissions(
        &build_permission_check(current_task, security_server),
        current_task,
        current_sid,
        fs_node,
        &[CommonFsNodePermission::GetAttr],
        current_task.into(),
    )
}

pub(in crate::security) fn check_fs_node_listxattr_access(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    fs_node: NonPrivateFsNode<'_>,
) -> Result<(), Errno> {
    let current_sid = current_task_state(current_task).current_sid;
    has_fs_node_permissions(
        &build_permission_check(current_task, security_server),
        current_task,
        current_sid,
        fs_node,
        &[CommonFsNodePermission::GetAttr],
        current_task.into(),
    )
}

pub(in crate::security) fn check_fs_node_removexattr_access(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    fs_node: NonPrivateFsNode<'_>,
    name: &FsStr,
) -> Result<(), Errno> {
    // Removing the SELinux security label is not permitted.
    if name == XATTR_NAME_SELINUX.to_bytes() {
        return error!(EACCES);
    }

    let current_sid = current_task_state(current_task).current_sid;
    has_fs_node_permissions(
        &build_permission_check(current_task, security_server),
        current_task,
        current_sid,
        fs_node,
        &[CommonFsNodePermission::SetAttr],
        current_task.into(),
    )
}

/// If `fs_node` is in a filesystem without xattr support, returns the xattr name for the security
/// label (i.e. "security.selinux"). Otherwise returns None.
pub(in crate::security) fn fs_node_listsecurity(fs_node: NonPrivateFsNode<'_>) -> Option<FsString> {
    if fs_node.fs().security_state.state.supports_xattr() {
        None
    } else {
        Some(XATTR_NAME_SELINUX.to_bytes().into())
    }
}

/// Returns the Security Context corresponding to the SID with which `FsNode`
/// is labelled, or `Ok(None)` if `name` is not `"security.selinux"`.
pub(in crate::security) fn fs_node_getsecurity(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    fs_node: NonPrivateFsNode<'_>,
    name: &FsStr,
    max_size: usize,
) -> Result<Option<ValueOrSize<FsString>>, Errno> {
    if name != XATTR_NAME_SELINUX.to_bytes() {
        return Ok(None);
    }

    // If the SID cached on the node is "unlabeled" then the node may have an xattr with an invalid
    // Security Context, which we should return, so return the `get_xattr()` result, unless it indicates
    // that the filesystem does not support the attribute.
    let sid = fs_node_effective_sid_and_class(fs_node).sid;
    if sid == InitialSid::Unlabeled.into() && fs_node.fs().security_state.state.supports_xattr() {
        let result = fs_node.ops().get_xattr(&fs_node, current_task, name, max_size);
        if result != error!(ENOTSUP) {
            return result.map(Some);
        }
    }

    // Serialize the SID to a Security Context and return it.
    if let Some(context) = security_server.sid_to_security_context_with_nul(sid) {
        return Ok(Some(ValueOrSize::Value(context.into())));
    }

    error!(ENOTSUP)
}

/// Sets the `name`d security attribute on `fs_node` and updates internal
/// kernel state, or returns `Ok(None)` if `name` is not `"security.selinux"`.
// TODO: https://fxbug.dev/367585803 - This API should be called with the `fs_node`'s security
// state already locked by `check_fs_node_setxattr_access()`, for consistency.
pub(in crate::security) fn fs_node_setsecurity(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    fs_node: NonPrivateFsNode<'_>,
    name: &FsStr,
    value: &FsStr,
    op: XattrOp,
) -> Result<Option<()>, Errno> {
    if name != XATTR_NAME_SELINUX.to_bytes() {
        return Ok(None);
    }

    // If the filesystem is configured to persist labels into xattrs then apply the label to the
    // node.
    if fs_node.fs().security_state.state.supports_xattr() {
        fs_node.ops().set_xattr(&fs_node, current_task, name, value, op)?;
    }

    // Finally, update the label cached on the file node.
    let new_sid = security_server.security_context_to_sid(value.into()).ok();
    let effective_new_sid = new_sid.unwrap_or_else(|| InitialSid::Unlabeled.into());
    set_cached_sid(fs_node, effective_new_sid);

    Ok(Some(()))
}

/// Updates `new_creds` with the credentials required to copy-up `fs_node` into a new node.
pub(in crate::security) fn fs_node_copy_up(
    _current_task: &CurrentTask,
    fs_node: NonPrivateFsNode<'_>,
    fs: &FileSystem,
    new_creds: &mut Credentials,
) {
    // If the overlay filesystem is mounted with `context=` (`Mountpoint` labeling) then use that
    // SID; otherwise copy the effective SID of the lower `fs_node`.
    let new_sid = if let Some(FileSystemLabel {
        scheme: FileSystemLabelingScheme::Mountpoint { sid },
        ..
    }) = fs.security_state.state.label()
    {
        *sid
    } else {
        fs_node_effective_sid_and_class(fs_node).sid
    };
    new_creds.security_state.fscreate_sid = Some(new_sid);
}

#[cfg(test)]
mod tests {
    use super::super::get_cached_sid;
    use super::super::testing::{
        self, TEST_FILE_NAME, spawn_kernel_with_selinux_hooks_test_policy_and_run,
    };
    use super::*;
    use crate::fs::fuchsia::{create_file_from_handle, create_fuchsia_pipe, new_remote_file};
    use crate::mm::PAGE_SIZE;
    use crate::testing::spawn_kernel_and_run;
    use crate::vfs::{FsNode, XattrOp};
    use bstr::BStr;
    use fidl_fuchsia_io as fio;
    use selinux::SocketClass;
    use starnix_uapi::errno;
    use starnix_uapi::open_flags::OpenFlags;

    const VALID_SECURITY_CONTEXT: &[u8] = b"u:object_r:test_valid_t:s0";
    const VALID_SECURITY_CONTEXT_WITH_NUL: &[u8] = b"u:object_r:test_valid_t:s0\0";

    /// Clears the cached security id on `fs_node`.
    fn clear_cached_sid(fs_node: &FsNode) {
        fs_node.security_state.selinux_for_test().clear_label_for_test();
    }

    #[fuchsia::test]
    async fn fs_node_resolved_and_effective_sids_for_missing_xattr() {
        spawn_kernel_with_selinux_hooks_test_policy_and_run(|current_task, security_server| {
            let dir_entry = &testing::create_test_file(current_task).entry;
            let non_private_dir_entry = NonPrivateDirEntry::new(dir_entry).unwrap();
            let node = non_private_dir_entry.node();

            // Remove the "security.selinux" label, if any.
            let _ =
                node.ops().remove_xattr(&node, &current_task, XATTR_NAME_SELINUX.to_bytes().into());
            assert_eq!(
                node.ops()
                    .get_xattr(&node, &current_task, XATTR_NAME_SELINUX.to_bytes().into(), 4096)
                    .unwrap_err(),
                errno!(ENODATA)
            );

            // Clear the cached SID and use `fs_node_init_with_dentry()` to re-resolve the label.
            clear_cached_sid(&node);
            assert_eq!(None, get_cached_sid(&node));
            fs_node_init_with_dentry(
                &security_server,
                &current_task,
                non_private_dir_entry,
                /* read_xaddr = */
                true,
            )
            .expect("fs_node_init_with_dentry");

            // `fs_node_getsecurity()` should now fall-back to the policy's "file" Context.
            let default_file_context = security_server
                .sid_to_security_context_with_nul(InitialSid::File.into())
                .unwrap()
                .into();
            let result = fs_node_getsecurity(
                &security_server,
                &current_task,
                node,
                XATTR_NAME_SELINUX.to_bytes().into(),
                SECURITY_SELINUX_XATTR_VALUE_MAX_SIZE,
            )
            .unwrap();
            assert_eq!(result, Some(ValueOrSize::Value(default_file_context)));
            assert!(get_cached_sid(&node).is_some());
        })
        .await;
    }

    #[fuchsia::test]
    async fn fs_node_resolved_and_effective_sids_for_invalid_xattr() {
        spawn_kernel_with_selinux_hooks_test_policy_and_run(|current_task, security_server| {
            let dir_entry = &testing::create_test_file(current_task).entry;
            let non_private_dir_entry = NonPrivateDirEntry::new(dir_entry).unwrap();
            let node = non_private_dir_entry.node();

            const INVALID_CONTEXT: &[u8] = b"invalid context!";

            // Set the security label to a value which is not a valid Security Context.
            node.ops()
                .set_xattr(
                    &node,
                    &current_task,
                    XATTR_NAME_SELINUX.to_bytes().into(),
                    INVALID_CONTEXT.into(),
                    XattrOp::Set,
                )
                .expect("setxattr");

            // Clear the cached SID and use `fs_node_init_with_dentry()` to re-resolve the label.
            clear_cached_sid(&node);
            assert_eq!(None, get_cached_sid(&node));
            fs_node_init_with_dentry(
                &security_server,
                &current_task,
                non_private_dir_entry,
                /* read_xaddr = */
                true,
            )
            .expect("fs_node_init_with_dentry");

            // `fs_node_getsecurity()` should report the same invalid string as is in the xattr.
            let result = fs_node_getsecurity(
                &security_server,
                &current_task,
                node,
                XATTR_NAME_SELINUX.to_bytes().into(),
                SECURITY_SELINUX_XATTR_VALUE_MAX_SIZE,
            )
            .unwrap();
            assert_eq!(result, Some(ValueOrSize::Value(INVALID_CONTEXT.into())));

            // The SID cached for the `node` should be "unlabeled".
            let unlabeled_initial_sid = InitialSid::Unlabeled.into();
            assert_eq!(Some(unlabeled_initial_sid), get_cached_sid(&node));

            // The effective SID of the node should be "unlabeled".
            assert_eq!(unlabeled_initial_sid, fs_node_effective_sid_and_class(node).sid);
        })
        .await;
    }

    #[fuchsia::test]
    async fn fs_node_effective_sid_valid_xattr_stored() {
        spawn_kernel_with_selinux_hooks_test_policy_and_run(|current_task, security_server| {
            let dir_entry = &testing::create_test_file(current_task).entry;
            let non_private_dir_entry = NonPrivateDirEntry::new(dir_entry).unwrap();
            let node = non_private_dir_entry.node();

            // Store a valid Security Context in the attribute, then clear the cached label and
            // re-resolve it. The hooks test policy defines that "tmpfs" use "fs_use_xattr"
            // labeling, which should result in the (valid) label being read from the file, and
            // the corresponding SID cached.
            node.ops()
                .set_xattr(
                    &node,
                    &current_task,
                    XATTR_NAME_SELINUX.to_bytes().into(),
                    VALID_SECURITY_CONTEXT.into(),
                    XattrOp::Set,
                )
                .expect("setxattr");
            clear_cached_sid(&node);
            assert_eq!(None, get_cached_sid(&node));
            fs_node_init_with_dentry(
                &security_server,
                &current_task,
                non_private_dir_entry,
                /* read_xaddr = */
                true,
            )
            .expect("fs_node_init_with_dentry");

            // `fs_node_getsecurity()` should report the same valid Security Context string as the xattr holds.
            let result = fs_node_getsecurity(
                &security_server,
                &current_task,
                node,
                XATTR_NAME_SELINUX.to_bytes().into(),
                SECURITY_SELINUX_XATTR_VALUE_MAX_SIZE,
            )
            .unwrap();
            assert_eq!(result, Some(ValueOrSize::Value(VALID_SECURITY_CONTEXT_WITH_NUL.into())));

            // There should be a SID cached, and it should map to the valid Security Context.
            let cached_sid = get_cached_sid(&node).unwrap();
            assert_eq!(
                security_server.sid_to_security_context(cached_sid).unwrap(),
                VALID_SECURITY_CONTEXT
            );

            // Requesting the effective SID should simply return the cached value.
            assert_eq!(cached_sid, fs_node_effective_sid_and_class(node).sid);
        })
        .await;
    }

    #[fuchsia::test]
    async fn setxattr_set_sid() {
        spawn_kernel_with_selinux_hooks_test_policy_and_run(|current_task, security_server| {
            let expected_sid = security_server
                .security_context_to_sid(VALID_SECURITY_CONTEXT.into())
                .expect("no SID for VALID_SECURITY_CONTEXT");
            let node = &testing::create_test_file(current_task).entry.node;

            node.set_xattr(
                current_task,
                &current_task.fs().root().mount,
                XATTR_NAME_SELINUX.to_bytes().into(),
                VALID_SECURITY_CONTEXT.into(),
                XattrOp::Set,
            )
            .expect("setxattr");

            // Verify that the SID now cached on the node corresponds to VALID_SECURITY_CONTEXT.
            assert_eq!(Some(expected_sid), get_cached_sid(node));
        })
        .await;
    }

    #[fuchsia::test]
    async fn get_fs_relative_path_root() {
        // Verify the full path for the root entry.
        spawn_kernel_and_run(async |current_task| {
            let dir_entry = current_task.fs().root().entry;

            assert_eq!(BStr::new(b"/"), get_fs_relative_path(&dir_entry));
        })
        .await;
    }

    #[fuchsia::test]
    async fn get_fs_relative_path_simple_file() {
        // Verify the full path for a file directly under the root: "/" + [`TEST_FILE_NAME`].
        spawn_kernel_and_run(async |current_task| {
            let dir_entry = &testing::create_test_file(current_task).entry;

            let expected = format!("/{}", TEST_FILE_NAME);
            assert_eq!(BStr::new(&expected), get_fs_relative_path(&dir_entry));
        })
        .await;
    }

    #[fuchsia::test]
    async fn get_fs_relative_path_nested_dir() {
        // Verify the full path for a nested directory: "/foo/bar".
        spawn_kernel_and_run(async |current_task| {
            let dir_entry = &testing::create_directory_with_parents(
                vec![BStr::new(b"foo"), BStr::new(b"bar")],
                &current_task,
            )
            .entry;

            assert_eq!(BStr::new(b"/foo/bar"), get_fs_relative_path(&dir_entry));
        })
        .await;
    }

    fn spawn_mock_fuchsia_file(selinux_context: Option<fio::SelinuxContext>) -> zx::Channel {
        use fidl::endpoints::DiscoverableProtocolMarker as _;
        use futures::StreamExt as _;

        let (client, server) = zx::Channel::create();
        std::thread::spawn(move || {
            let mut executor = fuchsia_async::LocalExecutor::default();
            executor.run_singlethreaded(async move {
                let mut stream =
                    fidl::endpoints::ServerEnd::<fio::FileMarker>::new(server).into_stream();
                while let Some(Ok(request)) = stream.next().await {
                    match request {
                        fio::FileRequest::Query { responder } => {
                            let _ = responder.send(fio::FileMarker::PROTOCOL_NAME.as_bytes());
                        }
                        fio::FileRequest::Describe { responder } => {
                            let _ = responder.send(fio::FileInfo::default());
                        }
                        fio::FileRequest::GetAttributes { responder, .. } => {
                            let mutable = fio::MutableNodeAttributes {
                                selinux_context: selinux_context.clone(),
                                ..Default::default()
                            };
                            let immutable = fio::ImmutableNodeAttributes {
                                protocols: Some(fio::NodeProtocolKinds::FILE),
                                ..Default::default()
                            };
                            let _ = responder.send(Ok((&mutable, &immutable)));
                        }
                        fio::FileRequest::Close { responder } => {
                            let _ = responder.send(Ok(()));
                            break;
                        }
                        _ => panic!("Unexpected FileRequest: {request:?}"),
                    }
                }
            });
        });
        client
    }

    #[fuchsia::test]
    async fn remote_file_and_pipe_labeling() {
        spawn_kernel_with_selinux_hooks_test_policy_and_run(|current_task, security_server| {
            let task_sid = security_server
                .security_context_to_sid(VALID_SECURITY_CONTEXT.into())
                .expect("invalid security context");
            testing::mutate_attrs_for_test(current_task, |attrs| attrs.current_sid = task_sid);

            // Fuchsia stream socket wrapped via `create_fuchsia_pipe` is labeled as a `pipefs`
            // `fifo_file` with the creating task's SID.
            let (pipe_client, _pipe_server) = zx::Socket::create_stream();
            let pipe_file = create_fuchsia_pipe(current_task, pipe_client, OpenFlags::RDWR)
                .expect("create_fuchsia_pipe");
            assert_eq!(
                fs_node_effective_sid_and_class(NonPrivateFsNode::new(pipe_file.node()).unwrap()),
                FsNodeSidAndClass { sid: task_sid, class: FsNodeClass::File(FileClass::FifoFile) }
            );

            // Fuchsia stream socket wrapped via `create_file_from_handle` (used for component
            // startup handles such as stdio) is labeled as a `pipefs` `fifo_file` with the creating
            // task's SID.
            let (stdio_client, _stdio_server) = zx::Socket::create_stream();
            let stdio_file = create_file_from_handle(current_task, stdio_client.into())
                .expect("create_file_from_handle socket");
            assert_eq!(
                fs_node_effective_sid_and_class(NonPrivateFsNode::new(stdio_file.node()).unwrap()),
                FsNodeSidAndClass { sid: task_sid, class: FsNodeClass::File(FileClass::FifoFile) }
            );

            // Fuchsia stream socket wrapped via `new_remote_file` is labeled as a `sockfs`
            // `unix_stream_socket` with the creating task's SID.
            let (sock_client, _sock_server) = zx::Socket::create_stream();
            let sock_file = new_remote_file(current_task, sock_client.into(), OpenFlags::RDWR)
                .expect("new_remote_file socket");
            assert_eq!(
                fs_node_effective_sid_and_class(NonPrivateFsNode::new(sock_file.node()).unwrap()),
                FsNodeSidAndClass {
                    sid: task_sid,
                    class: FsNodeClass::Socket(SocketClass::UnixStreamSocket)
                }
            );

            // Fuchsia `fuchsia.io/File` and `fuchsia.io/Directory` channels wrapped via
            // `new_remote_file` are labeled on `remotefs` with `FileClass::File` / `FileClass::Dir`
            // and the default `file` initial SID when no `selinux_context` attribute is present.
            let (file_server, file_client) = zx::Channel::create();
            fdio::open("/pkg/meta/contents", fio::PERM_READABLE, file_server)
                .expect("open /pkg/meta/contents");
            let remote_file = new_remote_file(current_task, file_client.into(), OpenFlags::RDONLY)
                .expect("new_remote_file file");
            assert_eq!(
                fs_node_effective_sid_and_class(NonPrivateFsNode::new(remote_file.node()).unwrap()),
                FsNodeSidAndClass {
                    sid: InitialSid::File.into(),
                    class: FsNodeClass::File(FileClass::File)
                }
            );

            let (dir_server, dir_client) = zx::Channel::create();
            fdio::open("/pkg", fio::PERM_READABLE | fio::PERM_EXECUTABLE, dir_server)
                .expect("open /pkg");
            let remote_dir = new_remote_file(current_task, dir_client.into(), OpenFlags::RDONLY)
                .expect("new_remote_file directory");
            assert_eq!(
                fs_node_effective_sid_and_class(NonPrivateFsNode::new(remote_dir.node()).unwrap()),
                FsNodeSidAndClass {
                    sid: InitialSid::File.into(),
                    class: FsNodeClass::File(FileClass::Dir)
                }
            );

            // Fuchsia `fuchsia.io/File` channel reporting a valid `SELINUX_CONTEXT` attribute is
            // labeled with the reported context.
            let labeled_channel = spawn_mock_fuchsia_file(Some(fio::SelinuxContext::Data(
                VALID_SECURITY_CONTEXT.to_vec(),
            )));
            let labeled_file =
                new_remote_file(current_task, labeled_channel.into(), OpenFlags::RDONLY)
                    .expect("new_remote_file labeled file");
            assert_eq!(
                fs_node_effective_sid_and_class(
                    NonPrivateFsNode::new(labeled_file.node()).unwrap()
                ),
                FsNodeSidAndClass { sid: task_sid, class: FsNodeClass::File(FileClass::File) }
            );

            // Fuchsia `fuchsia.io/File` channel reporting an invalid `SELINUX_CONTEXT` attribute
            // is labeled as `Unlabeled`.
            let invalid_channel = spawn_mock_fuchsia_file(Some(fio::SelinuxContext::Data(
                b"invalid_context".to_vec(),
            )));
            let invalid_file =
                new_remote_file(current_task, invalid_channel.into(), OpenFlags::RDONLY)
                    .expect("new_remote_file invalid context file");
            assert_eq!(
                fs_node_effective_sid_and_class(
                    NonPrivateFsNode::new(invalid_file.node()).unwrap()
                ),
                FsNodeSidAndClass {
                    sid: InitialSid::Unlabeled.into(),
                    class: FsNodeClass::File(FileClass::File)
                }
            );

            // VMO and Counter handles wrapped via `new_remote_file` use kernel-private nodes.
            let vmo = zx::Vmo::create(*PAGE_SIZE).expect("Vmo::create");
            let vmo_file =
                new_remote_file(current_task, vmo.into(), OpenFlags::RDWR).expect("vmo file");
            assert!(vmo_file.node().is_private());
            assert!(vmo_file.node().is_reg());

            let counter = zx::Counter::create();
            let counter_file = new_remote_file(current_task, counter.into(), OpenFlags::RDONLY)
                .expect("counter file");
            assert!(counter_file.node().is_private());
        })
        .await;
    }
}
