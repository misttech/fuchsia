// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::task::{CurrentTaskAndLocked, Task, register_delayed_release};
use crate::vfs::{FdNumber, FileHandle, FileReleaser};
use bitflags::bitflags;
use fuchsia_inspect_contrib::profile_duration;
use fuchsia_rcu::rcu_arc::RcuArc;
use fuchsia_rcu::rcu_read_scope::RcuReadScope;
use fuchsia_rcu::rcu_write_scope::RcuWriteScope;
use fuchsia_rcu_collections::rcu_array::RcuArray;
use starnix_sync::{LockBefore, Locked, Mutex, MutexGuard, ThreadGroupLimits};
use starnix_syscalls::SyscallResult;
use starnix_types::ownership::{Releasable, ReleasableByRef};
use starnix_uapi::errors::Errno;
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::resource_limits::Resource;
use starnix_uapi::{FD_CLOEXEC, errno, error};
use static_assertions::const_assert;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct FdFlags: u32 {
        /// Whether the file descriptor should be closed when the process execs.
        const CLOEXEC = FD_CLOEXEC;
    }
}

impl std::convert::From<FdFlags> for SyscallResult {
    fn from(value: FdFlags) -> Self {
        value.bits().into()
    }
}

/// An identifier for an `FdTable`.
///
/// Used by flock to drop file locks when a file descriptor is closed.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct FdTableId(usize);

impl FdTableId {
    fn new(id: *const FdTableInner) -> Self {
        Self(id as usize)
    }

    pub fn raw(&self) -> usize {
        self.0
    }
}

/// We store the CLOEXEC bit and the address of the `FileObject` in a single `usize` so that we can
/// operate on an FdTable entry atomically. This mask is used to select the CLOEXEC bit.
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
    value: AtomicUsize,
}

// An assert to ensure that the lowest bit of the `FileHandle` is available to store the CLOEXEC
// bit.
const_assert!(std::mem::align_of::<*const FileReleaser>() >= 1 << FLAGS_MASK);

impl EncodedEntry {
    /// Encodes a `FileHandle` and `FdFlags` into a single `usize`.
    ///
    /// The returned value holds a reference to the `FileObject` and must be released to avoid a
    /// memory leak.
    fn encode(file: FileHandle, flags: FdFlags) -> usize {
        let ptr = Arc::into_raw(file) as usize;
        let flags = (flags.bits() as usize) & FLAGS_MASK;
        ptr | flags
    }

    /// Releases the `FileHandle` for a previously encoded value.
    ///
    /// # Safety
    ///
    /// `value` must have been encoded by `Self::encode`.
    unsafe fn release(id: FdTableId, value: usize) {
        let ptr = Self::decode_ptr(value);
        if !ptr.is_null() {
            // SAFETY: The pointer is valid because it was encoded in `self.value`.
            let file = unsafe { Arc::from_raw(ptr) };
            register_delayed_release(FlushedFile(file, id));
        }
    }

    /// Decodes the `FdFlags` from an encoded `usize`.
    fn decode_flags(value: usize) -> FdFlags {
        FdFlags::from_bits_truncate((value & FLAGS_MASK) as u32)
    }

    /// Decodes the `FileHandle` from an encoded `usize`.
    fn decode_ptr(value: usize) -> *const FileReleaser {
        (value & !FLAGS_MASK) as *const _
    }

    /// Creates a new `EncodedEntry` from a `FdTableEntry`.
    fn new(entry: FdTableEntry) -> Self {
        Self { value: AtomicUsize::new(Self::encode(entry.file, entry.flags)) }
    }

    /// Whether this entry contains a valid `FileHandle`.
    fn is_some(&self) -> bool {
        let value = self.value.load(Ordering::Acquire);
        value != 0
    }

    /// Whether this entry is empty.
    fn is_none(&self) -> bool {
        !self.is_some()
    }

