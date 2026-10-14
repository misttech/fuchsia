// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::array::Array;
use crate::doubly_linked_list::{self, DoublyLinkedList, DoublyLinkedListContainable};
use crate::ptr_traits::{ManagedPtr, PtrTraits};
use crate::singly_linked_list::{self, SinglyLinkedList, SinglyLinkedListContainable};
use crate::size_tracker::SizeTracker;
use core::borrow::Borrow;
use core::marker::PhantomData;
use core::pin::Pin;
use core::ptr::NonNull;
use kalloc::AllocError;

/// Trait that types must implement to expose the key used to store them in a `HashTable`.
///
/// This is the Rust analog of the `GetKey` portion of C++'s `fbl::DefaultKeyedObjectTraits`.
/// Keys are compared for equality using `K: Eq` (the analog of `KeyTraits::EqualTo`).
///
/// The key of an object must not change while the object is contained in a `HashTable`.
pub trait HashTableKeyable<K: ?Sized> {
    /// The type of key yielded by `get_key`, borrowing `K`.
    ///
    /// Implementations may return an owned/by-value key (e.g. `u64`) or a borrowed/by-reference
    /// key (e.g. `&'a KeyStruct`).
    type Key<'a>: Borrow<K>
    where
        Self: 'a;

    /// Returns the key of this object.
    fn get_key(&self) -> Self::Key<'_>;
}

/// Traits defining the hash function used by a `HashTable` to select a bucket for a key.
///
/// `get_hash` may return any `usize`; the hash table reduces it modulo its bucket count to
/// determine the index of the bucket the key belongs in.
pub trait HashTraits<K: ?Sized> {
    /// Returns the hash value of `key`.
    fn get_hash(key: &K) -> usize;
}

/// Key types which can be hashed by `DefaultHashTraits`.
///
/// This is the Rust analog of the static `GetHash` method which C++'s `fbl::DefaultHashTraits`
/// requires. Implementations should return what amounts to a "large number"; the hash table
/// takes it modulo its bucket count to determine the bucket index.
///
/// This trait is implemented for all primitive integer types, returning the value of the integer
/// itself (converted with `as usize`).
pub trait DefaultHashable {
    /// Returns the (unreduced) hash value of this key.
    fn hash_key(&self) -> usize;
}

macro_rules! impl_default_hashable_for_int {
    ($($t:ty),*) => {
        $(
            impl DefaultHashable for $t {
                #[inline]
                fn hash_key(&self) -> usize {
                    *self as usize
                }
            }
        )*
    };
}

impl_default_hashable_for_int!(u8, u16, u32, u64, usize, i8, i16, i32, i64, isize);

/// The default `HashTraits` implementation for a `HashTable`.
///
/// The hash value of a key is the value returned by its `DefaultHashable::hash_key`.
#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultHashTraits;

impl<K: DefaultHashable + ?Sized> HashTraits<K> for DefaultHashTraits {
    #[inline]
    fn get_hash(key: &K) -> usize {
        key.hash_key()
    }
}

mod sealed {
    use super::*;

    pub trait Sealed {}

    impl<P, Tag, S> Sealed for SinglyLinkedList<P, Tag, S>
    where
        P: PtrTraits,
        P::Target: SinglyLinkedListContainable<P::Target, Tag>,
        S: SizeTracker,
    {
    }

    impl<P, Tag, S> Sealed for DoublyLinkedList<P, Tag, S>
    where
        P: PtrTraits,
        P::Target: DoublyLinkedListContainable<P::Target, Tag>,
        S: SizeTracker,
    {
    }
}

/// A list type which can be used as the bucket type of a `HashTable` of pointers of type `P`.
///
/// This trait is sealed. It is implemented for:
///
/// * `SinglyLinkedList<P, Tag>`: Erasing a specific element (e.g. with
///   `HashTable::erase_element`) searches its bucket, so it runs in time proportional to the
///   length of the bucket.
/// * `DoublyLinkedList<P, Tag>`: Erasing a specific element runs in constant time. The buckets
///   must not move once initialized; the hash table keeps them pinned in its bucket storage.
pub trait HashTableBucket<P: PtrTraits>: sealed::Sealed + Sized {
    /// An iterator over the elements of a bucket.
    type Iter<'a>: core::iter::Iterator<Item = &'a P::Target>
    where
        Self: 'a,
        P: 'a;

    /// Allocates the storage for `count` empty buckets.
    fn try_new_buckets(count: usize) -> Result<Pin<Array<Self>>, AllocError>;

    /// Returns true if the bucket is empty.
    fn is_empty(&self) -> bool;

    /// Pushes an element to the front of the bucket.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `ptr` is a valid pointer to an object that is not currently in
    /// any container, and that the object outlives the reference from the bucket.
    unsafe fn push_front_raw(self: Pin<&mut Self>, ptr: P);

    /// Finds the first element of the bucket which satisfies the predicate.
    fn find_if<F>(&self, f: F) -> Option<&P::Target>
    where
        F: FnMut(&P::Target) -> bool;

    /// Finds the first element of the bucket which satisfies the predicate, erases it from the
    /// bucket, and returns it.
    fn erase_if<F>(self: Pin<&mut Self>, f: F) -> Option<P>
    where
        F: FnMut(&P::Target) -> bool;

    /// Erases `obj` from the bucket, returning it. Returns `None` if `obj` is not in any container.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `obj` is either not in any container, or is currently in this
    /// bucket.
    unsafe fn erase(self: Pin<&mut Self>, obj: &P::Target) -> Option<P>;

    /// Replaces `obj` with `replacement` in the bucket, returning the replaced element.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `obj` is currently in this bucket, and that `replacement` is a
    /// valid pointer to an object that is not currently in any container and that outlives the
    /// reference from the bucket.
    unsafe fn replace_raw(self: Pin<&mut Self>, obj: &P::Target, replacement: P) -> Option<P>;

    /// Removes all elements from the bucket.
    fn clear(self: Pin<&mut Self>);

    /// Returns an iterator over the elements of the bucket.
    fn iter<'a>(&'a self) -> Self::Iter<'a>
    where
        P: 'a;
}

/// Returns mutable access to a pinned `SinglyLinkedList` bucket.
#[inline]
fn unpin_singly_linked_list<P, Tag, S>(
    list: Pin<&mut SinglyLinkedList<P, Tag, S>>,
) -> &mut SinglyLinkedList<P, Tag, S>
where
    P: PtrTraits,
    P::Target: SinglyLinkedListContainable<P::Target, Tag>,
    S: SizeTracker,
{
    // SAFETY: A `SinglyLinkedList` does not depend on its own address (its elements never point
    // back to the list), so it may be moved freely and does not need to stay pinned.
    unsafe { list.get_unchecked_mut() }
}

impl<P, Tag, S> HashTableBucket<P> for SinglyLinkedList<P, Tag, S>
where
    P: PtrTraits,
    P::Target: SinglyLinkedListContainable<P::Target, Tag>,
    S: SizeTracker,
{
    type Iter<'a>
        = singly_linked_list::Iterator<'a, P, Tag>
    where
        Self: 'a,
        P: 'a;

    #[inline]
    fn try_new_buckets(count: usize) -> Result<Pin<Array<Self>>, AllocError> {
        Ok(Array::<Self>::try_new(count)?.into_pin())
    }

    #[inline]
    fn is_empty(&self) -> bool {
        SinglyLinkedList::is_empty(self)
    }

    #[inline]
    unsafe fn push_front_raw(self: Pin<&mut Self>, ptr: P) {
        // SAFETY: The caller guarantees that `ptr` is valid, not in any container, and outlives
        // its reference from the list.
        unsafe { SinglyLinkedList::push_front_raw(unpin_singly_linked_list(self), ptr) }
    }

    #[inline]
    fn find_if<F>(&self, f: F) -> Option<&P::Target>
    where
        F: FnMut(&P::Target) -> bool,
    {
        SinglyLinkedList::find_if(self, f)
    }

    #[inline]
    fn erase_if<F>(self: Pin<&mut Self>, f: F) -> Option<P>
    where
        F: FnMut(&P::Target) -> bool,
    {
        SinglyLinkedList::erase_if(unpin_singly_linked_list(self), f)
    }

    #[inline]
    unsafe fn erase(self: Pin<&mut Self>, obj: &P::Target) -> Option<P> {
        if !<P::Target as SinglyLinkedListContainable<P::Target, Tag>>::get_node(obj).in_container()
        {
            return None;
        }
        SinglyLinkedList::erase_if(unpin_singly_linked_list(self), |other| {
            core::ptr::eq(other, obj)
        })
    }

    #[inline]
    unsafe fn replace_raw(self: Pin<&mut Self>, obj: &P::Target, replacement: P) -> Option<P> {
        // SAFETY: The caller guarantees that `replacement` is valid, not in any container, and
        // outlives its reference from the list.
        unsafe {
            SinglyLinkedList::replace_if_raw(
                unpin_singly_linked_list(self),
                |other| core::ptr::eq(other, obj),
                replacement,
            )
        }
    }

    #[inline]
    fn clear(self: Pin<&mut Self>) {
        SinglyLinkedList::clear(unpin_singly_linked_list(self))
    }

    #[inline]
    fn iter<'a>(&'a self) -> Self::Iter<'a>
    where
        P: 'a,
    {
        SinglyLinkedList::iter(self)
    }
}

