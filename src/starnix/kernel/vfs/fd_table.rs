// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::task::{CurrentTaskAndLocked, Task, register_delayed_release};
use crate::vfs::{FdNumber, FileHandle, FileReleaser};
use bitflags::bitflags;
use fuchsia_inspect_contrib::profile_duration;
use fuchsia_rcu::rcu_arc::RcuArc;
use starnix_sync::{LockBefore, Locked, Mutex, ThreadGroupLimits};
use starnix_syscalls::SyscallResult;
use starnix_types::ownership::{Releasable, ReleasableByRef};
use starnix_uapi::errors::Errno;
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::resource_limits::Resource;
use starnix_uapi::{FD_CLOEXEC, errno, error};
use static_assertions::const_assert;
use std::sync::Arc;

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct FdFlags: u32 {
        const CLOEXEC = FD_CLOEXEC;
    }
}

impl std::convert::From<FdFlags> for SyscallResult {
    fn from(value: FdFlags) -> Self {
        value.bits().into()
    }
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct FdTableId(usize);

impl FdTableId {
    fn new(id: *const Vec<EncodedEntry>) -> Self {
        Self(id as usize)
    }

    pub fn raw(&self) -> usize {
        self.0
    }
}

const FLAGS_MASK: usize = 0x1;

/// An encoded entry in an `FdTable`.
///
/// Encodes both the `FileHandle` and the CLOEXEC bit. Can either hold an entry or be empty.
#[derive(Debug, Default)]
struct EncodedEntry {
    /// Rather than using a separate "flags" field, we encode the table entry into a single usize.
    ///
    /// If `value` is zero, the entry is empty.
    ///
    /// The lowest bit of `value` is the CLOEXEC bit.
    ///
    /// The remaining bits of `value` are a `FileHandle` converted to a raw pointer.
    value: usize,
}

const_assert!(std::mem::align_of::<*const FileReleaser>() >= 1 << FLAGS_MASK);

impl EncodedEntry {
    fn encode(file: FileHandle, flags: FdFlags) -> usize {
        let ptr = Arc::into_raw(file) as usize;
        let flags = (flags.bits() as usize) & FLAGS_MASK;
        ptr | flags
    }

    fn new(entry: FdTableEntry) -> Self {
        Self { value: Self::encode(entry.file, entry.flags) }
    }

    fn is_some(&self) -> bool {
        self.value != 0
    }

    fn is_none(&self) -> bool {
        self.value == 0
    }

    fn flags(&self) -> Option<FdFlags> {
        if self.is_none() {
            return None;
        }
        Some(self.unchecked_flags())
    }

    fn set_flags(&mut self, flags: FdFlags) {
        assert!(self.is_some());
        self.value = self.value & !FLAGS_MASK | (flags.bits() as usize) & FLAGS_MASK;
    }

    fn file(&self) -> Option<FileHandle> {
        if self.is_none() {
            return None;
        }
        let ptr = self.unchecked_ptr();
        // SAFETY: The pointer is valid because it was encoded in `self.value`.
        unsafe {
            Arc::increment_strong_count(ptr);
            Some(Arc::from_raw(ptr))
        }
    }

    fn set_file(&mut self, id: FdTableId, file: FileHandle) {
        assert!(self.is_some());
        let flags = self.unchecked_flags();
        self.replace(id, Self::encode(file, flags));
    }

    fn unchecked_ptr(&self) -> *const FileReleaser {
        (self.value & !FLAGS_MASK) as *const _
    }

    fn unchecked_flags(&self) -> FdFlags {
        FdFlags::from_bits_truncate((self.value & FLAGS_MASK) as u32)
    }

    fn to_entry(&self) -> Option<FdTableEntry> {
        self.file().map(|file| FdTableEntry { file, flags: self.unchecked_flags() })
    }

    fn clear(&mut self, id: FdTableId) -> bool {
        if self.is_none() {
            return false;
        }
        self.replace(id, 0);
        true
    }