    /// Returns the `FdFlags` for this entry, if any.
    fn flags(&self) -> Option<FdFlags> {
        let value = self.value.load(Ordering::Acquire);
        if value == 0 {
            return None;
        }
        Some(Self::decode_flags(value))
    }

    /// Sets the `FdFlags` for this entry, preserving the `FileHandle`.
    fn set_flags(&self, flags: FdFlags) {
        loop {
            let old_value = self.value.load(Ordering::Relaxed);
            assert!(old_value != 0);
            let new_value = old_value & !FLAGS_MASK | (flags.bits() as usize) & FLAGS_MASK;
            if self
                .value
                .compare_exchange_weak(old_value, new_value, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
        }
    }

    /// Returns the `FileHandle` for this entry, if any.
    fn file(&self) -> Option<FileHandle> {
        self.to_entry().map(|entry| entry.file)
    }

    /// Sets the `FileHandle` for this entry, preserving the `FdFlags`.
    fn set_file(&self, id: FdTableId, file: FileHandle) {
        let ptr = Arc::into_raw(file) as usize;
        loop {
            let old_value = self.value.load(Ordering::Relaxed);
            assert!(old_value != 0);
            let flags = old_value & FLAGS_MASK;
            let new_value = ptr | flags;
            if self
                .value
                .compare_exchange_weak(old_value, new_value, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                // SAFETY: The value was previously encoded by `Self::encode`.
                unsafe { Self::release(id, old_value) };
                return;
            }
        }
    }

    /// Returns the `FileHandle` and `FdFlags` for this entry, if any.
    fn to_entry(&self) -> Option<FdTableEntry> {
        let value = self.value.load(Ordering::Acquire);
        if value == 0 {
            return None;
        }
        let flags = Self::decode_flags(value);
        let ptr = Self::decode_ptr(value);
        // SAFETY: The pointer is valid because it was encoded in `self.value`.
        let file = unsafe {
            Arc::increment_strong_count(ptr);
            Arc::from_raw(ptr)
        };
        Some(FdTableEntry { file, flags })
    }

    /// Sets the `FileHandle` and `FdFlags` for this entry.
    fn set_entry(&self, id: FdTableId, entry: FdTableEntry) -> bool {
        // SAFETY: The value is encoded by `Self::encode`.
        unsafe { self.set(id, Self::encode(entry.file, entry.flags)) }
    }

    /// Makes the entry empty.
    fn clear(&self, id: FdTableId) -> bool {
        // SAFETY: The value is zero.
        unsafe { self.set(id, 0) }
    }

    /// Sets the value of this entry to the given value.
    ///
    /// Most clients should call `set_entry` or `clear` instead.
    ///
    /// # Safety
    ///
    /// The value must be encoded by `Self::encode` or be zero.
    unsafe fn set(&self, id: FdTableId, value: usize) -> bool {
        let old_value = self.value.swap(value, Ordering::AcqRel);
        if old_value != 0 {
            // SAFETY: The value was previously encoded by `Self::encode`.
            unsafe { Self::release(id, old_value) };
            true
        } else {
            false
        }
    }
}

impl Clone for EncodedEntry {
    fn clone(&self) -> Self {
        if let Some(entry) = self.to_entry() { Self::new(entry) } else { Self::default() }
    }
}

impl Drop for EncodedEntry {
    fn drop(&mut self) {
        let value = self.value.load(Ordering::Acquire);
        let ptr = Self::decode_ptr(value);
        if !ptr.is_null() {
            // SAFETY: The pointer is valid because it was encoded in `self.value`.
            let _file = unsafe { Arc::from_raw(ptr) };
        }
    }
}

/// An entry in the `FdTable`.
#[derive(Debug, Clone)]
struct FdTableEntry {
    /// The file handle.
    file: FileHandle,

