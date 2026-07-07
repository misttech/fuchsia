// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::security;
use crate::task::{CurrentTask, CurrentTaskAndLocked, register_delayed_release};
use crate::vfs::{FdNumber, FileHandle, FileReleaser};
use bitflags::bitflags;
use fuchsia_rcu::subtle::{RcuPtrRef, rcu_ptr_to_arc};
use fuchsia_rcu::{RcuReadScope, rcu_drop};
use fuchsia_rcu_collections::rcu_array::RcuArray;
use linux_uapi::{FD_CLOEXEC, FIOCLEX, FIONCLEX};
use starnix_sync::{
    FdTableShareCountLock, FileOpsCore, LockBefore, LockDepGuard, LockDepMutex, LockEqualOrBefore,
    Locked, ThreadGroupLimits, Unlocked,
};
use starnix_syscalls::SyscallResult;
use starnix_types::ownership::Releasable;
use starnix_uapi::errors::Errno;
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::resource_limits::Resource;
use starnix_uapi::{errno, error};
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
            // Concurrent readers expect the `FileHandle` to be retained for the entire RCU grace
            // period. `FlushedFile` delayed release may be processed before the grace period
            // expires. We must defer a reference to RCU to ensure delayed release does not drop the
            // last reference and free the file before RCU readers are done with it.
            register_delayed_release(FlushedFile(file.clone(), id));
            rcu_drop(file)
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

    /// Reads the entry, returning a guard that maintains a consistent view of it.
    fn read<'a>(&self, scope: &'a RcuReadScope) -> Option<FdTableEntryGuard<'a>> {
        let value = self.value.load(Ordering::Acquire);
        if value == 0 {
            return None;
        }
        let ptr = Self::decode_ptr(value);
        let flags = Self::decode_flags(value);
        // SAFETY: The pointer is valid because it was encoded in `self.value`.
        let file = unsafe { RcuPtrRef::new(scope, ptr) };
        Some(FdTableEntryGuard { file, flags })
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
        if let Some(guard) = self.read(&RcuReadScope::new()) {
            Self::new(guard.to_entry())
        } else {
            Self::default()
        }
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

/// A guard for reading an `FdTableEntry`.
///
/// This provides memory-safe access to decoded `FdTableEntry` data, which is guarded by RCU.
struct FdTableEntryGuard<'a> {
    /// The pointer to the file handle.
    file: RcuPtrRef<'a, FileReleaser>,

    /// The flags associated with the file handle.
    flags: FdFlags,
}

impl<'a> FdTableEntryGuard<'a> {
    fn flags(&self) -> FdFlags {
        self.flags
    }

    /// Acquire a strong reference to the file handle.
    fn to_handle(&self) -> FileHandle {
        // SAFETY: We can pass `self.file` to `rcu_ptr_to_arc` because it was obtained from
        // `Arc::into_raw` via `EncodedEntry::encode` and `EncodedEntry::decode_ptr`.
        unsafe { rcu_ptr_to_arc(self.file) }
    }

    /// Upgrade this guard to a full `FdTableEntry` independent of the guard lifetime.
    fn to_entry(&self) -> FdTableEntry {
        FdTableEntry { file: self.to_handle(), flags: self.flags }
    }
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
    fn get_file(&self, scope: &RcuReadScope, fd: FdNumber) -> Option<FileHandle> {
        self.slice
            .get(fd.raw() as usize)
            .and_then(|entry| entry.read(scope))
            .map(|guard| guard.to_handle())
    }

    /// Returns the `FdTableEntry` for a given `FdNumber`, if any.
    fn get_entry(&self, scope: &RcuReadScope, fd: FdNumber) -> Option<FdTableEntry> {
        self.slice
            .get(fd.raw() as usize)
            .and_then(|entry| entry.read(scope))
            .map(|guard| guard.to_entry())
    }
}

struct FdTableWriteGuard<'a> {
    store: &'a FdTableInner,
    share_count: LockDepGuard<'a, usize>,
}

impl<'a> FdTableWriteGuard<'a> {
    /// Increases the share count for this `FdTableInner`.
    fn share(&mut self) {
        assert!(*self.share_count > 0, "Cannot share unshared table");
        *self.share_count += 1;
    }