    fn replace(&mut self, id: FdTableId, value: usize) {
        let ptr = self.unchecked_ptr();
        self.value = value;
        if !ptr.is_null() {
            // SAFETY: The pointer is valid because it was encoded in `self.value`.
            let file = unsafe { Arc::from_raw(ptr) };
            register_delayed_release(FlushedFile(file, id));
        }
    }
}

impl Clone for EncodedEntry {
    fn clone(&self) -> Self {
        if let Some(entry) = self.to_entry() { Self::new(entry) } else { Self::default() }
    }
}

#[derive(Debug, Clone)]
struct FdTableEntry {
    file: FileHandle,
    flags: FdFlags,
}

struct FlushedFile(FileHandle, FdTableId);

impl Releasable for FlushedFile {
    type Context<'a> = CurrentTaskAndLocked<'a>;
    fn release<'a>(self, context: Self::Context<'a>) {
        let (locked, current_task) = context;
        let FlushedFile(file, id) = self;
        file.flush(locked, current_task, id);
    }
}

/// Having the map a separate data structure allows us to memoize next_fd, which is the
/// lowest numbered file descriptor not in use.
#[derive(Debug, Clone)]
struct FdTableStore {
    entries: Vec<EncodedEntry>,
    next_fd: FdNumber,
}

impl Default for FdTableStore {
    fn default() -> Self {
        FdTableStore { entries: Default::default(), next_fd: FdNumber::from_raw(0) }
    }
}

impl FdTableStore {
    fn insert_entry(
        &mut self,
        id: FdTableId,
        fd: FdNumber,
        rlimit: u64,
        entry: FdTableEntry,
    ) -> Result<bool, Errno> {
        let raw_fd = fd.raw();
        if raw_fd < 0 {
            return error!(EBADF);
        }
        if raw_fd as u64 >= rlimit {
            return error!(EMFILE);
        }
        if raw_fd == self.next_fd.raw() {
            self.next_fd = self.calculate_lowest_available_fd(&FdNumber::from_raw(raw_fd + 1));
        }
        let raw_fd = raw_fd as usize;
        if raw_fd >= self.entries.len() {
            self.entries.resize(raw_fd + 1, Default::default());
        }
        let mut entry = EncodedEntry::new(entry);
        std::mem::swap(&mut entry, &mut self.entries[raw_fd]);
        Ok(entry.clear(id))
    }

    fn remove_entry(&mut self, id: FdTableId, fd: &FdNumber) -> bool {
        let raw_fd = fd.raw() as usize;
        if raw_fd >= self.entries.len() {
            return false;
        }
        let removed = self.entries[raw_fd].clear(id);
        if removed && raw_fd < self.next_fd.raw() as usize {
            self.next_fd = *fd;
        }
        removed
    }

    fn is_some(&self, fd: FdNumber) -> bool {
        let raw_fd = fd.raw() as usize;
        self.entries.get(raw_fd).map_or(false, |entry| entry.is_some())
    }

    fn get_file(&self, fd: FdNumber) -> Option<FileHandle> {
        self.entries.get(fd.raw() as usize).map(EncodedEntry::file)?
    }

    fn get_entry(&self, fd: FdNumber) -> Option<FdTableEntry> {
        self.entries.get(fd.raw() as usize).map(EncodedEntry::to_entry)?
    }

    fn set_fd_flags(&mut self, fd: FdNumber, flags: FdFlags) -> Result<(), Errno> {
        let raw_fd = fd.raw() as usize;
        if raw_fd >= self.entries.len() {
            return error!(EBADF);
        }
        if self.entries[raw_fd].is_none() {
            return error!(EBADF);
        }
        self.entries[raw_fd].set_flags(flags);
        Ok(())
    }

    // Returns the (possibly memoized) lowest available FD >= minfd in this map.
    fn get_lowest_available_fd(&self, minfd: FdNumber) -> FdNumber {
        if minfd.raw() > self.next_fd.raw() {
            return self.calculate_lowest_available_fd(&minfd);
        }
        self.next_fd
    }

    // Recalculates the lowest available FD >= minfd based on the contents of the map.
    fn calculate_lowest_available_fd(&self, minfd: &FdNumber) -> FdNumber {
        let mut fd = *minfd;
        while self.is_some(fd) {
            fd = FdNumber::from_raw(fd.raw() + 1);
        }
        fd
    }

    fn retain<F>(&mut self, id: FdTableId, mut f: F)
    where
        F: FnMut(FdNumber, &mut FdFlags) -> bool,
    {
        for (index, encoded_entry) in self.entries.iter_mut().enumerate() {
            let fd = FdNumber::from_raw(index as i32);
            if let Some(flags) = encoded_entry.flags() {
                let mut modified_flags = flags;
                if !f(fd, &mut modified_flags) {
                    encoded_entry.clear(id);
                } else if modified_flags != flags {
                    encoded_entry.set_flags(modified_flags);
                }
            }
        }
        self.next_fd = self.calculate_lowest_available_fd(&FdNumber::from_raw(0));
    }
}