    /// The flags associated with the file handle.
    flags: FdFlags,
}

/// A `FileHandle` that has been closed and is waiting to be flushed.
struct FlushedFile(FileHandle, FdTableId);

impl Releasable for FlushedFile {
    type Context<'a> = CurrentTaskAndLocked<'a>;
    fn release<'a>(self, context: Self::Context<'a>) {
        let (locked, current_task) = context;
        let FlushedFile(file, id) = self;
        file.flush(locked, current_task, id);
    }
}

/// A read-only view of an `FdTable`.
///
/// When reading an `FdTable`, we use an `FdTableView` to have a coherent view of the table even
/// though the table can be modified by other threads concurrently.
///
/// The actual entries in the slice can still be modified by other threads. However, the view
/// provided by the `FdTableView` is protected by an RCU read lock.
struct FdTableView<'a> {
    /// The entries in the table.
    slice: &'a [EncodedEntry],
}

impl<'a> FdTableView<'a> {
    /// Returns the number of entries in the table.
    fn len(&self) -> usize {
        self.slice.len()
    }

    /// Whether the view contains a given `FdNumber`.
    fn is_some(&self, fd: FdNumber) -> bool {
        self.slice.get(fd.raw() as usize).map_or(false, |entry| entry.is_some())
    }

    /// Whether the view does not contain a given `FdNumber`.
    fn is_none(&self, fd: FdNumber) -> bool {
        !self.is_some(fd)
    }

    /// Returns the `FileHandle` for a given `FdNumber`, if any.
    fn get_file(&self, fd: FdNumber) -> Option<FileHandle> {
        self.slice.get(fd.raw() as usize).and_then(|entry| entry.file())
    }

    /// Returns the `FdTableEntry` for a given `FdNumber`, if any.
    fn get_entry(&self, fd: FdNumber) -> Option<FdTableEntry> {
        self.slice.get(fd.raw() as usize).and_then(|entry| entry.to_entry())
    }
}

struct FdTableWriteGuard<'a> {
    store: &'a FdTableInner,
    _write_guard: MutexGuard<'a, ()>,
}

impl<'a> FdTableWriteGuard<'a> {
    /// The lowest available `FdNumber`.
    fn next_fd(&self) -> FdNumber {
        self.store.next_fd.get()
    }

    /// Recalculates the lowest available FD >= minfd based on the contents of the map.
    fn calculate_lowest_available_fd(&self, view: &FdTableView<'_>, minfd: &FdNumber) -> FdNumber {
        let mut fd: FdNumber = *minfd;
        while view.is_some(fd) {
            fd = FdNumber::from_raw(fd.raw() + 1);
        }
        fd
    }

    // Returns the (possibly memoized) lowest available FD >= minfd in this map.
    fn get_lowest_available_fd(&self, scope: &RcuReadScope, minfd: FdNumber) -> FdNumber {
        if minfd > self.store.next_fd.get() {
            let view = self.store.read(scope);
            return self.calculate_lowest_available_fd(&view, &minfd);
        }
        self.store.next_fd.get()
    }

    /// Returns the `FileHandle` for a given `FdNumber`, if any.
    fn get_file(&self, scope: &RcuReadScope, fd: FdNumber) -> Option<FileHandle> {
        self.store.read(scope).get_file(fd)
    }

    /// Inserts a new entry into the `FdTable`.
    ///
    /// Returns whether the `FdTable` previously contained an entry for the given `FdNumber`.
    fn insert_entry(
        &self,
        write_scope: &RcuWriteScope,
        read_scope: &RcuReadScope,
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
        let mut view = self.store.read(read_scope);
        if raw_fd == self.store.next_fd.get().raw() {
            self.store
                .next_fd
                .set(self.calculate_lowest_available_fd(&view, &FdNumber::from_raw(raw_fd + 1)));
        }
        let raw_fd = raw_fd as usize;
        if view.len() <= raw_fd {
            // SAFETY: The write guard excludes concurrent writers.
            unsafe { self.store.entries.ensure_at_least(write_scope, raw_fd + 1) };
            view = self.store.read(read_scope);
        }
        let id = self.store.id();
        Ok(view.slice[raw_fd].set_entry(id, entry))
    }