    /// Decreases the share count for this `FdTableInner`. The table is cleared when the count
    /// reaches zero.
    fn unshare(mut self) {
        if *self.share_count > 0 {
            *self.share_count -= 1;
            if *self.share_count == 0 {
                self.clear();
            }
        }
    }

    /// Creates a snapshot of the table with the same files but a separate share count.
    fn fork(&self) -> FdTableInner {
        // THREAD SAFETY: Holding the `share_count` lock through `Self::share_count` ensures
        // coherence between `entries` and `next_fd` because they must only be modified while
        // holding the lock.
        FdTableInner {
            entries: self.store.entries.clone(),
            next_fd: self.store.next_fd.clone(),
            share_count: LockDepMutex::new(1),
        }
    }

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
        self.store.read(scope).get_file(scope, fd)
    }

    /// Inserts a new entry into the `FdTable`.
    ///
    /// Returns whether the `FdTable` previously contained an entry for the given `FdNumber`.
    fn insert_entry(
        &self,
        scope: &RcuReadScope,
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
        let mut view = self.store.read(scope);
        if raw_fd == self.store.next_fd.get().raw() {
            self.store
                .next_fd
                .set(self.calculate_lowest_available_fd(&view, &FdNumber::from_raw(raw_fd + 1)));
        }
        let raw_fd = raw_fd as usize;
        if view.len() <= raw_fd {
            // SAFETY: The write guard excludes concurrent writers.
            unsafe { self.store.entries.ensure_at_least(raw_fd + 1) };
            view = self.store.read(scope);
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
            if let Some(guard) = encoded_entry.read(scope) {
                let mut modified_flags = guard.flags();
                if !predicate(fd, &mut modified_flags) {
                    encoded_entry.clear(id);
                } else if modified_flags != guard.flags() {
                    encoded_entry.set_flags(modified_flags);
                }
            }
        }
        self.store.next_fd.set(self.calculate_lowest_available_fd(&view, &FdNumber::from_raw(0)));
    }

    /// Retain none of the entries in the table.
    fn clear(&self) {
        self.retain(&RcuReadScope::new(), |_, _| false);
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
            if let Some(guard) = encoded_entry.read(scope) {
                let file = guard.to_handle();
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

/// The inner state of a file descriptor table which is shared between tasks.
///
/// # Thread Safety
///
/// The table supports concurrent, lock-free reads via RCU. Writers serialize on the share count
/// mutex independently from readers.
#[derive(Debug)]
struct FdTableInner {
    /// The entries of the `FdTable`.
    ///
    /// # Thread Safety
    ///
    /// Must only be modified while holding the `share_count` lock.
    entries: RcuArray<EncodedEntry>,

    /// The next available `FdNumber`.
    ///
    /// # Thread Safety
    ///
    /// Must only be modified while holding the `share_count` lock.
    next_fd: AtomicFdNumber,

    /// The number of shared references to this table, and the mutex that serializes writers.
    ///
    /// If the value is 0, the table is read-only and empty.
    share_count: LockDepMutex<usize, FdTableShareCountLock>,
}

impl Default for FdTableInner {
    fn default() -> Self {
        Self {
            entries: Default::default(),
            next_fd: AtomicFdNumber::default(),
            share_count: LockDepMutex::new(1),
        }
    }
}

impl Clone for FdTableInner {
    fn clone(&self) -> Self {
        // THREAD SAFETY: Holding the `share_count` lock ensures coherence between `entries` and
        // `next_fd` because they must only be modified while holding the lock.
        let _guard = self.share_count.lock();
        Self {
            entries: self.entries.clone(),
            next_fd: self.next_fd.clone(),
            share_count: LockDepMutex::new(1),
        }
    }
}

impl Drop for FdTableInner {
    fn drop(&mut self) {
        let scope = RcuReadScope::new();
        let view = self.read(&scope);
        for entry in view.slice.iter() {
            assert!(entry.is_none());
        }
    }
}

impl FdTableInner {
    /// Returns the `FdTableId` of the `FdTableInner`.
    fn id(&self) -> FdTableId {
        FdTableId::new(self as *const Self)
    }

    /// Returns a `FdTableView` that provides read-only access to the state of the `FdTableInner`.
    fn read<'a>(&self, scope: &'a RcuReadScope) -> FdTableView<'a> {
        let slice = self.entries.as_slice(scope);
        FdTableView { slice }
    }

    /// Returns a `FdTableWriteGuard` that provides exclusive access to the state of the
    /// `FdTableInner`.
    ///
    /// # Errors
    ///
    /// Returns [`Err(ESRCH)`] if the table has no active sharers, indicating it is in the process
    /// of being destroyed.
    fn write(&self) -> Result<FdTableWriteGuard<'_>, Errno> {
        let share_count = self.share_count.lock();
        if *share_count == 0 {
            return error!(ESRCH);
        }
        Ok(FdTableWriteGuard { store: self, share_count })
    }
}