#[derive(Debug, Default)]
struct FdTableInner {
    store: Mutex<FdTableStore>,
}

impl FdTableInner {
    fn id(&self) -> FdTableId {
        FdTableId::new(&self.store.lock().entries as *const Vec<EncodedEntry>)
    }

    fn unshare(&self) -> Arc<Self> {
        Arc::new(self.clone())
    }
}

impl Clone for FdTableInner {
    fn clone(&self) -> FdTableInner {
        let cloned_store = self.store.lock().clone();
        FdTableInner { store: Mutex::new(cloned_store) }
    }
}

impl Drop for FdTableInner {
    fn drop(&mut self) {
        let id = self.id();
        let store = self.store.get_mut();
        for encoded_entry in store.entries.iter_mut() {
            encoded_entry.clear(id);
        }
    }
}

#[derive(Debug, Default)]
pub struct FdTable {
    inner: RcuArc<FdTableInner>,
}

pub enum TargetFdNumber {
    /// The duplicated FdNumber will be the smallest available FdNumber.
    Default,

    /// The duplicated FdNumber should be this specific FdNumber.
    Specific(FdNumber),

    /// The duplicated FdNumber should be greater than this FdNumber.
    Minimum(FdNumber),
}

impl FdTable {
    pub fn id(&self) -> FdTableId {
        self.inner.read().id()
    }

    /// Returns new unshared FD table populated with the same FD->`FileObject` mappings as `self`.
    pub fn fork(&self) -> FdTable {
        let unshared = self.inner.read().unshare();
        FdTable { inner: RcuArc::new(unshared) }
    }

    /// Ensures that this FD table is not shared by any other `FdTable` instance(s).
    pub fn unshare(&self) {
        let unshared = self.inner.read().unshare();
        self.inner.update_sync(unshared);
    }

    /// Trims close-on-exec FDs from the table.
    pub fn exec(&self) {
        self.retain(|_fd, flags| !flags.contains(FdFlags::CLOEXEC));
    }

    pub fn insert<L>(
        &self,
        locked: &mut Locked<L>,
        task: &Task,
        fd: FdNumber,
        file: FileHandle,
    ) -> Result<(), Errno>
    where
        L: LockBefore<ThreadGroupLimits>,
    {
        self.insert_with_flags(locked, task, fd, file, FdFlags::empty())
    }

    pub fn insert_with_flags<L>(
        &self,
        locked: &mut Locked<L>,
        task: &Task,
        fd: FdNumber,
        file: FileHandle,
        flags: FdFlags,
    ) -> Result<(), Errno>
    where
        L: LockBefore<ThreadGroupLimits>,
    {
        let rlimit = task.thread_group().get_rlimit(locked, Resource::NOFILE);
        let inner = self.inner.read();
        let id = inner.id();
        let mut state = inner.store.lock();
        state.insert_entry(id, fd, rlimit, FdTableEntry { file, flags })?;
        Ok(())
    }

    pub fn add_with_flags<L>(
        &self,
        locked: &mut Locked<L>,
        task: &Task,
        file: FileHandle,
        flags: FdFlags,
    ) -> Result<FdNumber, Errno>
    where
        L: LockBefore<ThreadGroupLimits>,
    {
        profile_duration!("AddFd");
        let rlimit = task.thread_group().get_rlimit(locked, Resource::NOFILE);
        let inner = self.inner.read();
        let id = inner.id();
        let mut state = inner.store.lock();
        let fd = state.next_fd;
        state.insert_entry(id, fd, rlimit, FdTableEntry { file, flags })?;
        Ok(fd)
    }