impl<P, Tag, S> HashTableBucket<P> for DoublyLinkedList<P, Tag, S>
where
    P: PtrTraits,
    P::Target: DoublyLinkedListContainable<P::Target, Tag>,
    S: SizeTracker,
{
    type Iter<'a>
        = doubly_linked_list::ForwardIterator<'a, P, Tag>
    where
        Self: 'a,
        P: 'a;

    #[inline]
    fn try_new_buckets(count: usize) -> Result<Pin<Array<Self>>, AllocError> {
        Array::try_new_pin_init(count, Self::new)
    }

    #[inline]
    fn is_empty(&self) -> bool {
        DoublyLinkedList::is_empty(self)
    }

    #[inline]
    unsafe fn push_front_raw(self: Pin<&mut Self>, ptr: P) {
        // SAFETY: The caller guarantees that `ptr` is valid, not in any container, and outlives
        // its reference from the list.
        unsafe { DoublyLinkedList::push_front_raw(self, ptr) }
    }

    #[inline]
    fn find_if<F>(&self, f: F) -> Option<&P::Target>
    where
        F: FnMut(&P::Target) -> bool,
    {
        DoublyLinkedList::find_if(self, f)
    }

    #[inline]
    fn erase_if<F>(self: Pin<&mut Self>, f: F) -> Option<P>
    where
        F: FnMut(&P::Target) -> bool,
    {
        DoublyLinkedList::erase_if(self, f)
    }

    #[inline]
    unsafe fn erase(self: Pin<&mut Self>, obj: &P::Target) -> Option<P> {
        // SAFETY: The caller guarantees that `obj` is either in no container (in which case the
        // list returns `None`) or in this list.
        unsafe { DoublyLinkedList::erase(self, obj) }
    }

    #[inline]
    unsafe fn replace_raw(self: Pin<&mut Self>, obj: &P::Target, replacement: P) -> Option<P> {
        // SAFETY: The caller guarantees that `obj` is in this list, and that `replacement` is
        // valid, not in any container, and outlives its reference from the list.
        unsafe { DoublyLinkedList::replace_raw(self, obj, replacement) }
    }

    #[inline]
    fn clear(self: Pin<&mut Self>) {
        DoublyLinkedList::clear(self)
    }

    #[inline]
    fn iter<'a>(&'a self) -> Self::Iter<'a>
    where
        P: 'a,
    {
        self.forward_iter()
    }
}