/// A wrapper around `FdTable` that manages the table's logical share count.
///
/// This type represents the primary reference to the file descriptor table held by a task. Cloning
/// and dropping `SharedFdTable` increment and decrement the share count of the `FdTable`,
/// respectively. When the last `SharedFdTable` for the table is dropped, the table is cleared.
#[derive(Debug, Default)]
pub struct SharedFdTable {
    pub table: FdTable,
}

impl Clone for SharedFdTable {
    fn clone(&self) -> Self {
        self.table.inner.write().expect("FdTable must be writable").share();
        Self { table: self.table.clone() }
    }
}

impl std::ops::Deref for SharedFdTable {
    type Target = FdTable;
    fn deref(&self) -> &Self::Target {
        &self.table
    }
}

impl Drop for SharedFdTable {
    fn drop(&mut self) {
        if let Ok(guard) = self.table.inner.write() {
            guard.unshare();
        }
    }
}

impl SharedFdTable {
    pub fn new(table: FdTable) -> Self {
        Self { table }
    }

    /// Replaces the wrapped table with a fork that has an independent share count.
    pub fn unshare(&mut self) {
        if let Ok(mut guard) = self.table.inner.clone().write() {
            if *guard.share_count > 1 {
                let inner = Arc::new(guard.fork());
                *guard.share_count -= 1;
                self.table = FdTable { inner };
            }
        }
    }
}