    // Duplicates a file handle.
    // If target is  TargetFdNumber::Minimum, a new FdNumber is allocated. Returns the new FdNumber.
    pub fn duplicate<L>(
        &self,
        locked: &mut Locked<L>,
        task: &Task,
        oldfd: FdNumber,
        target: TargetFdNumber,
        flags: FdFlags,
    ) -> Result<FdNumber, Errno>
    where
        L: LockBefore<ThreadGroupLimits>,
    {
        profile_duration!("DuplicateFd");
        let result = {
            let rlimit = task.thread_group().get_rlimit(locked, Resource::NOFILE);
            let inner = self.inner.read();
            let id = inner.id();
            let mut state = inner.store.lock();
            let file = state.get_file(oldfd).ok_or_else(|| errno!(EBADF))?;

            let fd = match target {
                TargetFdNumber::Specific(fd) => {
                    // We need to check the rlimit before we remove the entry from state
                    // because we cannot error out after removing the entry.
                    if fd.raw() as u64 >= rlimit {
                        // ltp_dup201 shows that we're supposed to return EBADF in this
                        // situtation, instead of EMFILE, which is what we normally return
                        // when we're past the rlimit.
                        return error!(EBADF);
                    }
                    state.remove_entry(id, &fd);
                    fd
                }
                TargetFdNumber::Minimum(fd) => state.get_lowest_available_fd(fd),
                TargetFdNumber::Default => state.get_lowest_available_fd(FdNumber::from_raw(0)),
            };
            let existing_entry =
                state.insert_entry(id, fd, rlimit, FdTableEntry { file, flags })?;
            assert!(!existing_entry);
            Ok(fd)
        };
        result
    }

    pub fn get_allowing_opath(&self, fd: FdNumber) -> Result<FileHandle, Errno> {
        self.get_allowing_opath_with_flags(fd).map(|(file, _flags)| file)
    }

    pub fn get_allowing_opath_with_flags(
        &self,
        fd: FdNumber,
    ) -> Result<(FileHandle, FdFlags), Errno> {
        profile_duration!("GetFdWithFlags");
        let inner = self.inner.read();
        let state = inner.store.lock();
        state.get_entry(fd).map(|entry| (entry.file, entry.flags)).ok_or_else(|| errno!(EBADF))
    }

    pub fn get(&self, fd: FdNumber) -> Result<FileHandle, Errno> {
        let file = self.get_allowing_opath(fd)?;
        if file.flags().contains(OpenFlags::PATH) {
            return error!(EBADF);
        }
        Ok(file)
    }

    pub fn close(&self, fd: FdNumber) -> Result<(), Errno> {
        profile_duration!("CloseFile");
        let inner = self.inner.read();
        let id = inner.id();
        let mut state = inner.store.lock();
        if state.remove_entry(id, &fd) { Ok(()) } else { error!(EBADF) }
    }

    pub fn get_fd_flags_allowing_opath(&self, fd: FdNumber) -> Result<FdFlags, Errno> {
        self.get_allowing_opath_with_flags(fd).map(|(_file, flags)| flags)
    }

    pub fn set_fd_flags(&self, fd: FdNumber, flags: FdFlags) -> Result<(), Errno> {
        profile_duration!("SetFdFlags");
        let inner = self.inner.read();
        let mut state = inner.store.lock();
        let file = state.get_file(fd).ok_or_else(|| errno!(EBADF))?;
        if file.flags().contains(OpenFlags::PATH) {
            return error!(EBADF);
        }
        state.set_fd_flags(fd, flags)
    }

    pub fn set_fd_flags_allowing_opath(&self, fd: FdNumber, flags: FdFlags) -> Result<(), Errno> {
        profile_duration!("SetFdFlagsAllowingOpath");
        self.inner.read().store.lock().set_fd_flags(fd, flags)
    }

    /// Retains only the FDs matching the predicate `f`.
    pub fn retain<F>(&self, f: F)
    where
        F: Fn(FdNumber, &mut FdFlags) -> bool,
    {
        profile_duration!("RetainFds");
        let inner = self.inner.read();
        let id = inner.id();
        let mut state = inner.store.lock();
        state.retain(id, |fd, flags| f(fd, flags));
    }

    /// Returns a vector of all current file descriptors in the table.
    pub fn get_all_fds(&self) -> Vec<FdNumber> {
        self.inner
            .read()
            .store
            .lock()
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, encoded_entry)| {
                if encoded_entry.is_none() { None } else { Some(FdNumber::from_raw(index as i32)) }
            })
            .collect()
    }

    /// Executes `predicate(file) => maybe_replacement` on every non-empty table entry. Replaces
    /// `file` with `replacement_file` in the table when
    /// `maybe_replacement == Some(replacement_file)`.
    pub fn remap_fds<F: Fn(&FileHandle) -> Option<FileHandle>>(&self, predicate: F) {
        let inner = self.inner.read();
        let id = inner.id();
        let mut store = inner.store.lock();
        for encoded_entry in store.entries.iter_mut() {
            if let Some(file) = encoded_entry.file() {
                if let Some(replacement_file) = predicate(&file) {
                    encoded_entry.set_file(id, replacement_file);
                }
            }
        }
    }
}

