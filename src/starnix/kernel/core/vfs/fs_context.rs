// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::security;
use crate::task::{CurrentTask, MountsWriteToken};
use crate::vfs::{AccessCheck, ActiveNamespaceNode, Namespace, NamespaceNode};
use fuchsia_rcu::RcuArc;
use starnix_logging::log_trace;
use starnix_sync::LockDepRwLock;
use starnix_uapi::auth::CAP_SYS_CHROOT;
use starnix_uapi::errno;
use starnix_uapi::errors::Errno;
use starnix_uapi::file_mode::FileMode;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The mutable state for an FsContext.
///
/// This state is cloned in FsContext::fork.
#[derive(Debug, Clone)]
struct FsContextState {
    /// The namespace tree for this FsContext.
    ///
    /// This field owns the mount table for this FsContext.
    namespace: Arc<Namespace>,

    /// The root of the namespace tree for this FsContext.
    ///
    /// Operations on the file system are typically either relative to this
    /// root or to the cwd().
    root: ActiveNamespaceNode,

    /// The current working directory.
    cwd: ActiveNamespaceNode,

    // See <https://man7.org/linux/man-pages/man2/umask.2.html>
    umask: FileMode,
}

impl FsContextState {
    fn set_namespace(
        &mut self,
        new_ns: Arc<Namespace>,
        mounts_guard: &MountsWriteToken,
    ) -> Result<(), Errno> {
        log_trace!("updating namespace");
        let new_root = Namespace::translate_node(self.root.to_passive(), &new_ns, mounts_guard)
            .ok_or_else(|| errno!(EINVAL))?;
        let new_cwd = Namespace::translate_node(self.cwd.to_passive(), &new_ns, mounts_guard)
            .ok_or_else(|| errno!(EINVAL))?;

        // Only perform a mutation if the rebased nodes both exist in the target namespace.
        mounts_guard.defer_drop(std::mem::replace(&mut self.root, new_root.into_active()));
        mounts_guard.defer_drop(std::mem::replace(&mut self.cwd, new_cwd.into_active()));
        mounts_guard.defer_drop(std::mem::replace(&mut self.namespace, new_ns));
        log_trace!("namespace update succeeded");
        Ok(())
    }
}

/// The file system context associated with a task.
///
/// File system operations, such as opening a file or mounting a directory, are
/// performed using this context.
#[derive(Debug)]
pub struct FsContext {
    state: LockDepRwLock<FsContextState, starnix_sync::FsContextStateLock>,

    /// Number of running tasks that have this `FsContext` as their active filesystem context.
    ///
    /// Tracked separately from `Arc::strong_count` so that transient `Arc<FsContext>` references
    /// do not cause a task to appear to share its filesystem context.
    share_count: AtomicUsize,
}

/// Wrapper around [`RcuArc<FsContext>`] that maintains the [`FsContext`] share count.
#[derive(Debug)]
pub struct SharedFsContext(RcuArc<FsContext>);

impl SharedFsContext {
    /// Creates a [`SharedFsContext`] holding `fs` and increments its share count.
    pub fn new(fs: Arc<FsContext>) -> Self {
        fs.share_count.fetch_add(1, Ordering::Relaxed);
        Self(RcuArc::new(Some(fs)))
    }

    /// Returns a strong reference to the underlying [`FsContext`], or `None` if cleared on exit.
    pub fn upgrade(&self) -> Option<Arc<FsContext>> {
        self.0.upgrade()
    }

    /// Replaces the held [`FsContext`] for `current_task` and updates the old and new share counts.
    pub fn update(&self, current_task: &CurrentTask, fs: impl Into<Option<Arc<FsContext>>>) {
        // Requiring `&CurrentTask` (`!Sync`) and verifying that `self` belongs to `current_task`
        // serializes updates so `self.set()` cannot race with another writer on `self`.
        debug_assert!(std::ptr::eq(&current_task.running_state().fs, self));
        self.set(fs.into());
    }