/// An `FdTable` is a table of file descriptors.
#[derive(Debug, Clone, Default)]
pub struct FdTable {
    /// The state of the `FdTable` that is shared between tasks.
    inner: Arc<FdTableInner>,
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
        self.inner.id()
    }

    /// Returns new unshared `FdTable` that is a snapshot of the state of the `FdTable`.
    pub fn fork(&self) -> FdTable {
        let forked = (*self.inner).clone();
        FdTable { inner: Arc::new(forked) }
    }

    /// Trims close-on-exec file descriptors from the table.
    pub fn exec(&self, locked: &mut Locked<Unlocked>, current_task: &CurrentTask) {
        self.retain(locked, current_task, |_fd, flags| !flags.contains(FdFlags::CLOEXEC));
    }

    /// Inserts a file descriptor into the table.
    pub fn insert<L>(
        &self,
        locked: &mut Locked<L>,
        current_task: &CurrentTask,
        fd: FdNumber,
        file: FileHandle,
    ) -> Result<(), Errno>
    where
        L: LockBefore<ThreadGroupLimits>,
    {
        let flags = FdFlags::empty();
        let rlimit = current_task.thread_group().get_rlimit(locked, Resource::NOFILE);
        let guard = self.inner.write()?;
        guard.insert_entry(&RcuReadScope::new(), fd, rlimit, FdTableEntry { file, flags })?;
        Ok(())
    }

    /// Adds a file descriptor to the table.
    ///
    /// The file descriptor will be assigned the next available number.
    ///
    /// Returns the assigned file descriptor number.
    ///
    /// This function is the most common way to add a file descriptor to the table.
    pub fn add<L>(
        &self,
        locked: &mut Locked<L>,
        current_task: &CurrentTask,
        file: FileHandle,
        flags: FdFlags,
    ) -> Result<FdNumber, Errno>
    where
        L: LockEqualOrBefore<FileOpsCore>,
    {
        let locked = locked.cast_locked::<FileOpsCore>();
        let rlimit = current_task.thread_group().get_rlimit(locked, Resource::NOFILE);
        let guard = self.inner.write()?;
        let fd = guard.next_fd();
        guard.insert_entry(&RcuReadScope::new(), fd, rlimit, FdTableEntry { file, flags })?;
        Ok(fd)
    }

    /// Duplicates a file descriptor.
    ///
    /// If `target` is `TargetFdNumber::Minimum`, a new `FdNumber` is allocated. Returns the new
    /// `FdNumber`.
    pub fn duplicate<L>(
        &self,
        locked: &mut Locked<L>,
        current_task: &CurrentTask,
        oldfd: FdNumber,
        target: TargetFdNumber,
        flags: FdFlags,
    ) -> Result<FdNumber, Errno>
    where
        L: LockBefore<ThreadGroupLimits>,
    {
        let rlimit = current_task.thread_group().get_rlimit(locked, Resource::NOFILE);
        let guard = self.inner.write()?;
        let scope = RcuReadScope::new();
        let file = guard.get_file(&scope, oldfd).ok_or_else(|| errno!(EBADF))?;

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
                guard.remove_entry(&scope, &fd);
                fd
            }
            TargetFdNumber::Minimum(fd) => guard.get_lowest_available_fd(&scope, fd),
            TargetFdNumber::Default => guard.get_lowest_available_fd(&scope, FdNumber::from_raw(0)),
        };
        let existing_entry =
            guard.insert_entry(&scope, fd, rlimit, FdTableEntry { file, flags })?;
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
        let scope = RcuReadScope::new();
        let view = self.inner.read(&scope);
        view.get_entry(&scope, fd)
            .map(|entry| (entry.file, entry.flags))
            .ok_or_else(|| errno!(EBADF))
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
        let guard = self.inner.write()?;
        let scope = RcuReadScope::new();
        if guard.remove_entry(&scope, &fd) { Ok(()) } else { error!(EBADF) }
    }

    /// Returns the flags associated with the given file descriptor.
    ///
    /// Returns the flags even if the file was opened with `O_PATH`.
    pub fn get_fd_flags_allowing_opath(&self, fd: FdNumber) -> Result<FdFlags, Errno> {
        self.get_allowing_opath_with_flags(fd).map(|(_file, flags)| flags)
    }

    /// Updates the flags of the specified FD with the `request`ed change.
    ///
    /// This operation fails if the file descriptor was opened with `O_PATH` or is not valid.
    pub fn ioctl_fd_flags(
        &self,
        current_task: &CurrentTask,
        fd: FdNumber,
        request: u32,
    ) -> Result<(), Errno> {
        let guard = self.inner.write()?;
        let scope = RcuReadScope::new();
        let file = guard.get_file(&scope, fd).ok_or_else(|| errno!(EBADF))?;
        if file.flags().contains(OpenFlags::PATH) {
            return error!(EBADF);
        }
        let flags = match request {
            FIOCLEX => FdFlags::CLOEXEC,
            FIONCLEX => FdFlags::empty(),
            _ => {
                return error!(EINVAL);
            }
        };
        security::check_file_ioctl_access(current_task, &file, request)?;
        guard.set_fd_flags(&scope, fd, flags)
    }

    /// Sets the flags associated with the given file descriptor.
    ///
    /// This operation fails if the file descriptor is not valid.
    pub fn set_fd_flags_allowing_opath(&self, fd: FdNumber, flags: FdFlags) -> Result<(), Errno> {
        let guard = self.inner.write()?;
        guard.set_fd_flags(&RcuReadScope::new(), fd, flags)
    }

    /// Retains only the FDs matching the given `predicate`.
    ///
    /// The predicate is called with the `FdNumber` and a mutable reference to the `FdFlags` for
    /// each entry in the `FdTable`. If the predicate returns `false`, the entry is removed from
    /// the `FdTable`. Otherwise, the `FdFlags` are updated to the value modified by the predicate.
    pub fn retain<L, F>(&self, _locked: &mut Locked<L>, _current_task: &CurrentTask, predicate: F)
    where
        L: LockEqualOrBefore<FileOpsCore>,
        F: Fn(FdNumber, &mut FdFlags) -> bool,
    {
        if let Ok(guard) = self.inner.write() {
            guard.retain(&RcuReadScope::new(), predicate);
        }
    }

    /// Returns a vector of all current file descriptors in the table.
    pub fn get_all_fds(&self) -> Vec<FdNumber> {
        let scope = RcuReadScope::new();
        let view = self.inner.read(&scope);
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
    pub fn remap<L, F: Fn(&FileHandle) -> Option<FileHandle>>(
        &self,
        _locked: &mut Locked<L>,
        _current_task: &CurrentTask,
        predicate: F,
    ) where
        L: LockEqualOrBefore<FileOpsCore>,
    {
        if let Ok(guard) = self.inner.write() {
            guard.remap(&RcuReadScope::new(), predicate);
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::fs::fuchsia::SyslogFile;
    use crate::testing::*;

    fn add(
        locked: &mut Locked<Unlocked>,
        current_task: &CurrentTask,
        files: &FdTable,
        file: FileHandle,
    ) -> Result<FdNumber, Errno> {
        files.add(locked, current_task, file, FdFlags::empty())
    }

    #[::fuchsia::test]
    async fn test_fd_table_install() {
        spawn_kernel_and_run(async |locked, current_task| {
            let files = SharedFdTable::default();
            let file = SyslogFile::new_file(locked, &current_task);

            let fd0 = add(locked, &current_task, &files, file.clone()).unwrap();
            assert_eq!(fd0.raw(), 0);
            let fd1 = add(locked, &current_task, &files, file.clone()).unwrap();
            assert_eq!(fd1.raw(), 1);

            assert!(Arc::ptr_eq(&files.get(fd0).unwrap(), &file));
            assert!(Arc::ptr_eq(&files.get(fd1).unwrap(), &file));
            assert_eq!(files.get(FdNumber::from_raw(fd1.raw() + 1)).map(|_| ()), error!(EBADF));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_fd_table_fork() {
        spawn_kernel_and_run(async |locked, current_task| {
            let files = SharedFdTable::default();
            let file = SyslogFile::new_file(locked, &current_task);

            let fd0 = add(locked, &current_task, &files, file.clone()).unwrap();
            let fd1 = add(locked, &current_task, &files, file).unwrap();
            let fd2 = FdNumber::from_raw(2);

            let forked = SharedFdTable::new(files.fork());

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
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_fd_table_exec() {
        spawn_kernel_and_run(async |locked, current_task| {
            let files = SharedFdTable::default();
            let file = SyslogFile::new_file(locked, &current_task);

            let fd0 = add(locked, &current_task, &files, file.clone()).unwrap();
            let fd1 = add(locked, &current_task, &files, file).unwrap();

            files.set_fd_flags_allowing_opath(fd0, FdFlags::CLOEXEC).unwrap();

            assert!(files.get(fd0).is_ok());
            assert!(files.get(fd1).is_ok());

            files.exec(locked, &current_task);

            assert!(files.get(fd0).is_err());
            assert!(files.get(fd1).is_ok());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_fd_table_pack_values() {
        spawn_kernel_and_run(async |locked, current_task| {
            let files = SharedFdTable::default();
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
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_fd_table_shared_release() {
        spawn_kernel_and_run(async |locked, current_task| {
            let files = SharedFdTable::default();
            let file = SyslogFile::new_file(locked, &current_task);

            let fd = add(locked, &current_task, &files, file).unwrap();
            assert_eq!(files.get_all_fds(), vec![fd]);

            let shared_files = files.clone();
            assert_eq!(shared_files.get_all_fds(), vec![fd]);

            // Release the original files. Since `shared_files` holds a shared reference, the table
            // should not be cleared.
            drop(files);
            assert_eq!(shared_files.get_all_fds(), vec![fd]);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_fd_table_mutate_after_clear() {
        spawn_kernel_and_run(async |locked, current_task| {
            let shared_files = SharedFdTable::default();
            let file = SyslogFile::new_file(locked, &current_task);

            // Clone the underlying FdTable. This does not increment the share_count, but it does
            // increment the Arc reference count of FdTableInner.
            let fd_table_clone = shared_files.table.clone();

            // Drop the SharedFdTable. This decrements share_count to 0, triggering a table clear.
            drop(shared_files);

            // Now attempt to add a file to the cloned FdTable. It should fail with ESRCH.
            let result = fd_table_clone.add(locked, &current_task, file, FdFlags::empty());
            assert_eq!(result.map(|_| ()), error!(ESRCH));

            // When fd_table_clone is dropped, it should not panic because the above add() call
            // failed to insert an entry.
            drop(fd_table_clone);
        })
        .await;
    }
}