/// The result of `HashTable::insert_or_find`.
///
/// `Ok(())` if the element was inserted, or `Err((ptr, cursor))` with the rejected pointer and a
/// cursor positioned at the colliding element otherwise.
pub type HashTableInsertOrFindResult<'a, K, P, B, H> =
    Result<(), (P, HashTableCursorMut<'a, K, P, B, H>)>;

/// An intrusive, unordered, associative hash table container.
///
/// ### Bookkeeping & Memory Storage
///
/// Objects are stored in an array of buckets, each of which is an intrusive linked list (see
/// "Buckets" below). The bookkeeping storage required to link an object into its bucket (a list
/// node) lives on the object itself, so adding or removing members to/from the container never
/// requires allocation. The only allocation performed by the hash table is the one-time
/// allocation of its bucket array, which is fallible.
///
/// The hash table stores pointers to the objects, not the objects themselves, and is parameterized
/// based on the specific pointer wrapper to be stored (`P`). Supported pointer wrappers are:
///
/// * `*mut T`       : Raw unmanaged pointers.
/// * `UniquePtr<T>` : Unique managed pointers.
/// * `RefPtr<T>`    : Shared managed pointers to reference-counted objects.
///
/// ### Buckets
///
/// The type of the buckets is selected by the `B` parameter (see `HashTableBucket`):
///
/// * `SinglyLinkedList<P, Tag>` (the default): Objects need a `SinglyLinkedListNode`, which is a
///   single pointer. Erasing an element by reference searches its bucket.
/// * `DoublyLinkedList<P, Tag>`: Objects need a `DoublyLinkedListNode`, which is two pointers.
///   Erasing an element by reference runs in constant time.
///
/// The `Tag` of the bucket type selects the node used by the hash table, which allows an object
/// to be in a hash table and in other containers at the same time.
///
/// ### Keys & Hashing
///
/// Objects expose their key of type `K` through `HashTableKeyable<K>`, and keys are compared using
/// `K: Eq`. Duplicate keys are not allowed. The bucket an object belongs to is the hash of its
/// key, as computed by the hash traits `H` (see `HashTraits`, which default to
/// `DefaultHashTraits`), modulo the bucket count of the hash table.
///
/// The key of an object must not change while the object is in the hash table.
///
/// ### Bucket Storage & Initialization
///
/// The bucket count of a `HashTable` is chosen at runtime. A hash table may be created with its
/// bucket storage in a single step using `try_new`, or created in an "invalid" (uninitialized)
/// state using the infallible `const fn new` and later initialized exactly once using `try_init`
/// (the analog of C++'s `HashTableOption::DelayedInit` followed by `Init`). An uninitialized hash
/// table is empty and may be cleared, iterated, or dropped, but it must be initialized before any
/// keyed operation (insert, find, erase, ...) is performed on it.
///
/// The bucket storage is heap allocated and its buckets never move, so unlike the other intrusive
/// containers in this crate, a `HashTable` itself does not need to be pinned (even with
/// `DoublyLinkedList` buckets) and may be freely moved.
///
/// Bucket storage with a bucket count fixed at compile time is not currently supported.
///
/// ### Lifecycle Management
///
/// * **Managed Pointers (`UniquePtr`/`RefPtr`)**: The hash table holds ownership references of
///   elements. Clearing the hash table or dropping it automatically releases references, which may
///   destruct the elements if it was their last reference.
///
/// * **Unmanaged Pointers (`*mut T`)**: The hash table performs no lifecycle management. It is up
///   to the caller to ensure elements outlive the hash table and are freed correctly. As a safety
///   check, a hash table of unmanaged pointers will debug-assert if it is dropped with elements
///   still inside.
///
/// ### Complexity
///
/// Insert, find and erase-by-key run in time proportional to the length of the bucket the key
/// hashes to. Direct erase operations (where a reference to the element to be erased is already
/// known, e.g. `erase_element` and `HashTableCursorMut::erase`) run in constant time with
/// `DoublyLinkedList` buckets, and in time proportional to the length of the bucket with
/// `SinglyLinkedList` buckets. `len` and `is_empty` run in constant time.
///
/// ---
///
/// ### Example: Hash table of unique managed pointers
///
/// ```rust
/// use fbl::{
///     DoublyLinkedList, DoublyLinkedListContainable, DoublyLinkedListNode, HashTable,
///     HashTableKeyable, Recyclable, SinglyLinkedListContainable, SinglyLinkedListNode, UniquePtr,
/// };
///
/// #[derive(SinglyLinkedListContainable, DoublyLinkedListContainable, Recyclable)]
/// struct Foo {
///     id: u64,
///     #[sll_node]
///     sll_node: SinglyLinkedListNode<Foo>,
///     #[dll_node]
///     dll_node: DoublyLinkedListNode<Foo>,
/// }
///
/// impl Foo {
///     fn new(id: u64) -> UniquePtr<Foo> {
///         let sll_node = SinglyLinkedListNode::new();
///         let dll_node = DoublyLinkedListNode::new();
///         UniquePtr::try_new(Foo { id, sll_node, dll_node }).unwrap()
///     }
/// }
///
/// impl HashTableKeyable<u64> for Foo {
///     type Key<'a> = u64;
///     fn get_key(&self) -> u64 {
///         self.id
///     }
/// }
///
/// // A hash table with the default `SinglyLinkedList` buckets.
/// let mut table = HashTable::<u64, UniquePtr<Foo>>::try_new(37).unwrap();
/// table.insert(Foo::new(1));
/// table.insert(Foo::new(2));
///
/// assert_eq!(table.len(), 2);
/// assert_eq!(table.find(&1).map(|foo| foo.id), Some(1));
///
/// let foo = table.erase(&2).unwrap();
/// assert_eq!(foo.id, 2);
///
/// // A hash table with `DoublyLinkedList` buckets, which can erase an element by reference in
/// // constant time.
/// type DllBuckets = DoublyLinkedList<UniquePtr<Foo>>;
/// let mut dll_table = HashTable::<u64, UniquePtr<Foo>, DllBuckets>::try_new(37).unwrap();
/// dll_table.insert(foo);
/// let foo: *const Foo = dll_table.find(&2).unwrap();
/// // SAFETY: `foo` is in `dll_table` and its key has not changed.
/// assert!(unsafe { dll_table.erase_element(&*foo) }.is_some());
///
/// // Dropping the tables automatically drops the remaining unique pointers.
/// ```
pub struct HashTable<K, P, B = SinglyLinkedList<P>, H = DefaultHashTraits>
where
    P: PtrTraits,
    P::Target: HashTableKeyable<K>,
    B: HashTableBucket<P>,
    K: Eq,
    H: HashTraits<K>,
{
    /// The number of elements currently in the hash table.
    count: usize,

    /// The bucket storage.
    ///
    /// The buckets are pinned: they live in a heap allocation which is never resized, and they
    /// are only ever accessed in place (see `buckets_mut`). The storage is only ever replaced
    /// wholesale while it holds no buckets.
    ///
    /// An empty array means that the hash table has not been initialized yet.
    buckets: Pin<Array<B>>,

    _phantom: PhantomData<(K, P, H)>,
}

impl<K, P, B, H> HashTable<K, P, B, H>
where
    P: PtrTraits,
    P::Target: HashTableKeyable<K>,
    B: HashTableBucket<P>,
    K: Eq,
    H: HashTraits<K>,
{
    /// Creates a new, uninitialized hash table without any bucket storage.
    ///
    /// The hash table must be initialized with `try_init` before any keyed operation is performed
    /// on it. This is the analog of constructing a C++ `fbl::HashTable` with
    /// `HashTableOption::DelayedInit`.
    #[inline]
    pub const fn new() -> Self {
        Self { count: 0, buckets: Array::new().into_pin(), _phantom: PhantomData }
    }

    /// Creates a new, empty hash table with `bucket_count` buckets.
    ///
    /// Returns an error if the bucket storage could not be allocated.
    ///
    /// # Panics
    ///
    /// Panics if `bucket_count` is zero.
    pub fn try_new(bucket_count: usize) -> Result<Self, AllocError> {
        let mut table = Self::new();
        table.try_init(bucket_count)?;
        Ok(table)
    }

    /// Initializes a hash table created with `new` with `bucket_count` buckets.
    ///
    /// Initialization is not idempotent and must be performed exactly once before use. Returns an
    /// error if the bucket storage could not be allocated, in which case the hash table remains
    /// uninitialized.
    ///
    /// # Panics
    ///
    /// Panics if `bucket_count` is zero, or if the hash table has already been initialized.
    pub fn try_init(&mut self, bucket_count: usize) -> Result<(), AllocError> {
        assert!(!self.is_initialized(), "HashTable may only be initialized once");
        assert!(bucket_count > 0, "HashTables must have at least one bucket");
        self.buckets = B::try_new_buckets(bucket_count)?;
        Ok(())
    }

    /// Returns true if the hash table has been given its bucket storage.
    #[inline]
    pub fn is_initialized(&self) -> bool {
        !self.buckets.is_empty()
    }

    /// Returns the number of buckets in the hash table, or zero if it has not been initialized.
    #[inline]
    pub fn bucket_count(&self) -> usize {
        self.buckets.len()
    }

    /// Returns the number of elements in the hash table.
    #[inline]
    pub fn len(&self) -> usize {
        self.count
    }

    /// Returns true if the hash table contains no elements.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Returns pinned mutable access to the bucket at index `ndx`.
    ///
    /// # Panics
    ///
    /// Panics if `ndx` is out of bounds.
    #[inline]
    fn bucket_mut(&mut self, ndx: usize) -> Pin<&mut B> {
        // SAFETY: The buckets are pinned in `self.buckets`, and projecting to one of them in place
        // does not move it.
        unsafe { self.buckets.as_mut().map_unchecked_mut(|buckets| &mut buckets[ndx]) }
    }

    /// Returns the index of the bucket `key` belongs in.
    #[inline]
    fn bucket_index(&self, key: &K) -> usize {
        debug_assert!(self.is_initialized(), "HashTable used before initialization");
        H::get_hash(key) % self.buckets.len()
    }

    /// Returns the index of the bucket `obj` belongs in.
    #[inline]
    fn bucket_index_of(&self, obj: &P::Target) -> usize {
        let obj_key = <P::Target as HashTableKeyable<K>>::get_key(obj);
        self.bucket_index(obj_key.borrow())
    }

    /// Returns true if the key of `obj` is equal to `key`.
    #[inline]
    fn key_matches(obj: &P::Target, key: &K) -> bool {
        let obj_key = <P::Target as HashTableKeyable<K>>::get_key(obj);
        let obj_key: &K = obj_key.borrow();
        obj_key == key
    }

    /// Returns a pointer to the element of `bucket` whose key is `key`, if there is one.
    #[inline]
    fn find_in_bucket(bucket: &B, key: &K) -> Option<NonNull<P::Target>> {
        bucket.find_if(|obj| Self::key_matches(obj, key)).map(NonNull::from)
    }

    /// Locates the bucket for the object pointed to by `ptr`, and any element already in that
    /// bucket with the same key. Returns the bucket index and a pointer to the colliding element,
    /// if there is one.
    #[inline]
    fn locate(&self, ptr: &P) -> (usize, Option<NonNull<P::Target>>) {
        let obj_key = <P::Target as HashTableKeyable<K>>::get_key(PtrTraits::get_ref(ptr));
        let key: &K = obj_key.borrow();
        let ndx = self.bucket_index(key);
        (ndx, Self::find_in_bucket(&self.buckets[ndx], key))
    }

    /// Inserts an element into the hash table.
    ///
    /// Duplicate keys are disallowed. If there might already be an element with the same key in
    /// the hash table, use `insert_or_find` or `insert_or_replace` instead.
    ///
    /// For raw pointers, use `insert_raw` instead.
    ///
    /// # Panics
    ///
    /// Panics if the hash table has not been initialized. Panics in debug builds if the object is
    /// already in a container, or if an element with the same key is already in the hash table.
    #[inline]
    pub fn insert(&mut self, ptr: P)
    where
        P: ManagedPtr,
    {
        // SAFETY: `P` is a `ManagedPtr`, which guarantees that the pointer is valid and that the
        // object will outlive its reference from this hash table.
        unsafe { self.insert_raw(ptr) }
    }

    /// Inserts an element into the hash table.
    ///
    /// Duplicate keys are disallowed. If there might already be an element with the same key in
    /// the hash table, use `insert_or_find_raw` or `insert_or_replace_raw` instead.
    ///
    /// # Panics
    ///
    /// Panics if the hash table has not been initialized. Panics in debug builds if the object is
    /// already in a container, or if an element with the same key is already in the hash table.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `ptr` is a valid pointer to an object that is not currently in
    /// any container, and that the object outlives the reference from the hash table.
    pub unsafe fn insert_raw(&mut self, ptr: P) {
        let (ndx, existing) = self.locate(&ptr);

        // Duplicate keys are disallowed. Debug assert if someone tries to insert an element with a
        // duplicate key. If the user thought that there might be a duplicate key in the hash table
        // already, they should have used insert_or_find() instead.
        debug_assert!(existing.is_none(), "Duplicate keys are not allowed in a HashTable");

        // SAFETY: The caller guarantees that `ptr` is valid, not in any container, and outlives
        // its reference from the bucket.
        unsafe { self.bucket_mut(ndx).push_front_raw(ptr) };
        self.count += 1;
    }

    /// Inserts the element pointed to by `ptr` if no element with the same key is already in the
    /// hash table, or finds the element that `ptr` collided with instead.
    ///
    /// For raw pointers, use `insert_or_find_raw` instead.
    ///
    /// # Returns
    ///
    /// * `Ok(())` if there was no collision and the element was successfully inserted. In this
    ///   case, the hash table takes ownership of `ptr`.
    /// * `Err((ptr, cursor))` if there was a collision. In this case, `ptr` is not consumed and is
    ///   returned to the caller, along with a cursor positioned at the colliding element already in
    ///   the hash table.
    ///
    /// # Panics
    ///
    /// Panics if the hash table has not been initialized. Panics in debug builds if the object is
    /// already in a container.
    #[inline]
    pub fn insert_or_find(&mut self, ptr: P) -> HashTableInsertOrFindResult<'_, K, P, B, H>
    where
        P: ManagedPtr,
    {
        // SAFETY: `P` is a `ManagedPtr`, which guarantees that the pointer is valid and that the
        // object will outlive its reference from this hash table.
        unsafe { self.insert_or_find_raw(ptr) }
    }

    /// Inserts the element pointed to by `ptr` if no element with the same key is already in the
    /// hash table, or finds the element that `ptr` collided with instead.
    ///
    /// See `insert_or_find` for a description of the return value.
    ///
    /// # Panics
    ///
    /// Panics if the hash table has not been initialized. Panics in debug builds if the object is
    /// already in a container.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `ptr` is a valid pointer to an object that is not currently in
    /// any container, and that the object outlives the reference from the hash table.
    pub unsafe fn insert_or_find_raw(
        &mut self,
        ptr: P,
    ) -> HashTableInsertOrFindResult<'_, K, P, B, H> {
        let (ndx, existing) = self.locate(&ptr);
        if existing.is_some() {
            return Err((ptr, HashTableCursorMut { table: self, bucket: ndx, current: existing }));
        }

        // SAFETY: The caller guarantees that `ptr` is valid, not in any container, and outlives
        // its reference from the bucket.
        unsafe { self.bucket_mut(ndx).push_front_raw(ptr) };
        self.count += 1;
        Ok(())
    }

    /// Finds the element in the hash table with the same key as `*ptr` and replaces it with `ptr`,
    /// returning the element which was replaced.
    ///
    /// If no element in the hash table shares a key with `*ptr`, simply adds `ptr` to the hash
    /// table and returns `None`. In both cases, `ptr` is consumed.
    ///
    /// For raw pointers, use `insert_or_replace_raw` instead.
    ///
    /// # Panics
    ///
    /// Panics if the hash table has not been initialized. Panics in debug builds if the object is
    /// already in a container.
    #[inline]
    pub fn insert_or_replace(&mut self, ptr: P) -> Option<P>
    where
        P: ManagedPtr,
    {
        // SAFETY: `P` is a `ManagedPtr`, which guarantees that the pointer is valid and that the
        // object will outlive its reference from this hash table.
        unsafe { self.insert_or_replace_raw(ptr) }
    }

    /// Finds the element in the hash table with the same key as `*ptr` and replaces it with `ptr`,
    /// returning the element which was replaced.
    ///
    /// If no element in the hash table shares a key with `*ptr`, simply adds `ptr` to the hash
    /// table and returns `None`. In both cases, `ptr` is consumed.
    ///
    /// # Panics
    ///
    /// Panics if the hash table has not been initialized. Panics in debug builds if the object is
    /// already in a container.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `ptr` is a valid pointer to an object that is not currently in
    /// any container, and that the object outlives the reference from the hash table.
    pub unsafe fn insert_or_replace_raw(&mut self, ptr: P) -> Option<P> {
        let (ndx, existing) = self.locate(&ptr);
        let bucket = self.bucket_mut(ndx);
        match existing {
            None => {
                // SAFETY: The caller guarantees that `ptr` is valid, not in any container, and
                // outlives its reference from the bucket.
                unsafe { bucket.push_front_raw(ptr) };
                self.count += 1;
                None
            }
            // SAFETY: `existing` was just found in `bucket` and nothing has modified the bucket
            // since. The caller guarantees that `ptr` is valid, not in any container, and outlives
            // its reference from the bucket.
            Some(existing) => unsafe { bucket.replace_raw(existing.as_ref(), ptr) },
        }
    }

    /// Finds the element in the hash table with the given key.
    ///
    /// # Panics
    ///
    /// Panics if the hash table has not been initialized.
    #[inline]
    pub fn find(&self, key: &K) -> Option<&P::Target> {
        let ndx = self.bucket_index(key);
        self.buckets[ndx].find_if(|obj| Self::key_matches(obj, key))
    }

    /// Finds the element in the hash table with the given key and returns a cursor positioned at
    /// it.
    ///
    /// If the key is not found, the returned cursor is not positioned at any element (i.e.
    /// `cursor.get()` returns `None`).
    ///
    /// # Panics
    ///
    /// Panics if the hash table has not been initialized.
    #[inline]
    pub fn find_cursor(&mut self, key: &K) -> HashTableCursorMut<'_, K, P, B, H> {
        let ndx = self.bucket_index(key);
        let current = Self::find_in_bucket(&self.buckets[ndx], key);
        HashTableCursorMut { table: self, bucket: ndx, current }
    }

    /// Erases the element with the given key from the hash table, returning it.
    ///
    /// Returns `None` if no element in the hash table has the given key.
    ///
    /// # Panics
    ///
    /// Panics if the hash table has not been initialized.
    #[inline]
    pub fn erase(&mut self, key: &K) -> Option<P> {
        let ndx = self.bucket_index(key);
        let ret = self.bucket_mut(ndx).erase_if(|obj| Self::key_matches(obj, key));
        if ret.is_some() {
            self.count -= 1;
        }
        ret
    }

    /// Erases the given element from the hash table, returning it.
    ///
    /// Returns `None` if `obj` is not in any container. This operation runs in constant time with
    /// `DoublyLinkedList` buckets, and in time proportional to the length of the bucket of `obj`
    /// with `SinglyLinkedList` buckets.
    ///
    /// # Panics
    ///
    /// Panics if the hash table has not been initialized.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `obj` is either not in any container, or is currently in this
    /// hash table instance and its key has not changed since it was inserted.
    #[inline]
    pub unsafe fn erase_element(&mut self, obj: &P::Target) -> Option<P> {
        let ndx = self.bucket_index_of(obj);
        // SAFETY: The caller guarantees that `obj` is either in no container (in which case the
        // bucket returns `None`) or in this hash table with an unchanged key, in which case it is
        // in the bucket its key hashes to, `self.buckets[ndx]`.
        let ret = unsafe { self.bucket_mut(ndx).erase(obj) };
        if ret.is_some() {
            self.count -= 1;
        }
        ret
    }

    /// Finds the first element of the hash table which satisfies the predicate, erases it from the
    /// hash table, and returns it. Returns `None` if no element satisfies the predicate.
    pub fn erase_if<F>(&mut self, mut f: F) -> Option<P>
    where
        F: FnMut(&P::Target) -> bool,
    {
        debug_assert!(self.is_initialized(), "HashTable used before initialization");

        if self.is_empty() {
            return None;
        }

        let ret = (0..self.bucket_count()).find_map(|ndx| self.bucket_mut(ndx).erase_if(&mut f));
        if ret.is_some() {
            self.count -= 1;
        }
        ret
    }

    /// Finds the first element of the hash table which satisfies the predicate. Returns `None` if
    /// no element satisfies the predicate.
    #[inline]
    pub fn find_if<F>(&self, mut f: F) -> Option<&P::Target>
    where
        F: FnMut(&P::Target) -> bool,
    {
        debug_assert!(self.is_initialized(), "HashTable used before initialization");
        self.iter().find(|&obj| f(obj))
    }

    /// Removes all elements from the hash table.
    ///
    /// For managed pointer types, this releases all of the references held by the hash table to
    /// the objects which were in it. Clearing an uninitialized hash table is a no-op.
    pub fn clear(&mut self) {
        for ndx in 0..self.bucket_count() {
            self.bucket_mut(ndx).clear();
        }
        self.count = 0;
    }

    /// Returns an iterator over the elements of the hash table, in an unspecified order.
    #[inline]
    pub fn iter(&self) -> HashTableIter<'_, P, B> {
        HashTableIter { buckets: self.buckets.iter(), current: None, remaining: self.count }
    }
}