    fn set(&self, fs: Option<Arc<FsContext>>) {
        if let Some(ref fs) = fs {
            fs.share_count.fetch_add(1, Ordering::Relaxed);
        }
        let old_fs = self.0.upgrade();
        self.0.update(fs);
        if let Some(old_fs) = old_fs {
            let previous = old_fs.share_count.fetch_sub(1, Ordering::AcqRel);
            debug_assert!(previous >= 1);
        }
    }
}

impl Drop for SharedFsContext {
    fn drop(&mut self) {
        self.set(None);
    }
}

impl FsContext {
    /// Create an FsContext for the given namespace.
    ///
    /// The root and cwd of the FsContext are initialized to the root of the
    /// namespace.
    pub fn new(namespace: Arc<Namespace>) -> Arc<FsContext> {
        let root = namespace.root();
        Arc::new(FsContext {
            state: FsContextState {
                namespace,
                root: root.clone().into_active(),
                cwd: root.into_active(),
                umask: FileMode::DEFAULT_UMASK,
            }
            .into(),
            share_count: AtomicUsize::new(0),
        })
    }

    pub fn fork(&self) -> Arc<FsContext> {
        // A child process created via fork(2) inherits its parent's umask.
        // The umask is left unchanged by execve(2).
        //
        // See <https://man7.org/linux/man-pages/man2/umask.2.html>

        Arc::new(FsContext {
            state: self.state.read().clone().into(),
            share_count: AtomicUsize::new(0),
        })
    }

    /// Returns whether more than one running task shares this filesystem context.
    pub fn is_shared(&self) -> bool {
        self.share_count.load(Ordering::Acquire) > 1
    }

    /// Returns a reference to the current working directory.
    pub fn cwd(&self) -> NamespaceNode {
        let state = self.state.read();
        state.cwd.to_passive()
    }

    /// Returns the root.
    pub fn root(&self) -> NamespaceNode {
        let state = self.state.read();
        state.root.to_passive()
    }

    /// Change the current working directory.
    pub fn chdir(&self, current_task: &CurrentTask, name: NamespaceNode) -> Result<(), Errno> {
        name.check_access(current_task, AccessCheck::for_chdir())?;
        let mut state = self.state.write();
        state.cwd = name.into_active();
        Ok(())
    }

    /// Change the root.
    pub fn chroot(&self, current_task: &CurrentTask, name: NamespaceNode) -> Result<(), Errno> {
        name.check_access(current_task, AccessCheck::for_chroot()).map_err(|_| errno!(EACCES))?;
        security::check_task_capable(current_task, CAP_SYS_CHROOT)?;

        let mut state = self.state.write();
        state.root = name.into_active();
        Ok(())
    }

    pub fn umask(&self) -> FileMode {
        self.state.read().umask
    }

    pub fn apply_umask(&self, mode: FileMode) -> FileMode {
        let umask = self.state.read().umask;
        mode & !umask
    }

    pub fn set_umask(&self, umask: FileMode) -> FileMode {
        let mut state = self.state.write();
        let old_umask = state.umask;

        // umask() sets the calling process's file mode creation mask
        // (umask) to mask & 0o777 (i.e., only the file permission bits of
        // mask are used), and returns the previous value of the mask.
        //
        // See <https://man7.org/linux/man-pages/man2/umask.2.html>
        state.umask = umask & FileMode::from_bits(0o777);

        old_umask
    }

    pub fn set_namespace(&self, new_ns: Arc<Namespace>) -> Result<(), Errno> {
        let mut state = self.state.write();
        let kernel = state.namespace.kernel();
        let mounts_guard = kernel.mounts_lock();
        state.set_namespace(new_ns, &mounts_guard)?;
        Ok(())
    }

    /// Replaces the bootstrap `Namespace` backing this `FsContext`, resetting both `root` and
    /// `cwd` to the root of `new_ns`.
    ///
    /// # Panics
    ///
    /// Panics if the outgoing bootstrap `Namespace`, its root mount, or its underlying filesystem
    /// still have any other references or created entries.
    pub fn replace_bootstrap_namespace(&self, current_task: &CurrentTask, new_ns: Arc<Namespace>) {
        let mut state = self.state.write();
        let root = new_ns.root();
        let old_root = std::mem::replace(&mut state.root, root.clone().into_active());
        let old_cwd = std::mem::replace(&mut state.cwd, root.into_active());
        let old_ns = std::mem::replace(&mut state.namespace, new_ns);
        drop(state);
        drop((old_root, old_cwd));
        old_ns.destroy_bootstrap();
        current_task.trigger_delayed_releaser();
    }