    /// Removes an entry from the `FdTable`.
    ///
    /// Returns whether the `FdTable` previously contained an entry for the given `FdNumber`.
    fn remove_entry(&self, scope: &RcuReadScope, fd: &FdNumber) -> bool {
        let raw_fd = fd.raw() as usize;
        let view = self.store.read(scope);
        if raw_fd >= view.len() {
            return false;
        }
        let id = self.store.id();
        let removed = view.slice[raw_fd].clear(id);
        if removed && raw_fd < self.store.next_fd.get().raw() as usize {
            self.store.next_fd.set(*fd);
        }
        removed
    }

    /// Sets the flags for a given `FdNumber`.
    ///
    /// Returns `Errno` if the `FdTable` does not contain an entry for the given `FdNumber`.
    fn set_fd_flags(
        &self,
        scope: &RcuReadScope,
        fd: FdNumber,
        flags: FdFlags,
    ) -> Result<(), Errno> {
        let view = self.store.read(scope);
        if view.is_none(fd) {
            return error!(EBADF);
        }
        let raw_fd = fd.raw() as usize;
        view.slice[raw_fd].set_flags(flags);
        Ok(())
    }

    /// Retains only the entries for which the given predicate returns `true`.
    ///
    /// The predicate is called with the `FdNumber` and a mutable reference to the `FdFlags` for
    /// each entry in the `FdTable`. If the predicate returns `false`, the entry is removed from
    /// the `FdTable`. Otherwise, the `FdFlags` are updated to the value modified by the predicate.
    fn retain<F>(&self, scope: &RcuReadScope, mut predicate: F)
    where
        F: FnMut(FdNumber, &mut FdFlags) -> bool,
    {
        let id = self.store.id();
        let view = self.store.read(scope);
        for (index, encoded_entry) in view.slice.iter().enumerate() {
            let fd = FdNumber::from_raw(index as i32);
            if let Some(flags) = encoded_entry.flags() {
                let mut modified_flags = flags;
                if !predicate(fd, &mut modified_flags) {
                    encoded_entry.clear(id);
                } else if modified_flags != flags {
                    encoded_entry.set_flags(modified_flags);
                }
            }
        }
        self.store.next_fd.set(self.calculate_lowest_available_fd(&view, &FdNumber::from_raw(0)));
    }

    /// Replaces the `FileHandle` for each entry in the `FdTable` with the result of the given
    /// predicate.
    ///
    /// The predicate is called with the `FileHandle` for each entry in the `FdTable`. If the
    /// predicate returns `Some(file)`, the entry is updated with the new `FileHandle`. Otherwise,
    /// the entry is left unchanged.
    fn remap<F>(&self, scope: &RcuReadScope, predicate: F)
    where
        F: Fn(&FileHandle) -> Option<FileHandle>,
    {
        let id = self.store.id();
        let view = self.store.read(scope);
        for encoded_entry in view.slice.iter() {
            if let Some(file) = encoded_entry.file() {
                if let Some(replacement_file) = predicate(&file) {
                    encoded_entry.set_file(id, replacement_file);
                }
            }
        }
    }
}

/// An `FdNumber` that can be atomically updated.
///
/// Used for the `next_fd` field of `FdTableInner`, which is only modified by the `FdTable` when
/// holding the `writer_queue` lock.
#[derive(Debug, Default)]
struct AtomicFdNumber {
    /// The raw value of the `FdNumber`.
    value: AtomicI32,
}

impl AtomicFdNumber {
    /// Returns the current value of the `FdNumber`.
    ///
    /// Uses `Ordering::Relaxed`.
    fn get(&self) -> FdNumber {
        FdNumber::from_raw(self.value.load(Ordering::Relaxed))
    }