impl ReleasableByRef for FdTable {
    type Context<'a> = ();
    /// Drop the fd table, closing any files opened exclusively by this table.
    fn release<'a>(&self, _context: ()) {
        self.inner.update_sync(Default::default());
    }
}

impl Clone for FdTable {
    fn clone(&self) -> Self {
        FdTable { inner: self.inner.clone() }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::fs::fuchsia::SyslogFile;
    use crate::task::*;
    use crate::testing::*;
    use starnix_sync::Unlocked;

    fn add(
        locked: &mut Locked<Unlocked>,
        current_task: &CurrentTask,
        files: &FdTable,
        file: FileHandle,
    ) -> Result<FdNumber, Errno> {
        files.add_with_flags(locked, current_task, file, FdFlags::empty())
    }

    #[::fuchsia::test]
    async fn test_fd_table_install() {
        spawn_kernel_and_run(|locked, current_task| {
            let files = FdTable::default();
            let file = SyslogFile::new_file(locked, &current_task);

            let fd0 = add(locked, &current_task, &files, file.clone()).unwrap();
            assert_eq!(fd0.raw(), 0);
            let fd1 = add(locked, &current_task, &files, file.clone()).unwrap();
            assert_eq!(fd1.raw(), 1);

            assert!(Arc::ptr_eq(&files.get(fd0).unwrap(), &file));
            assert!(Arc::ptr_eq(&files.get(fd1).unwrap(), &file));
            assert_eq!(files.get(FdNumber::from_raw(fd1.raw() + 1)).map(|_| ()), error!(EBADF));

            files.release(());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_fd_table_fork() {
        spawn_kernel_and_run(|locked, current_task| {
            let files = FdTable::default();
            let file = SyslogFile::new_file(locked, &current_task);

            let fd0 = add(locked, &current_task, &files, file.clone()).unwrap();
            let fd1 = add(locked, &current_task, &files, file).unwrap();
            let fd2 = FdNumber::from_raw(2);

            let forked = files.fork();

            assert_eq!(
                Arc::as_ptr(&files.get(fd0).unwrap()),
                Arc::as_ptr(&forked.get(fd0).unwrap())
            );
            assert_eq!(
                Arc::as_ptr(&files.get(fd1).unwrap()),
                Arc::as_ptr(&forked.get(fd1).unwrap())
            );
            assert!(files.get(fd2).is_err());
            assert!(forked.get(fd2).is_err());

            files.set_fd_flags_allowing_opath(fd0, FdFlags::CLOEXEC).unwrap();
            assert_eq!(FdFlags::CLOEXEC, files.get_fd_flags_allowing_opath(fd0).unwrap());
            assert_ne!(FdFlags::CLOEXEC, forked.get_fd_flags_allowing_opath(fd0).unwrap());

            forked.release(());
            files.release(());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_fd_table_exec() {
        spawn_kernel_and_run(|locked, current_task| {
            let files = FdTable::default();
            let file = SyslogFile::new_file(locked, &current_task);

            let fd0 = add(locked, &current_task, &files, file.clone()).unwrap();
            let fd1 = add(locked, &current_task, &files, file).unwrap();

            files.set_fd_flags_allowing_opath(fd0, FdFlags::CLOEXEC).unwrap();

            assert!(files.get(fd0).is_ok());
            assert!(files.get(fd1).is_ok());

            files.exec();

            assert!(files.get(fd0).is_err());
            assert!(files.get(fd1).is_ok());

            files.release(());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_fd_table_pack_values() {
        spawn_kernel_and_run(|locked, current_task| {
            let files = FdTable::default();
            let file = SyslogFile::new_file(locked, &current_task);

            // Add two FDs.
            let fd0 = add(locked, &current_task, &files, file.clone()).unwrap();
            let fd1 = add(locked, &current_task, &files, file.clone()).unwrap();
            assert_eq!(fd0.raw(), 0);
            assert_eq!(fd1.raw(), 1);

            // Close FD 0
            assert!(files.close(fd0).is_ok());
            assert!(files.close(fd0).is_err());
            // Now it's gone.
            assert!(files.get(fd0).is_err());

            // The next FD we insert fills in the hole we created.
            let another_fd = add(locked, &current_task, &files, file).unwrap();
            assert_eq!(another_fd.raw(), 0);

            files.release(());
        })
        .await;
    }
}