    pub fn unshare_namespace(&self) {
        let mut state = self.state.write();
        let kernel = state.namespace.kernel();
        let mounts_guard = kernel.mounts_lock();

        let cloned = state.namespace.clone_namespace(&mounts_guard);
        state
            .set_namespace(cloned, &mounts_guard)
            .expect("nodes should exist in the cloned namespace");
    }

    pub fn namespace(&self) -> Arc<Namespace> {
        Arc::clone(&self.state.read().namespace)
    }

    pub fn update_root_after_pivot(&self, old_root: &NamespaceNode, new_root: &NamespaceNode) {
        let mut state = self.state.write();
        if state.root.to_passive() == *old_root {
            state.root = new_root.clone().into_active();
        }
        if state.cwd.to_passive() == *old_root {
            state.cwd = new_root.clone().into_active();
        }
    }
}

#[cfg(test)]
mod test {
    use crate::fs::tmpfs::TmpFs;
    use crate::testing::{spawn_kernel_and_run, spawn_kernel_and_run_with_pkgfs};
    use crate::vfs::{FsContext, Namespace};
    use starnix_uapi::file_mode::FileMode;
    use starnix_uapi::open_flags::OpenFlags;

    #[::fuchsia::test]
    async fn test_umask() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            let fs = FsContext::new(Namespace::new(TmpFs::new_fs(&kernel)));

            assert_eq!(FileMode::from_bits(0o22), fs.set_umask(FileMode::from_bits(0o3020)));
            assert_eq!(FileMode::from_bits(0o646), fs.apply_umask(FileMode::from_bits(0o666)));
            assert_eq!(FileMode::from_bits(0o3646), fs.apply_umask(FileMode::from_bits(0o3666)));
            assert_eq!(FileMode::from_bits(0o20), fs.set_umask(FileMode::from_bits(0o11)));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_chdir() {
        spawn_kernel_and_run_with_pkgfs(async |current_task| {
            assert_eq!("/", current_task.fs().cwd().path_escaping_chroot());

            let bin = current_task
                .open_file("bin".into(), OpenFlags::RDONLY)
                .expect("missing bin directory");
            current_task.fs().chdir(&current_task, bin.name.to_passive()).expect("Failed to chdir");
            assert_eq!("/bin", current_task.fs().cwd().path_escaping_chroot());

            // Now that we have changed directories to bin, we're opening a file
            // relative to that directory, which doesn't exist.
            assert!(current_task.open_file("bin".into(), OpenFlags::RDONLY).is_err());

            // However, bin still exists in the root directory.
            assert!(current_task.open_file("/bin".into(), OpenFlags::RDONLY).is_ok());

            let previous_directory = current_task
                .open_file("..".into(), OpenFlags::RDONLY)
                .expect("failed to open ..")
                .name
                .to_passive();
            current_task.fs().chdir(&current_task, previous_directory).expect("Failed to chdir");
            assert_eq!("/", current_task.fs().cwd().path_escaping_chroot());

            // Now bin exists again because we've gone back to the root.
            assert!(current_task.open_file("bin".into(), OpenFlags::RDONLY).is_ok());

            // Repeating the .. doesn't do anything because we're already at the root.
            let previous_directory = current_task
                .open_file("..".into(), OpenFlags::RDONLY)
                .expect("failed to open ..")
                .name
                .to_passive();
            current_task.fs().chdir(&current_task, previous_directory).expect("Failed to chdir");
            assert_eq!("/", current_task.fs().cwd().path_escaping_chroot());
            assert!(current_task.open_file("bin".into(), OpenFlags::RDONLY).is_ok());
        })
        .await;
    }
}