    /// Sets the value of the `FdNumber`.
    ///
    /// Uses `Ordering::Relaxed`.
    fn set(&self, value: FdNumber) {
        self.value.store(value.raw(), Ordering::Relaxed);
    }
}

impl Clone for AtomicFdNumber {
    fn clone(&self) -> Self {
        Self { value: AtomicI32::new(self.value.load(Ordering::Relaxed)) }
    }
}

/// The state of an `FdTable` that is shared between tasks.
///
/// The `writer_queue` is used to serialize concurrent writers to the `FdTable`, and to prevent
/// writers from being blocked by readers.
#[derive(Debug)]
struct FdTableInner {
    /// The entries of the `FdTable`.
    entries: RcuArray<EncodedEntry>,

    /// The next available `FdNumber`.
    next_fd: AtomicFdNumber,

    /// A mutex used to serialize concurrent writers to the `FdTable`, and to prevent writers from
    /// being blocked by readers.
    writer_queue: Mutex<()>,
}

impl Default for FdTableInner {
    fn default() -> Self {
        FdTableInner {
            entries: Default::default(),
            next_fd: AtomicFdNumber::default(),
            writer_queue: Mutex::new(()),
        }
    }
}

impl Clone for FdTableInner {
    fn clone(&self) -> Self {
        let _guard = self.writer_queue.lock();
        Self {
            entries: self.entries.clone(),
            next_fd: self.next_fd.clone(),
            writer_queue: Mutex::new(()),
        }
    }
}

impl Drop for FdTableInner {
    fn drop(&mut self) {
        let id = self.id();
        let scope = RcuReadScope::new();
        let view = self.read(&scope);
        for entry in view.slice.iter() {
            entry.clear(id);
        }
    }
}

impl FdTableInner {
    /// Returns the `FdTableId` of the `FdTableInner`.
    fn id(&self) -> FdTableId {
        FdTableId::new(self as *const Self)
    }

    /// Returns an `Arc<FdTableInner>` that is a snapshot of the state of the `FdTableInner`.
    fn unshare(&self) -> Arc<Self> {
        Arc::new(self.clone())
    }

    /// Returns a `FdTableView` that provides read-only access to the state of the `FdTableInner`.
    fn read<'a>(&self, scope: &'a RcuReadScope) -> FdTableView<'a> {
        let slice = self.entries.as_slice(scope);
        FdTableView { slice }
    }

    /// Returns a `FdTableWriteGuard` that provides exclusive access to the state of the
    /// `FdTableInner`.
    fn write(&self) -> FdTableWriteGuard<'_> {
        FdTableWriteGuard { store: self, _write_guard: self.writer_queue.lock() }
    }
}

/// An `FdTable` is a table of file descriptors.
#[derive(Debug, Default)]
pub struct FdTable {
    /// The state of the `FdTable` that is shared between tasks.
    inner: RcuArc<FdTableInner>,
}

/// The target `FdNumber` for a duplicated file descriptor.
pub enum TargetFdNumber {
    /// The duplicated `FdNumber` will be the smallest available `FdNumber`.
    Default,

    /// The duplicated `FdNumber` should be this specific `FdNumber`.
    Specific(FdNumber),

    /// The duplicated `FdNumber` should be greater than this `FdNumber`.
    Minimum(FdNumber),
}

impl FdTable {
    /// Returns the `FdTableId` of the `FdTable`.
    pub fn id(&self) -> FdTableId {
        self.inner.read().id()
    }

    /// Returns new unshared `FdTable` that is a snapshot of the state of the `FdTable`.
    pub fn fork(&self) -> FdTable {
        let unshared = self.inner.read().unshare();
        FdTable { inner: RcuArc::new(unshared) }
    }