impl<K, P, B, H> Default for HashTable<K, P, B, H>
where
    P: PtrTraits,
    P::Target: HashTableKeyable<K>,
    B: HashTableBucket<P>,
    K: Eq,
    H: HashTraits<K>,
{
    /// Creates a new, uninitialized hash table. See `HashTable::new`.
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<K, P, B, H> Drop for HashTable<K, P, B, H>
where
    P: PtrTraits,
    P::Target: HashTableKeyable<K>,
    B: HashTableBucket<P>,
    K: Eq,
    H: HashTraits<K>,
{
    fn drop(&mut self) {
        if P::IS_MANAGED {
            self.clear();
        } else {
            debug_assert!(self.is_empty(), "HashTable must be empty on destruction");
        }
    }
}

impl<K, P, B, H> core::fmt::Debug for HashTable<K, P, B, H>
where
    P: PtrTraits,
    P::Target: HashTableKeyable<K> + core::fmt::Debug,
    B: HashTableBucket<P>,
    K: Eq,
    H: HashTraits<K>,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

impl<'a, K, P, B, H> IntoIterator for &'a HashTable<K, P, B, H>
where
    P: PtrTraits,
    P::Target: HashTableKeyable<K>,
    B: HashTableBucket<P>,
    K: Eq,
    H: HashTraits<K>,
{
    type Item = &'a P::Target;
    type IntoIter = HashTableIter<'a, P, B>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// A cursor positioned at an element of a `HashTable` (or at no element), which can be used to
/// access or erase that element.
///
/// Returned by `HashTable::find_cursor` and `HashTable::insert_or_find`.
pub struct HashTableCursorMut<'a, K, P, B = SinglyLinkedList<P>, H = DefaultHashTraits>
where
    P: PtrTraits,
    P::Target: HashTableKeyable<K>,
    B: HashTableBucket<P>,
    K: Eq,
    H: HashTraits<K>,
{
    table: &'a mut HashTable<K, P, B, H>,
    /// The index of the bucket containing `current`.
    bucket: usize,
    /// The element the cursor is positioned at, if any.
    current: Option<NonNull<P::Target>>,
}

impl<'a, K, P, B, H> HashTableCursorMut<'a, K, P, B, H>
where
    P: PtrTraits,
    P::Target: HashTableKeyable<K>,
    B: HashTableBucket<P>,
    K: Eq,
    H: HashTraits<K>,
{
    /// Returns a reference to the element the cursor is positioned at, or `None` if it is not
    /// positioned at any element.
    #[inline]
    pub fn get(&self) -> Option<&P::Target> {
        // SAFETY: `self.current` points to an element of `self.table`, which has been exclusively
        // borrowed by this cursor since the element was found, so it is still valid.
        self.current.map(|current| unsafe { current.as_ref() })
    }

    /// Erases the element the cursor is positioned at from the hash table, returning it.
    ///
    /// Returns `None` if the cursor is not positioned at any element.
    #[inline]
    pub fn erase(self) -> Option<P> {
        let current = self.current?;
        // SAFETY: `current` was found in `self.table.buckets[self.bucket]`, and the table has been
        // exclusively borrowed by this cursor since, so it is still valid and in that bucket.
        let ret = unsafe { self.table.bucket_mut(self.bucket).erase(current.as_ref()) };
        if ret.is_some() {
            self.table.count -= 1;
        }
        ret
    }
}

/// An iterator over the elements of a `HashTable`, in an unspecified order.
pub struct HashTableIter<'a, P, B = SinglyLinkedList<P>>
where
    P: PtrTraits + 'a,
    B: HashTableBucket<P>,
{
    /// The buckets which have not been visited yet.
    buckets: core::slice::Iter<'a, B>,
    /// The iterator over the bucket currently being visited, if any.
    current: Option<B::Iter<'a>>,
    /// The number of elements which have not been yielded yet.
    remaining: usize,
}

impl<'a, P, B> Iterator for HashTableIter<'a, P, B>
where
    P: PtrTraits + 'a,
    B: HashTableBucket<P> + 'a,
{
    type Item = &'a P::Target;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(obj) = self.current.as_mut().and_then(|iter| iter.next()) {
                self.remaining -= 1;
                return Some(obj);
            }
            match self.buckets.next() {
                Some(bucket) => self.current = Some(bucket.iter()),
                None => {
                    self.current = None;
                    return None;
                }
            }
        }
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl<'a, P, B> ExactSizeIterator for HashTableIter<'a, P, B>
where
    P: PtrTraits + 'a,
    B: HashTableBucket<P> + 'a,
{
}

impl<'a, P, B> core::iter::FusedIterator for HashTableIter<'a, P, B>
where
    P: PtrTraits + 'a,
    B: HashTableBucket<P> + 'a,
{
}

#[cfg(test)]
mod tests {
    extern crate alloc;
    use super::*;
    use crate::doubly_linked_list::DoublyLinkedListNode;
    use crate::intrusive_container_test_support::*;
    use crate::ref_ptr::RefPtr;
    use crate::singly_linked_list::SinglyLinkedListNode;
    use crate::unique_ptr::UniquePtr;

    const REGULAR_BUCKETS: usize = 37;
    const SHARED_BUCKETS: usize = 1031;

    #[derive(
        crate::SinglyLinkedListContainable, crate::DoublyLinkedListContainable, crate::Recyclable,
    )]
    struct TestObject {
        value: i32,
        #[sll_node]
        sll_node: SinglyLinkedListNode<TestObject>,
        #[dll_node]
        dll_node: DoublyLinkedListNode<TestObject>,
    }

    impl TestValue for TestObject {
        fn new(value: i32) -> Self {
            Self {
                value,
                sll_node: SinglyLinkedListNode::new(),
                dll_node: DoublyLinkedListNode::new(),
            }
        }
    }

    impl HashTableKeyable<usize> for TestObject {
        type Key<'a> = usize;
        fn get_key(&self) -> usize {
            self.value as usize
        }
    }

    #[derive(
        crate::SinglyLinkedListContainable, crate::DoublyLinkedListContainable, crate::Recyclable,
    )]
    struct UniqueTestObject {
        value: i32,
        #[sll_node]
        sll_node: SinglyLinkedListNode<UniqueTestObject>,
        #[dll_node]
        dll_node: DoublyLinkedListNode<UniqueTestObject>,
    }

    impl TestValue for UniqueTestObject {
        fn new(value: i32) -> Self {
            Self {
                value,
                sll_node: SinglyLinkedListNode::new(),
                dll_node: DoublyLinkedListNode::new(),
            }
        }
    }

    impl HashTableKeyable<usize> for UniqueTestObject {
        type Key<'a> = usize;
        fn get_key(&self) -> usize {
            self.value as usize
        }
    }

    #[fbl::ref_counted]
    #[derive(
        crate::SinglyLinkedListContainable, crate::DoublyLinkedListContainable, crate::Recyclable,
    )]
    #[repr(C)]
    pub struct RefTestObject {
        value: i32,
        #[sll_node]
        sll_node: SinglyLinkedListNode<RefTestObject>,
        #[dll_node]
        dll_node: DoublyLinkedListNode<RefTestObject>,
    }

    impl TestValue for RefTestObject {
        fn new_ref_counted(value: i32) -> RefPtr<Self> {
            crate::make_ref_counted!(RefTestObject {
                value: value,
                sll_node: SinglyLinkedListNode::new(),
                dll_node: DoublyLinkedListNode::new()
            })
            .unwrap()
        }
    }

    impl HashTableKeyable<usize> for RefTestObject {
        type Key<'a> = usize;
        fn get_key(&self) -> usize {
            self.value as usize
        }
    }

    // A dynamically sized HashTable consists of an element count, a pointer to the bucket storage,
    // and a bucket count, regardless of the bucket type.
    ::zr::static_assert!(
        core::mem::size_of::<HashTable<usize, *mut TestObject>>()
            == 3 * core::mem::size_of::<usize>()
    );
    ::zr::static_assert!(
        core::mem::align_of::<HashTable<usize, *mut TestObject>>()
            == core::mem::align_of::<usize>()
    );
    ::zr::static_assert!(
        core::mem::size_of::<HashTable<usize, *mut TestObject, DoublyLinkedList<*mut TestObject>>>(
        ) == 3 * core::mem::size_of::<usize>()
    );

    macro_rules! generate_hash_table_tests {
        ($mod_name:ident, $ptr_type:ty, $bucket_type:ty, $factory_type:ty, $insert:expr) => {
            mod $mod_name {
                use super::*;

                type Table = HashTable<usize, $ptr_type, $bucket_type>;

                fn raw_of(ptr: &$ptr_type) -> *const <$ptr_type as PtrTraits>::Target {
                    PtrTraits::get_ref(ptr)
                }

                /// Returns pointers to the elements of a bucket, in iteration order.
                fn bucket_elements(
                    table: &Table,
                    ndx: usize,
                ) -> alloc::vec::Vec<*const <$ptr_type as PtrTraits>::Target> {
                    table.buckets[ndx].iter().map(|obj| obj as *const _).collect()
                }

                #[test]
                fn test_basic() {
                    let mut factory = <$factory_type>::new();
                    let mut table = Table::try_new(REGULAR_BUCKETS).unwrap();
                    assert!(table.is_initialized());
                    assert_eq!(table.bucket_count(), REGULAR_BUCKETS);
                    assert!(table.is_empty());
                    assert_eq!(table.len(), 0);

                    $insert(&mut table, factory.create(1));
                    $insert(&mut table, factory.create(2));
                    $insert(&mut table, factory.create(3));
                    assert!(!table.is_empty());
                    assert_eq!(table.len(), 3);

                    assert_eq!(table.find(&1).unwrap().value, 1);
                    assert_eq!(table.find(&2).unwrap().value, 2);
                    assert_eq!(table.find(&3).unwrap().value, 3);
                    assert!(table.find(&4).is_none());

                    let erased = table.erase(&2).unwrap();
                    assert_eq!(PtrTraits::get_ref(&erased).value, 2);
                    assert_eq!(table.len(), 2);
                    assert!(table.find(&2).is_none());
                    assert!(table.erase(&2).is_none());
                    assert_eq!(table.len(), 2);

                    table.clear();
                    assert!(table.is_empty());
                    assert_eq!(table.len(), 0);
                    assert!(table.find(&1).is_none());
                    assert!(table.find(&3).is_none());
                }

                #[test]
                fn test_collisions() {
                    let mut factory = <$factory_type>::new();
                    let mut table = Table::try_new(REGULAR_BUCKETS).unwrap();

                    // All of these keys hash to the same bucket.
                    let base = 5;
                    let keys = [base, base + REGULAR_BUCKETS, base + 2 * REGULAR_BUCKETS];
                    for key in keys {
                        $insert(&mut table, factory.create(key as i32));
                    }
                    assert_eq!(table.len(), 3);
                    assert_eq!(table.buckets[base].iter().count(), 3);
                    for (ndx, bucket) in table.buckets.iter().enumerate() {
                        if ndx != base {
                            assert!(bucket.is_empty());
                        }
                    }

                    for key in keys {
                        assert_eq!(table.find(&key).unwrap().value, key as i32);
                    }

                    // Erase the middle element of the chain; the others must still be found.
                    assert!(table.erase(&keys[1]).is_some());
                    assert_eq!(table.len(), 2);
                    assert!(table.find(&keys[1]).is_none());
                    assert_eq!(table.find(&keys[0]).unwrap().value, keys[0] as i32);
                    assert_eq!(table.find(&keys[2]).unwrap().value, keys[2] as i32);
                    assert_eq!(table.buckets[base].iter().count(), 2);

                    table.clear();
                    assert!(table.buckets[base].is_empty());
                }

                #[test]
                fn test_insert_or_find() {
                    let mut factory = <$factory_type>::new();
                    let mut table = Table::try_new(REGULAR_BUCKETS).unwrap();

                    let first = factory.create(7);
                    let first_raw = raw_of(&first);
                    assert!(unsafe { table.insert_or_find_raw(first) }.is_ok());
                    assert_eq!(table.len(), 1);

                    // A second object with the same key collides with the first one.
                    let dup = factory.create(7);
                    let dup_raw = raw_of(&dup);
                    match unsafe { table.insert_or_find_raw(dup) } {
                        Ok(()) => panic!("insert_or_find must not insert a duplicate key"),
                        Err((returned, cursor)) => {
                            assert!(core::ptr::eq(raw_of(&returned), dup_raw));
                            assert!(core::ptr::eq(cursor.get().unwrap(), first_raw));
                        }
                    }
                    assert_eq!(table.len(), 1);
                    assert!(core::ptr::eq(table.find(&7).unwrap(), first_raw));

                    // A different key is inserted.
                    assert!(unsafe { table.insert_or_find_raw(factory.create(8)) }.is_ok());
                    assert_eq!(table.len(), 2);

                    // The cursor returned on a collision can erase the colliding element.
                    let dup = factory.create(8);
                    match unsafe { table.insert_or_find_raw(dup) } {
                        Ok(()) => panic!("insert_or_find must not insert a duplicate key"),
                        Err((_returned, cursor)) => {
                            assert_eq!(
                                cursor.erase().map(|p| PtrTraits::get_ref(&p).value),
                                Some(8)
                            );
                        }
                    }
                    assert_eq!(table.len(), 1);
                    assert!(table.find(&8).is_none());

                    table.clear();
                }

                #[test]
                fn test_insert_or_replace() {
                    let mut factory = <$factory_type>::new();
                    let mut table = Table::try_new(REGULAR_BUCKETS).unwrap();

                    let first = factory.create(3);
                    let first_raw = raw_of(&first);
                    assert!(unsafe { table.insert_or_replace_raw(first) }.is_none());
                    assert_eq!(table.len(), 1);

                    let second = factory.create(3);
                    let second_raw = raw_of(&second);
                    let replaced = unsafe { table.insert_or_replace_raw(second) }.unwrap();
                    assert!(core::ptr::eq(raw_of(&replaced), first_raw));
                    assert!(!PtrTraits::get_ref(&replaced).sll_node.in_container());
                    assert!(!PtrTraits::get_ref(&replaced).dll_node.in_container());
                    assert_eq!(table.len(), 1);
                    assert!(core::ptr::eq(table.find(&3).unwrap(), second_raw));

                    assert!(unsafe { table.insert_or_replace_raw(factory.create(4)) }.is_none());
                    assert_eq!(table.len(), 2);

                    table.clear();
                }

                #[test]
                fn test_insert_or_replace_in_chain() {
                    let mut factory = <$factory_type>::new();
                    let mut table = Table::try_new(REGULAR_BUCKETS).unwrap();

                    // All of these keys hash to the same bucket.
                    let base = 9;
                    let keys = [base, base + REGULAR_BUCKETS, base + 2 * REGULAR_BUCKETS];
                    for key in keys {
                        $insert(&mut table, factory.create(key as i32));
                    }

                    // Replace each element of the chain, starting from the middle, then the tail,
                    // then the head.
                    for position in [1, 2, 0] {
                        let elements = bucket_elements(&table, base);
                        let old = elements[position];
                        // SAFETY: `old` is in `table`.
                        let key = unsafe { &*old }.value;

                        let replacement = factory.create(key);
                        let replacement_raw = raw_of(&replacement);
                        let replaced = unsafe { table.insert_or_replace_raw(replacement) }.unwrap();
                        assert!(core::ptr::eq(raw_of(&replaced), old));
                        assert_eq!(table.len(), keys.len());
                        assert!(core::ptr::eq(
                            table.find(&(key as usize)).unwrap(),
                            replacement_raw
                        ));

                        // The replacement takes the position of the replaced element.
                        let mut expected = elements;
                        expected[position] = replacement_raw;
                        assert_eq!(bucket_elements(&table, base), expected);
                    }

                    table.clear();
                }

                #[test]
                fn test_find_cursor() {
                    let mut factory = <$factory_type>::new();
                    let mut table = Table::try_new(REGULAR_BUCKETS).unwrap();
                    for i in 0..10 {
                        $insert(&mut table, factory.create(i));
                    }

                    assert_eq!(table.find_cursor(&4).get().unwrap().value, 4);
                    let erased = table.find_cursor(&4).erase().unwrap();
                    assert_eq!(PtrTraits::get_ref(&erased).value, 4);
                    assert_eq!(table.len(), 9);
                    assert!(table.find(&4).is_none());

                    assert!(table.find_cursor(&4).get().is_none());
                    assert!(table.find_cursor(&4).erase().is_none());
                    assert_eq!(table.len(), 9);

                    table.clear();
                }

                #[test]
                fn test_erase_element() {
                    let mut factory = <$factory_type>::new();
                    let mut table = Table::try_new(REGULAR_BUCKETS).unwrap();
                    for i in 0..10 {
                        $insert(&mut table, factory.create(i));
                    }

                    let obj: *const _ = table.find(&6).unwrap();
                    // SAFETY: `obj` is in `table` and its key has not changed.
                    let erased = unsafe { table.erase_element(&*obj) }.unwrap();
                    assert!(core::ptr::eq(raw_of(&erased), obj));
                    assert_eq!(table.len(), 9);
                    assert!(table.find(&6).is_none());

                    // Erasing an element which is no longer in any container is a no-op.
                    // SAFETY: `erased` is not in any container.
                    assert!(unsafe { table.erase_element(PtrTraits::get_ref(&erased)) }.is_none());
                    assert_eq!(table.len(), 9);

                    table.clear();
                }

                #[test]
                fn test_erase_element_in_chain() {
                    // Erase elements from the middle, the tail, and the head of a chain, in every
                    // order, through both `erase_element` and cursors.
                    let orders = [[0, 1, 2], [0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]];
                    for (iteration, order) in orders.iter().enumerate() {
                        let mut factory = <$factory_type>::new();
                        let mut table = Table::try_new(REGULAR_BUCKETS).unwrap();

                        // All of these keys hash to the same bucket.
                        let base = 11;
                        let keys = [base, base + REGULAR_BUCKETS, base + 2 * REGULAR_BUCKETS];
                        for key in keys {
                            $insert(&mut table, factory.create(key as i32));
                        }

                        let mut remaining = bucket_elements(&table, base);
                        let mut positions = alloc::vec::Vec::from(*order);
                        for step in 0..positions.len() {
                            let position = positions.remove(0);
                            let obj = remaining.remove(position);
                            // Shift the positions after the removed element.
                            for p in positions.iter_mut() {
                                if *p > position {
                                    *p -= 1;
                                }
                            }

                            let erased = if (iteration + step) % 2 == 0 {
                                // SAFETY: `obj` is in `table` and its key has not changed.
                                unsafe { table.erase_element(&*obj) }.unwrap()
                            } else {
                                // SAFETY: `obj` is in `table`.
                                let key = unsafe { &*obj }.value as usize;
                                table.find_cursor(&key).erase().unwrap()
                            };
                            assert!(core::ptr::eq(raw_of(&erased), obj));
                            assert_eq!(table.len(), remaining.len());
                            assert_eq!(bucket_elements(&table, base), remaining);
                            for &other in remaining.iter() {
                                // SAFETY: `other` is in `table`.
                                let key = unsafe { &*other }.value as usize;
                                assert!(core::ptr::eq(table.find(&key).unwrap(), other));
                            }
                        }
                        assert!(table.is_empty());
                        assert!(table.buckets[base].is_empty());
                    }
                }

                #[test]
                fn test_iter() {
                    let mut factory = <$factory_type>::new();
                    let mut table = Table::try_new(REGULAR_BUCKETS).unwrap();
                    assert!(table.iter().next().is_none());

                    const COUNT: i32 = 100;
                    for i in 0..COUNT {
                        $insert(&mut table, factory.create(i));
                    }

                    let iter = table.iter();
                    assert_eq!(iter.len(), COUNT as usize);
                    let mut seen = [false; COUNT as usize];
                    let mut count = 0;
                    for obj in &table {
                        assert!(!seen[obj.value as usize]);
                        seen[obj.value as usize] = true;
                        count += 1;
                    }
                    assert_eq!(count, COUNT);
                    assert!(seen.iter().all(|&s| s));

                    table.clear();
                    assert!(table.iter().next().is_none());
                }

                #[test]
                fn test_erase_if_and_find_if() {
                    let mut factory = <$factory_type>::new();
                    let mut table = Table::try_new(REGULAR_BUCKETS).unwrap();
                    assert!(table.erase_if(|_| true).is_none());

                    for i in 0..20 {
                        $insert(&mut table, factory.create(i));
                    }

                    assert_eq!(table.find_if(|obj| obj.value == 13).unwrap().value, 13);
                    assert!(table.find_if(|obj| obj.value == 20).is_none());

                    let erased = table.erase_if(|obj| obj.value == 13).unwrap();
                    assert_eq!(PtrTraits::get_ref(&erased).value, 13);
                    assert_eq!(table.len(), 19);
                    assert!(table.find(&13).is_none());
                    assert!(table.erase_if(|obj| obj.value == 13).is_none());

                    // Drain the table using erase_if.
                    while table.erase_if(|_| true).is_some() {}
                    assert!(table.is_empty());
                    assert!(table.iter().next().is_none());
                }

                #[test]
                fn test_dynamic_bucket_counts() {
                    for bucket_count in [REGULAR_BUCKETS, SHARED_BUCKETS] {
                        let mut factory = <$factory_type>::new();
                        let mut table = Table::try_new(bucket_count).unwrap();
                        assert_eq!(table.bucket_count(), bucket_count);

                        // Insert two elements per bucket.
                        let count = 2 * bucket_count;
                        for i in 0..count {
                            $insert(&mut table, factory.create(i as i32));
                        }
                        assert_eq!(table.len(), count);
                        for bucket in table.buckets.iter() {
                            assert_eq!(bucket.iter().count(), 2);
                        }

                        for i in 0..count {
                            assert_eq!(table.find(&i).unwrap().value, i as i32);
                        }
                        for i in 0..count {
                            assert!(table.erase(&i).is_some());
                            assert_eq!(table.len(), count - i - 1);
                        }
                        assert!(table.is_empty());
                        assert!(table.buckets.iter().all(|bucket| bucket.is_empty()));
                    }
                }
            }
        };
    }

    generate_hash_table_tests!(
        raw_ptr_sll_tests,
        *mut TestObject,
        SinglyLinkedList<*mut TestObject>,
        RawFactory<TestObject>,
        |table: &mut Table, obj| unsafe { table.insert_raw(obj) }
    );

    generate_hash_table_tests!(
        raw_ptr_dll_tests,
        *mut TestObject,
        DoublyLinkedList<*mut TestObject>,
        RawFactory<TestObject>,
        |table: &mut Table, obj| unsafe { table.insert_raw(obj) }
    );

    generate_hash_table_tests!(
        unique_ptr_sll_tests,
        UniquePtr<UniqueTestObject>,
        SinglyLinkedList<UniquePtr<UniqueTestObject>>,
        UniqueFactory<UniqueTestObject>,
        |table: &mut Table, obj| table.insert(obj)
    );

    generate_hash_table_tests!(
        unique_ptr_dll_tests,
        UniquePtr<UniqueTestObject>,
        DoublyLinkedList<UniquePtr<UniqueTestObject>>,
        UniqueFactory<UniqueTestObject>,
        |table: &mut Table, obj| table.insert(obj)
    );

    generate_hash_table_tests!(
        ref_ptr_sll_tests,
        RefPtr<RefTestObject>,
        SinglyLinkedList<RefPtr<RefTestObject>>,
        RefFactory<RefTestObject>,
        |table: &mut Table, obj| table.insert(obj)
    );

    generate_hash_table_tests!(
        ref_ptr_dll_tests,
        RefPtr<RefTestObject>,
        DoublyLinkedList<RefPtr<RefTestObject>>,
        RefFactory<RefTestObject>,
        |table: &mut Table, obj| table.insert(obj)
    );

    #[test]
    fn test_delayed_init() {
        let mut table = HashTable::<usize, UniquePtr<UniqueTestObject>>::new();
        assert!(!table.is_initialized());
        assert_eq!(table.bucket_count(), 0);
        assert!(table.is_empty());
        assert_eq!(table.len(), 0);
        assert!(table.iter().next().is_none());
        table.clear();

        table.try_init(REGULAR_BUCKETS).unwrap();
        assert!(table.is_initialized());
        assert_eq!(table.bucket_count(), REGULAR_BUCKETS);
        assert!(table.is_empty());

        table.insert(UniquePtr::try_new(UniqueTestObject::new(1)).unwrap());
        assert_eq!(table.len(), 1);
        assert_eq!(table.find(&1).unwrap().value, 1);
    }

    #[test]
    fn test_default_is_uninitialized() {
        let table: HashTable<usize, *mut TestObject> = Default::default();
        assert!(!table.is_initialized());
        assert!(table.is_empty());

        let table: HashTable<usize, *mut TestObject, DoublyLinkedList<*mut TestObject>> =
            Default::default();
        assert!(!table.is_initialized());
        assert!(table.is_empty());
    }

    #[test]
    fn test_move_table() {
        // The bucket storage lives on the heap, so a HashTable may be moved freely, even while
        // it holds elements, and even with buckets which must be pinned.
        type DllTable = HashTable<
            usize,
            UniquePtr<UniqueTestObject>,
            DoublyLinkedList<UniquePtr<UniqueTestObject>>,
        >;
        let mut table = DllTable::try_new(REGULAR_BUCKETS).unwrap();
        for i in 0..10 {
            table.insert(UniquePtr::try_new(UniqueTestObject::new(i)).unwrap());
        }

        let mut moved = table;
        assert_eq!(moved.len(), 10);
        for i in 0..10 {
            assert_eq!(moved.find(&(i as usize)).unwrap().value, i);
        }
        assert!(moved.erase(&3).is_some());
        moved.insert(UniquePtr::try_new(UniqueTestObject::new(42)).unwrap());
        assert_eq!(moved.len(), 10);
        assert_eq!(moved.iter().count(), 10);

        // Replace an uninitialized table with an initialized one.
        let mut late = DllTable::new();
        late = core::mem::replace(&mut moved, late);
        assert_eq!(late.len(), 10);
        assert!(!moved.is_initialized());
    }

    struct ShiftHashTraits;

    impl HashTraits<usize> for ShiftHashTraits {
        fn get_hash(key: &usize) -> usize {
            *key >> 4
        }
    }

    #[test]
    fn test_custom_hash_traits() {
        let mut table = HashTable::<
            usize,
            UniquePtr<UniqueTestObject>,
            SinglyLinkedList<UniquePtr<UniqueTestObject>>,
            ShiftHashTraits,
        >::try_new(REGULAR_BUCKETS)
        .unwrap();

        // Keys 0..16 all hash to bucket 0, and keys 16..32 all hash to bucket 1.
        for i in 0..32 {
            table.insert(UniquePtr::try_new(UniqueTestObject::new(i)).unwrap());
        }
        assert_eq!(table.buckets[0].iter().count(), 16);
        assert_eq!(table.buckets[1].iter().count(), 16);
        for i in 0..32 {
            assert_eq!(table.find(&i).unwrap().value, i as i32);
        }
    }

    struct ReversedHashTraits;

    impl HashTraits<usize> for ReversedHashTraits {
        fn get_hash(key: &usize) -> usize {
            usize::MAX - *key
        }
    }

    #[test]
    fn test_hash_reduced_modulo_bucket_count() {
        // Hash values much larger than the bucket count are reduced modulo the bucket count.
        let mut table = HashTable::<
            usize,
            UniquePtr<UniqueTestObject>,
            DoublyLinkedList<UniquePtr<UniqueTestObject>>,
            ReversedHashTraits,
        >::try_new(REGULAR_BUCKETS)
        .unwrap();

        for i in 0..100 {
            table.insert(UniquePtr::try_new(UniqueTestObject::new(i)).unwrap());
        }
        for (ndx, bucket) in table.buckets.iter().enumerate() {
            for obj in bucket.iter() {
                assert_eq!((usize::MAX - obj.value as usize) % REGULAR_BUCKETS, ndx);
            }
        }
        for i in 0..100 {
            assert_eq!(table.find(&i).unwrap().value, i as i32);
        }
    }

    #[test]
    fn test_default_hash_traits() {
        assert_eq!(<DefaultHashTraits as HashTraits<usize>>::get_hash(&40), 40);
        assert_eq!(<DefaultHashTraits as HashTraits<u32>>::get_hash(&1031), 1031);
        assert_eq!(<DefaultHashTraits as HashTraits<u64>>::get_hash(&1), 1);
        assert_eq!(<DefaultHashTraits as HashTraits<u8>>::get_hash(&200), 200);
        assert_eq!(<DefaultHashTraits as HashTraits<usize>>::get_hash(&usize::MAX), usize::MAX);
    }

    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};

    #[derive(
        crate::SinglyLinkedListContainable, crate::DoublyLinkedListContainable, crate::Recyclable,
    )]
    struct LifecycleObject {
        key: u64,
        destroyed: Arc<AtomicUsize>,
        #[sll_node]
        sll_node: SinglyLinkedListNode<LifecycleObject>,
        #[dll_node]
        dll_node: DoublyLinkedListNode<LifecycleObject>,
    }

    impl LifecycleObject {
        fn new(key: u64, destroyed: Arc<AtomicUsize>) -> Self {
            Self {
                key,
                destroyed,
                sll_node: SinglyLinkedListNode::new(),
                dll_node: DoublyLinkedListNode::new(),
            }
        }
    }

    impl HashTableKeyable<u64> for LifecycleObject {
        type Key<'a> = u64;
        fn get_key(&self) -> u64 {
            self.key
        }
    }

    impl Drop for LifecycleObject {
        fn drop(&mut self) {
            self.destroyed.fetch_add(1, Ordering::Relaxed);
        }
    }

    type LifecycleDllBuckets = DoublyLinkedList<UniquePtr<LifecycleObject>>;

    #[test]
    fn test_lifecycle_on_drop() {
        let destroyed = Arc::new(AtomicUsize::new(0));
        {
            let mut table =
                HashTable::<u64, UniquePtr<LifecycleObject>>::try_new(SHARED_BUCKETS).unwrap();
            let mut dll_table =
                HashTable::<u64, UniquePtr<LifecycleObject>, LifecycleDllBuckets>::try_new(
                    SHARED_BUCKETS,
                )
                .unwrap();
            for key in 0..5 {
                table.insert(
                    UniquePtr::try_new(LifecycleObject::new(key, destroyed.clone())).unwrap(),
                );
                dll_table.insert(
                    UniquePtr::try_new(LifecycleObject::new(key, destroyed.clone())).unwrap(),
                );
            }
            assert_eq!(destroyed.load(Ordering::Relaxed), 0);
        } // tables drop here
        assert_eq!(destroyed.load(Ordering::Relaxed), 10);
    }

    #[test]
    fn test_lifecycle_on_clear_and_erase() {
        let destroyed = Arc::new(AtomicUsize::new(0));
        let mut table =
            HashTable::<u64, UniquePtr<LifecycleObject>>::try_new(REGULAR_BUCKETS).unwrap();
        for key in 0..5 {
            table.insert(UniquePtr::try_new(LifecycleObject::new(key, destroyed.clone())).unwrap());
        }

        // Erased elements are handed back to the caller, not destroyed.
        let erased = table.erase(&2).unwrap();
        assert_eq!(destroyed.load(Ordering::Relaxed), 0);
        drop(erased);
        assert_eq!(destroyed.load(Ordering::Relaxed), 1);

        table.clear();
        assert_eq!(destroyed.load(Ordering::Relaxed), 5);
        assert!(table.is_empty());
    }

    #[test]
    fn test_move_between_table_and_free_list() {
        // Mirrors how FutexContext moves objects between its free list and its active hash table,
        // which share the same `DoublyLinkedListNode`.
        pin_init::stack_pin_init!(let free = LifecycleDllBuckets::new());
        let destroyed = Arc::new(AtomicUsize::new(0));
        for _ in 0..4 {
            free.as_mut().push_front(
                UniquePtr::try_new(LifecycleObject::new(0, destroyed.clone())).unwrap(),
            );
        }

        let mut active =
            HashTable::<u64, UniquePtr<LifecycleObject>, LifecycleDllBuckets>::try_new(
                SHARED_BUCKETS,
            )
            .unwrap();
        let ids = [0x1000u64, 0x1000 + SHARED_BUCKETS as u64, 0x2000, 0x3000];
        for id in ids {
            let mut state = free.as_mut().pop_front().unwrap();
            state.key = id;
            active.insert(state);
        }
        assert!(free.is_empty());
        assert_eq!(active.len(), ids.len());

        for id in ids {
            let state: *const LifecycleObject = active.find(&id).unwrap();
            // SAFETY: `state` is in `active` and its key has not changed.
            let mut removed = unsafe { active.erase_element(&*state) }.unwrap();
            removed.key = 0;
            free.as_mut().push_front(removed);
        }
        assert!(active.is_empty());
        assert_eq!(destroyed.load(Ordering::Relaxed), 0);

        free.as_mut().clear();
        assert_eq!(destroyed.load(Ordering::Relaxed), 4);
    }
}