    /// Ensures that this `FdTable` is not shared by any other `FdTable` instances.
    pub fn unshare(&self) {
        let unshared = self.inner.read().unshare();
        self.inner.update_sync(unshared);
    }

    /// Trims close-on-exec file descriptors from the table.
    pub fn exec(&self) {
        self.retain(|_fd, flags| !flags.contains(FdFlags::CLOEXEC));
    }

    /// Inserts a file descriptor into the table.
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

    /// Inserts a file descriptor into the table with the specified flags.
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
        let scope = RcuWriteScope::new();
        let rlimit = task.thread_group().get_rlimit(locked, Resource::NOFILE);
        let inner = self.inner.read();
        let guard = inner.write();
        guard.insert_entry(&scope, &inner.scope, fd, rlimit, FdTableEntry { file, flags })?;
        Ok(())
    }

    /// Adds a file descriptor to the table.
    ///
    /// The file descriptor will be assigned the next available number.
    ///
    /// Returns the assigned file descriptor number.
    ///
    /// This function is the most common way to add a file descriptor to the table.
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
        let scope = RcuWriteScope::new();
        let rlimit = task.thread_group().get_rlimit(locked, Resource::NOFILE);
        let inner = self.inner.read();
        let guard = inner.write();
        let fd = guard.next_fd();
        guard.insert_entry(&scope, &inner.scope, fd, rlimit, FdTableEntry { file, flags })?;
        Ok(fd)
    }

    /// Duplicates a file descriptor.
    ///
    /// If `target` is `TargetFdNumber::Minimum`, a new `FdNumber` is allocated. Returns the new
    /// `FdNumber`.
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
        let scope = RcuWriteScope::new();
        let rlimit = task.thread_group().get_rlimit(locked, Resource::NOFILE);
        let inner = self.inner.read();
        let guard = inner.write();
        let file = guard.get_file(&inner.scope, oldfd).ok_or_else(|| errno!(EBADF))?;

        let fd = match target {
            TargetFdNumber::Specific(fd) => {
                // We need to check the rlimit before we remove the entry from state
                // because we cannot error out after removing the entry.
                if fd.raw() as u64 >= rlimit {
                    // ltp_dup201 shows that we're supposed to return EBADF in this
                    // situation, instead of EMFILE, which is what we normally return
                    // when we're past the rlimit.
                    return error!(EBADF);
                }
                guard.remove_entry(&inner.scope, &fd);
                fd
            }
            TargetFdNumber::Minimum(fd) => guard.get_lowest_available_fd(&inner.scope, fd),
            TargetFdNumber::Default => {
                guard.get_lowest_available_fd(&inner.scope, FdNumber::from_raw(0))
            }
        };
        let existing_entry =
            guard.insert_entry(&scope, &inner.scope, fd, rlimit, FdTableEntry { file, flags })?;
        assert!(!existing_entry);
        Ok(fd)
    }

    /// Returns the file handle associated with the given file descriptor.
    ///
    /// Returns the file handle even if the file was opened with `O_PATH`.
    ///
    /// This operation is uncommon. Most clients should use `get` instead, which fails if the file
    /// was opened with `O_PATH`.
    pub fn get_allowing_opath(&self, fd: FdNumber) -> Result<FileHandle, Errno> {
        self.get_allowing_opath_with_flags(fd).map(|(file, _flags)| file)
    }

    /// Returns the file handle and flags associated with the given file descriptor.
    ///
    /// Returns the file handle even if the file was opened with `O_PATH`.
    ///
    /// This operation is uncommon. Most clients should use `get` instead, which fails if the file
    /// was opened with `O_PATH`.
    pub fn get_allowing_opath_with_flags(
        &self,
        fd: FdNumber,
    ) -> Result<(FileHandle, FdFlags), Errno> {
        profile_duration!("GetFdWithFlags");
        let inner = self.inner.read();
        let view = inner.read(&inner.scope);
        view.get_entry(fd).map(|entry| (entry.file, entry.flags)).ok_or_else(|| errno!(EBADF))
    }

    /// Returns the file handle associated with the given file descriptor.
    ///
    /// This operation fails if the file was opened with `O_PATH`.
    pub fn get(&self, fd: FdNumber) -> Result<FileHandle, Errno> {
        let file = self.get_allowing_opath(fd)?;
        if file.flags().contains(OpenFlags::PATH) {
            return error!(EBADF);
        }
        Ok(file)
    }

    /// Closes the file descriptor associated with the given file descriptor.
    ///
    /// This operation fails if the file descriptor is not valid.
    pub fn close(&self, fd: FdNumber) -> Result<(), Errno> {
        profile_duration!("CloseFile");
        let inner = self.inner.read();
        let guard = inner.write();
        if guard.remove_entry(&inner.scope, &fd) { Ok(()) } else { error!(EBADF) }
    }

    /// Returns the flags associated with the given file descriptor.
    ///
    /// Returns the flags even if the file was opened with `O_PATH`.
    pub fn get_fd_flags_allowing_opath(&self, fd: FdNumber) -> Result<FdFlags, Errno> {
        self.get_allowing_opath_with_flags(fd).map(|(_file, flags)| flags)
    }

    /// Sets the flags associated with the given file descriptor.
    ///
    /// This operation fails if the file descriptor was opened with `O_PATH` or is not valid.
    pub fn set_fd_flags(&self, fd: FdNumber, flags: FdFlags) -> Result<(), Errno> {
        profile_duration!("SetFdFlags");
        let inner = self.inner.read();
        let guard = inner.write();
        let file = guard.get_file(&inner.scope, fd).ok_or_else(|| errno!(EBADF))?;
        if file.flags().contains(OpenFlags::PATH) {
            return error!(EBADF);
        }
        guard.set_fd_flags(&inner.scope, fd, flags)
    }

    /// Sets the flags associated with the given file descriptor.
    ///
    /// This operation fails if the file descriptor is not valid.
    pub fn set_fd_flags_allowing_opath(&self, fd: FdNumber, flags: FdFlags) -> Result<(), Errno> {
        profile_duration!("SetFdFlagsAllowingOpath");
        let inner = self.inner.read();
        let guard = inner.write();
        guard.set_fd_flags(&inner.scope, fd, flags)
    }

    /// Retains only the FDs matching the given `predicate`.
    ///
    /// The predicate is called with the `FdNumber` and a mutable reference to the `FdFlags` for
    /// each entry in the `FdTable`. If the predicate returns `false`, the entry is removed from
    /// the `FdTable`. Otherwise, the `FdFlags` are updated to the value modified by the predicate.
    pub fn retain<F>(&self, predicate: F)
    where
        F: Fn(FdNumber, &mut FdFlags) -> bool,
    {
        profile_duration!("RetainFds");
        let inner = self.inner.read();
        let guard = inner.write();
        guard.retain(&inner.scope, predicate);
    }

    /// Returns a vector of all current file descriptors in the table.
    pub fn get_all_fds(&self) -> Vec<FdNumber> {
        let inner = self.inner.read();
        let view = inner.read(&inner.scope);
        view.slice
            .iter()
            .enumerate()
            .filter_map(|(index, encoded_entry)| {
                if encoded_entry.is_none() { None } else { Some(FdNumber::from_raw(index as i32)) }
            })
            .collect()
    }

    /// Executes `predicate(file) => maybe_replacement` on every non-empty table entry.
    ///
    /// Replaces `file` with `replacement_file` in the table when
    /// `maybe_replacement == Some(replacement_file)`.
    pub fn remap<F: Fn(&FileHandle) -> Option<FileHandle>>(&self, predicate: F) {
        profile_duration!("RemapFds");
        let inner = self.inner.read();
        let guard = inner.write();
        guard.remap(&inner.scope, predicate);
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
