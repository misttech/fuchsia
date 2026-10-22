// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::ptr_traits::{ManagedPtr, MutPtr, PtrTraits};
use crate::sentinel::{is_sentinel_ptr, make_sentinel};
use crate::size_tracker::{NonTrackingSize, SizeTracker, TrackingSize};
use crate::tag::DefaultObjectTag;
use core::cell::UnsafeCell;
use core::pin::Pin;
use pin_init::{PinInit, pin_data, pin_init, pinned_drop};

/// A node in a doubly linked list.
///
/// Nodes are `!Unpin`: while an element is linked into a list, its neighbors hold raw pointers to
/// it, so it must not be moved (or have its node swapped/overwritten). Containing types should mark
/// their node field(s) with `#[pin]` when using `#[pin_data]` so that the `!Unpin`-ness propagates.
#[repr(C)]
pub struct DoublyLinkedListNode<T> {
    /// The next element in the list.
    pub next: UnsafeCell<*mut T>,
    /// The previous element in the list.
    pub prev: UnsafeCell<*mut T>,
    /// Marker making the node (and any type embedding it) `!Unpin`.
    ///
    /// Wrapped in `PhantomData` so the node remains FFI-safe (`improper_ctypes` rejects a bare
    /// `PhantomPinned` in `extern` signatures, but ignores `PhantomData` fields).
    _pin: core::marker::PhantomData<core::marker::PhantomPinned>,
}

impl<T> DoublyLinkedListNode<T> {
    /// Creates a new, unlinked node.
    #[inline]
    pub const fn new() -> Self {
        Self {
            next: UnsafeCell::new(core::ptr::null_mut()),
            prev: UnsafeCell::new(core::ptr::null_mut()),
            _pin: core::marker::PhantomData,
        }
    }

    /// Returns true if the node is currently in a list.
    #[inline]
    pub fn in_container(&self) -> bool {
        // SAFETY: `self.next.get()` returns a valid pointer to the inner field of `self.next`
        // which is a validly allocated UnsafeCell inside `self`.
        !unsafe { *self.next.get() }.is_null()
    }

    #[inline]
    fn get_next(&self) -> *mut T {
        // SAFETY: `self.next.get()` is a valid pointer to `self.next` which is owned by `self`.
        unsafe { *self.next.get() }
    }

    #[inline]
    fn set_next(&self, next: *mut T) {
        // SAFETY: `self.next.get()` is a valid, writable pointer to `self.next` owned by `self`.
        // UnsafeCell allows interior mutability through a shared reference.
        unsafe {
            *self.next.get() = next;
        }
    }

    #[inline]
    fn get_prev(&self) -> *mut T {
        // SAFETY: `self.prev.get()` is a valid pointer to `self.prev` which is owned by `self`.
        unsafe { *self.prev.get() }
    }

    #[inline]
    fn set_prev(&self, prev: *mut T) {
        // SAFETY: `self.prev.get()` is a valid, writable pointer to `self.prev` owned by `self`.
        // UnsafeCell allows interior mutability through a shared reference.
        unsafe {
            *self.prev.get() = prev;
        }
    }
}

impl<T> core::fmt::Debug for DoublyLinkedListNode<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DoublyLinkedListNode").field("in_container", &self.in_container()).finish()
    }
}

impl<T> Default for DoublyLinkedListNode<T> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Drop for DoublyLinkedListNode<T> {
    #[inline]
    fn drop(&mut self) {
        debug_assert!(!self.in_container(), "Object destroyed while still in container");
    }
}

/// Trait that types must implement to be contained in a `DoublyLinkedList`.
pub trait DoublyLinkedListContainable<T, Tag = DefaultObjectTag> {
    /// Returns a reference to the list node.
    fn get_node(&self) -> &DoublyLinkedListNode<T>;
}

/// An intrusive doubly linked list container supporting custom ownership semantics, constant-time
/// operations, and circular-like node layout.
///
/// ### Bookkeeping & Memory Storage
///
/// The bookkeeping storage (`DoublyLinkedListNode`) required to link elements exists directly
/// on the objects themselves. This intrusive pattern eliminates the need for runtime bookkeeping
/// allocations/deallocations when adding or removing members to/from the container.
///
/// The list stores pointers to the objects, not the objects themselves, and is parameterized
/// based on the specific pointer wrapper to be stored (`P`). Supported pointer wrappers are:
///
/// * `*mut T`       : Raw unmanaged pointers.
/// * `UniquePtr<T>` : Unique managed pointers.
/// * `RefPtr<T>`    : Shared managed pointers to reference-counted objects.
///
/// ### Lifecycle Management
///
/// * **Managed Pointers (`UniquePtr`/`RefPtr`)**: The list holds ownership references of elements
///   and follows the rules of the respective smart pointer. Clearing the list or dropping it out of
///   scope automatically releases references, which may destruct the elements if it was their last
///   reference.
///
/// * **Unmanaged Pointers (`*mut T`)**: The list performs no lifecycle management. It is up to the
///   caller to ensure elements outlive the list and are freed correctly. As a safety check, a list
///   of unmanaged pointers will panic/debug-assert if it is dropped with elements still inside.
///
/// ### Ring Layout & Sentinel
///
/// Nodes are arranged in a circular-like ring structure:
///
/// * `head` stores a sentinel value (a pointer to the container itself) when the list is empty.
/// * For non-empty lists, the `next` pointer of the tail node points to the sentinel, and the
///   `prev` pointer of the head node points to the tail node. This allows constant-time O(1) tail
///   lookup and bidirectionality.
/// * Because the sentinel points back to the container's own memory address, the `DoublyLinkedList`
///   container **must be pinned in memory** (typically via `pin_init::stack_pin_init!`) and cannot
///   be safely moved after initialization.
///
/// ### Additional Functionality over SinglyLinkedList
///
/// * O(1) `push_back`, `pop_back`, and `back` operations.
/// * The ability to `insert` (before an element) in addition to `insert_after`.
/// * The ability to `erase` (by reference or iterator) in addition to `erase_next`.
/// * Bidirectional iteration support.
///
/// ### Multiple List Participation
///
/// Objects may exist on multiple lists simultaneously through the use of custom `Tag` classes
/// implementing `DoublyLinkedListContainable` multiple times.
///
/// ---
///
/// ### Example: Simple list of unmanaged raw pointers
///
/// ```rust
/// # use fbl::{DoublyLinkedList, DoublyLinkedListNode, stack_pin_init, pin_init::PinInit};
/// #[derive(fbl::DoublyLinkedListContainable)]
/// struct Foo {
///     value: i32,
///     #[dll_node]
///     node: DoublyLinkedListNode<Foo>,
/// }
///
/// impl Foo {
///     fn new(value: i32) -> Self {
///         Self { value, node: DoublyLinkedListNode::new() }
///     }
/// }
///
/// stack_pin_init!(let list = DoublyLinkedList::<*mut Foo>::new());
///
/// // SAFETY: The objects are freshly allocated and are freed only after being removed.
/// unsafe {
///     list.as_mut().push_front_raw(Box::into_raw(Box::new(Foo::new(1))));
///     list.as_mut().push_back_raw(Box::into_raw(Box::new(Foo::new(2))));
/// }
///
/// for foo in list.iter() {
///     println!("Value: {}", foo.value);
/// }
///
/// while let Some(foo_ptr) = list.as_mut().pop_front() {
///     // SAFETY: `foo_ptr` was created by `Box::into_raw` and is no longer in the list.
///     let _ = unsafe { Box::from_raw(foo_ptr) };
/// }
/// ```
///
/// ### Example: Simple list of unique managed pointers
///
/// ```rust
/// use fbl::{DoublyLinkedList, DoublyLinkedListNode, UniquePtr, stack_pin_init};
///
/// #[derive(fbl::DoublyLinkedListContainable, fbl::Recyclable)]
/// struct Foo {
///     value: i32,
///     #[dll_node]
///     node: DoublyLinkedListNode<Foo>,
/// }
///
/// impl Foo {
///     fn new(value: i32) -> Self {
///         Self { value, node: DoublyLinkedListNode::new() }
///     }
/// }
///
/// stack_pin_init!(let list = DoublyLinkedList::<UniquePtr<Foo>>::new());
///
/// list.as_mut().push_front(UniquePtr::try_new(Foo::new(1)).unwrap());
/// list.as_mut().push_back(UniquePtr::try_new(Foo::new(2)).unwrap());
///
/// for foo in list.iter() {
///     println!("Value: {}", foo.value);
/// }
///
/// // Clearing the list automatically drops unique pointers and reclaims their memory!
/// list.as_mut().clear();
/// ```
///
/// ### Example: Shared objects in multiple lists simultaneously using Tags
///
/// ```rust
/// use fbl::{DoublyLinkedList, DoublyLinkedListNode, RefPtr, stack_pin_init};
///
/// struct TagA;
/// struct TagB;
///
/// #[fbl::ref_counted]
/// #[derive(fbl::DoublyLinkedListContainable)]
/// struct Foo {
///     value: i32,
///     #[dll_node(TagA)]
///     node_a: DoublyLinkedListNode<Foo>,
///     #[dll_node(TagB)]
///     node_b: DoublyLinkedListNode<Foo>,
/// }
///
/// stack_pin_init!(let list_a = DoublyLinkedList::<RefPtr<Foo>, TagA>::new());
/// stack_pin_init!(let list_b = DoublyLinkedList::<RefPtr<Foo>, TagB>::new());
///
/// let foo = fbl::make_ref_counted!(Foo {
///     value: 42,
///     node_a: DoublyLinkedListNode::new(),
///     node_b: DoublyLinkedListNode::new(),
/// }).unwrap();
///
/// list_a.as_mut().push_back(foo.clone());
/// list_b.as_mut().push_back(foo);
/// ```
#[repr(C)]
#[pin_data(PinnedDrop)]
pub struct DoublyLinkedList<P, Tag = DefaultObjectTag, S = NonTrackingSize>
where
    P: PtrTraits,
    P::Target: DoublyLinkedListContainable<P::Target, Tag>,
    S: SizeTracker,
{
    /// Pointer to the first element of the list.
    ///
    /// # Link Structure
    ///
    /// Nodes in the list are arranged in a circular-like ring structure using a sentinel:
    /// * For a non-empty list, the `next` pointer of each node points to the next element,
    ///   and the `next` pointer of the **tail** node points to the **sentinel** (which is
    ///   a pointer back to this `DoublyLinkedList` container itself).
    /// * The `prev` pointer of each node points to the previous element.
    ///
    /// # Empty List Value
    ///
    /// When the list is empty, this `head` pointer holds the **sentinel** value (a pointer
    /// to the container itself).
    ///
    /// # Tail Pointer Location
    ///
    /// The tail pointer of the list is located in the `prev` field of the **head** node's
    /// list node (`head->prev`), which can be accessed or updated via `self.get_tail()` and
    /// `self.set_tail()`.
    head: *mut P::Target,

    /// The size tracker for the list, supporting either O(N) or O(1) size operations
    /// depending on the `S` parameter (e.g., `NonTrackingSize` or `TrackingSize`).
    size: S,

    /// Marker to ensure the list container is pinned in memory. Pinning is required
    /// because the sentinel pointer points back to the container's own memory address,
    /// meaning the list cannot be safely moved once initialized.
    ///
    /// Wrapped in `PhantomData` so the list remains FFI-safe when embedded in `#[repr(C)]`
    /// structs named in `extern` signatures (`improper_ctypes` rejects a bare `PhantomPinned`,
    /// but ignores `PhantomData` fields).
    #[pin]
    _pin: core::marker::PhantomData<core::marker::PhantomPinned>,

    _phantom: core::marker::PhantomData<(P, Tag)>,
}

impl<P, Tag, S> DoublyLinkedList<P, Tag, S>
where
    P: PtrTraits,
    P::Target: DoublyLinkedListContainable<P::Target, Tag>,
    S: SizeTracker,
{
    /// Creates a new, empty list.
    #[inline]
    pub fn new() -> impl PinInit<Self, core::convert::Infallible> {
        pin_init!(&this in Self {
            head: make_sentinel(this.as_ptr()),
            size: S::INIT,
            _pin: core::marker::PhantomData,
            _phantom: core::marker::PhantomData,
        })
    }

    #[inline]
    fn get_sentinel(&self) -> *mut P::Target {
        make_sentinel(self as *const Self as *mut Self)
    }

    #[inline]
    fn get_tail(&self) -> *mut P::Target {
        if self.is_empty() {
            self.get_sentinel()
        } else {
            // SAFETY: `self.head` is a valid, aligned pointer to an element in the list.
            // Reading `prev` from its node returns a valid pointer (either another node or
            // sentinel).
            unsafe { *(*self.head).get_node().prev.get() }
        }
    }

    /// # Safety
    ///
    /// The caller must ensure that the list is not empty.
    #[inline]
    unsafe fn set_tail(&self, tail: *mut P::Target) {
        debug_assert!(!self.is_empty());
        // SAFETY: `self.head` is a valid, aligned pointer to an element in the list.
        // Writing to its `prev` node UnsafeCell is safe because we have exclusive or shared access
        // and interior mutability is allowed.
        unsafe {
            *(*self.head).get_node().prev.get() = tail;
        }
    }

    /// # Safety
    ///
    /// The caller must ensure that `ptr` is a valid, aligned, and dereferenceable pointer
    /// to an initialized `P::Target` object that is alive for `'a`.
    #[inline]
    unsafe fn get_node_ref<'a>(&self, ptr: *mut P::Target) -> &'a DoublyLinkedListNode<P::Target> {
        let _ = self;
        // SAFETY: The caller guarantees `ptr` is valid, aligned, and dereferenceable.
        unsafe { &(*ptr) }.get_node()
    }

    /// Returns true if the list is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        is_sentinel_ptr(self.head)
    }

    /// Returns a reference to the first element of the list, or `None` if it is empty.
    #[inline]
    pub fn front(&self) -> Option<&P::Target> {
        if self.is_empty() { None } else { unsafe { Some(&*self.head) } }
    }

    /// Returns a pinned mutable reference to the first element of the list, or `None` if it is
    /// empty.
    #[inline]
    pub fn front_mut(self: Pin<&mut Self>) -> Option<Pin<&mut P::Target>>
    where
        P: MutPtr,
    {
        if self.is_empty() {
            None
        } else {
            // SAFETY: `self.head` is a valid element of this list. `P: MutPtr` guarantees that
            // exclusive access to the list implies exclusive access to its elements, and the list
            // never moves its elements while they are linked.
            Some(unsafe { Pin::new_unchecked(&mut *self.head) })
        }
    }

    /// Returns a reference to the last element of the list, or `None` if it is empty.
    #[inline]
    pub fn back(&self) -> Option<&P::Target> {
        let tail = self.get_tail();
        if is_sentinel_ptr(tail) { None } else { unsafe { Some(&*tail) } }
    }

    /// Returns a pinned mutable reference to the last element of the list, or `None` if it is
    /// empty.
    #[inline]
    pub fn back_mut(self: Pin<&mut Self>) -> Option<Pin<&mut P::Target>>
    where
        P: MutPtr,
    {
        let tail = self.get_tail();
        if is_sentinel_ptr(tail) {
            None
        } else {
            // SAFETY: `tail` is a valid element of this list. `P: MutPtr` guarantees that
            // exclusive access to the list implies exclusive access to its elements, and the list
            // never moves its elements while they are linked.
            Some(unsafe { Pin::new_unchecked(&mut *tail) })
        }
    }

    /// Returns a cursor positioned at `current`, which must be either a member of this list or
    /// this list's sentinel.
    #[inline]
    fn cursor_at_ptr(self: Pin<&mut Self>, current: *mut P::Target) -> CursorMut<'_, P, Tag, S> {
        CursorMut { list: self, current }
    }

    /// Pushes an element to the front of the list.
    ///
    /// # Panics
    ///
    /// Panics if the object is already in a container.
    #[inline]
    pub fn push_front(self: Pin<&mut Self>, ptr: P)
    where
        P: ManagedPtr,
    {
        // SAFETY: `P` is a `ManagedPtr`, which guarantees that the pointer is valid and that the
        // object will outlive its reference from this list.
        unsafe { self.push_front_raw(ptr) }
    }

    /// Pushes an element to the front of the list.
    ///
    /// # Panics
    ///
    /// Panics in debug builds if the object is already in a container.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `ptr` is a valid pointer to a `T` that is not currently in any
    /// list, and that the object outlives the reference from the list. If mutable element
    /// accessors (`front_mut`, `back_mut`, `iter_mut`, `CursorMut::get_mut`, etc.) are used on a
    /// raw-pointer list, the caller must also ensure that no other references to the element are
    /// accessed while it is mutably borrowed through the list, and that the element satisfies
    /// `Pin`'s drop guarantee (it is not moved out of its memory location before being dropped,
    /// unless `P::Target: Unpin`).
    #[inline]
    pub unsafe fn push_front_raw(self: Pin<&mut Self>, ptr: P) {
        let mut cursor = self.cursor_front_mut();
        // SAFETY: `ptr` is valid and not in container (debug-asserted inside insert_before_raw).
        unsafe {
            cursor.insert_before_raw(ptr);
        }
    }

    /// Pushes an element to the back of the list.
    ///
    /// # Panics
    ///
    /// Panics if the object is already in a container.
    #[inline]
    pub fn push_back(self: Pin<&mut Self>, ptr: P)
    where
        P: ManagedPtr,
    {
        // SAFETY: `P` is a `ManagedPtr`, which guarantees that the pointer is valid and that the
        // object will outlive its reference from this list.
        unsafe { self.push_back_raw(ptr) }
    }

    /// Pushes an element to the back of the list.
    ///
    /// # Panics
    ///
    /// Panics in debug builds if the object is already in a container.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `ptr` is a valid pointer to an object that is not
    /// currently in any list. If mutable element accessors (`front_mut`, `back_mut`, `iter_mut`,
    /// `CursorMut::get_mut`, etc.) are used on a raw-pointer list, the caller must also ensure
    /// that no other references to the element are accessed while it is mutably borrowed through
    /// the list, and that the element satisfies `Pin`'s drop guarantee (it is not moved out of its
    /// memory location before being dropped, unless `P::Target: Unpin`).
    #[inline]
    pub unsafe fn push_back_raw(self: Pin<&mut Self>, ptr: P) {
        let mut cursor = self.cursor_back_mut();
        // SAFETY: `ptr` is valid and not in container.
        unsafe {
            cursor.insert_before_raw(ptr);
        }
    }

    /// Removes and returns the first element of the list, or `None` if it is empty.
    #[inline]
    pub fn pop_front(self: Pin<&mut Self>) -> Option<P> {
        self.cursor_front_mut().erase()
    }

    /// Removes and returns the last element of the list, or `None` if it is empty.
    #[inline]
    pub fn pop_back(self: Pin<&mut Self>) -> Option<P> {
        let tail = self.get_tail();
        self.cursor_at_ptr(tail).erase()
    }

    /// Removes all elements from the list.
    pub fn clear(mut self: Pin<&mut Self>) {
        while self.as_mut().pop_front().is_some() {}
    }

    /// A special clear operation that resets the internal container structure in O(1) time,
    /// leaving the node state of the current elements unmodified.
    ///
    /// Only usable with containers of unmanaged pointers (`!P::IS_MANAGED`); attempting to call
    /// this on a container of managed pointers (`UniquePtr` or `RefPtr`) fails to compile.
    ///
    /// While this can be useful in special cases (such as resetting a free list for a pool or slab
    /// allocator during destruction), you normally do not want this behavior. Think carefully
    /// before calling this!
    ///
    /// # Safety
    ///
    /// Unlike C++, `DoublyLinkedListNode` does not have an `AllowClearUnsafe` node option; this
    /// safety contract replaces the C++ node-option `static_assert`:
    /// - After this call, every former element retains stale `next` and `prev` links and still
    ///   reports `in_container() == true`.
    /// - The caller must ensure that no former element's node is used again by any list or cursor
    ///   operation (such as insertion, `erase`, `cursor_at`, `replace_raw`, or
    ///   `remove_from_container`) until its memory is freed or the node is reinitialized.
    /// - The caller must ensure that no former element's node is dropped while its `Drop`
    ///   implementation's `!in_container()` debug assertion would fire (for example, by keeping
    ///   the node in `ManuallyDrop`, freeing the backing memory without running `Drop`, or
    ///   manually resetting the node state first).
    #[inline]
    pub unsafe fn clear_unsafe(self: Pin<&mut Self>) {
        const {
            assert!(
                !P::IS_MANAGED,
                "clear_unsafe is not allowed for containers of managed pointers",
            );
        }

        let sentinel = self.get_sentinel();
        let this = self.project();
        *this.head = sentinel;

        // Update our count bookkeeping.
        if S::IS_TRACKING {
            this.size.set(0);
        }
    }

    /// Erases the given element from the list. Returns the erased element.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `obj` is a valid reference to an object that is
    /// currently in this list instance.
    #[inline]
    pub unsafe fn erase(self: Pin<&mut Self>, obj: &P::Target) -> Option<P> {
        let ptr = obj as *const P::Target as *mut P::Target;
        let node = obj.get_node();

        if !node.in_container() {
            return None;
        }

        self.cursor_at_ptr(ptr).erase()
    }

    /// Replaces the given element with `replacement`. Returns the replaced element.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `obj` is a valid reference to an object that is
    /// currently in this list instance, and `replacement` is not in any list. If mutable element
    /// accessors (`front_mut`, `back_mut`, `iter_mut`, `CursorMut::get_mut`, etc.) are used on a
    /// raw-pointer list, the caller must also ensure that no other references to `replacement` are
    /// accessed while it is mutably borrowed through the list, and that `replacement` satisfies
    /// `Pin`'s drop guarantee (it is not moved out of its memory location before being dropped,
    /// unless `P::Target: Unpin`).
    #[inline]
    pub unsafe fn replace_raw(self: Pin<&mut Self>, obj: &P::Target, replacement: P) -> Option<P> {
        let ptr = obj as *const P::Target as *mut P::Target;
        let node = obj.get_node();

        if !node.in_container() {
            return None;
        }

        let mut cursor = self.cursor_at_ptr(ptr);
        // SAFETY: `replacement` is not in any list, and cursor is positioned at a valid element.
        unsafe { cursor.replace_raw(replacement) }
    }

    /// Finds the first element matching the predicate, removes it from the list,
    /// and returns it. Returns `None` if no element matches.
    #[inline]
    pub fn erase_if<F>(self: Pin<&mut Self>, mut f: F) -> Option<P>
    where
        F: FnMut(&P::Target) -> bool,
    {
        let mut cursor = self.cursor_front_mut();
        while let Some(item) = cursor.get() {
            if f(item) {
                return cursor.erase();
            } else {
                cursor.move_next();
            }
        }
        None
    }

    /// Finds the first element that satisfies the predicate.
    #[inline]
    pub fn find_if<F>(&self, mut f: F) -> Option<&P::Target>
    where
        F: FnMut(&P::Target) -> bool,
    {
        self.iter().find(|&x| f(x))
    }

    /// Returns a cursor positioned at the front of the list.
    #[inline]
    pub fn cursor_front_mut(self: Pin<&mut Self>) -> CursorMut<'_, P, Tag, S> {
        let head = self.head;
        self.cursor_at_ptr(head)
    }

    /// Returns a cursor positioned at the back (end sentinel) of the list.
    #[inline]
    pub fn cursor_back_mut(self: Pin<&mut Self>) -> CursorMut<'_, P, Tag, S> {
        let sentinel = self.get_sentinel();
        self.cursor_at_ptr(sentinel)
    }

    /// Returns a cursor positioned at the given element.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `obj` is a member of this list.
    /// It is undefined behavior to use the returned cursor if `obj` is not in the list,
    /// or if it is in a different list.
    #[inline]
    pub unsafe fn cursor_at(self: Pin<&mut Self>, obj: &P::Target) -> CursorMut<'_, P, Tag, S> {
        debug_assert!(obj.get_node().in_container(), "Object must be in a container");
        self.cursor_at_ptr(obj as *const P::Target as *mut P::Target)
    }

    /// Splices all elements from `other` onto the end of `self`.
    ///
    /// Upon completion, `other` is left empty.
    ///
    /// This operation is O(1).
    #[inline]
    pub fn splice(self: Pin<&mut Self>, other: Pin<&mut DoublyLinkedList<P, Tag, S>>) {
        self.cursor_back_mut().splice(other);
    }

    #[inline]
    pub fn iter(&self) -> Iterator<'_, P, Tag> {
        Iterator::new(self)
    }

    /// Returns a mutable bidirectional iterator over the elements of the list.
    ///
    /// Elements are yielded as `Pin<&mut P::Target>` since they must not be moved while linked.
    #[inline]
    pub fn iter_mut(self: Pin<&mut Self>) -> IteratorMut<'_, P, Tag>
    where
        P: MutPtr,
    {
        IteratorMut::new(self)
    }

    /// Returns a unidirectional forward iterator over the elements of the list.
    #[inline]
    pub fn forward_iter(&self) -> ForwardIterator<'_, P, Tag> {
        ForwardIterator::new(self.head)
    }

    /// Returns a unidirectional forward mutable iterator over the elements of the list.
    ///
    /// Elements are yielded as `Pin<&mut P::Target>` since they must not be moved while linked.
    #[inline]
    pub fn forward_iter_mut(self: Pin<&mut Self>) -> ForwardIteratorMut<'_, P, Tag>
    where
        P: MutPtr,
    {
        ForwardIteratorMut::new(self.head)
    }

    /// Returns a unidirectional reverse iterator over the elements of the list.
    #[inline]
    pub fn reverse_iter(&self) -> ReverseIterator<'_, P, Tag> {
        ReverseIterator::new(self.get_tail())
    }

    /// Returns a unidirectional reverse mutable iterator over the elements of the list.
    ///
    /// Elements are yielded as `Pin<&mut P::Target>` since they must not be moved while linked.
    #[inline]
    pub fn reverse_iter_mut(self: Pin<&mut Self>) -> ReverseIteratorMut<'_, P, Tag>
    where
        P: MutPtr,
    {
        ReverseIteratorMut::new(self.get_tail(), self.head)
    }
}

impl<P, Tag> DoublyLinkedList<P, Tag, TrackingSize>
where
    P: PtrTraits,
    P::Target: DoublyLinkedListContainable<P::Target, Tag>,
{
    /// Returns the number of elements in the list.
    #[inline]
    pub fn len(&self) -> usize {
        self.size.get()
    }
}

#[pinned_drop]
impl<P, Tag, S> PinnedDrop for DoublyLinkedList<P, Tag, S>
where
    P: PtrTraits,
    P::Target: DoublyLinkedListContainable<P::Target, Tag>,
    S: SizeTracker,
{
    fn drop(self: Pin<&mut Self>) {
        if P::IS_MANAGED {
            self.clear();
        } else {
            debug_assert!(self.is_empty(), "List must be empty on destruction");
            if S::IS_TRACKING {
                debug_assert_eq!(self.size.get(), 0, "Size must be zero on destruction");
            }
        }
    }
}

/// A cursor that can be used to iterate and modify a `DoublyLinkedList`.
///
/// The cursor holds the list as `Pin<&mut DoublyLinkedList>` and mutates the list's `head` and
/// `size` fields through pin projection, so an unpinned `&mut DoublyLinkedList` is never
/// materialized.
pub struct CursorMut<'a, P, Tag = DefaultObjectTag, S = NonTrackingSize>
where
    P: PtrTraits,
    P::Target: DoublyLinkedListContainable<P::Target, Tag>,
    S: SizeTracker,
{
    list: Pin<&'a mut DoublyLinkedList<P, Tag, S>>,
    current: *mut P::Target,
}

impl<'a, P, Tag, S> CursorMut<'a, P, Tag, S>
where
    P: PtrTraits,
    P::Target: DoublyLinkedListContainable<P::Target, Tag>,
    S: SizeTracker,
{
    #[inline]
    pub fn get(&self) -> Option<&P::Target> {
        if is_sentinel_ptr(self.current) { None } else { unsafe { Some(&*self.current) } }
    }

    /// Returns a pinned mutable reference to the element at the current position, or `None` if
    /// the cursor is positioned at the end sentinel.
    #[inline]
    pub fn get_mut(&mut self) -> Option<Pin<&mut P::Target>>
    where
        P: MutPtr,
    {
        if is_sentinel_ptr(self.current) {
            None
        } else {
            // SAFETY: `self.current` is a valid element of the list (not the sentinel). `P: MutPtr`
            // guarantees that exclusive access to the list (held by this cursor) implies exclusive
            // access to its elements, and the list never moves its elements while they are linked.
            Some(unsafe { Pin::new_unchecked(&mut *self.current) })
        }
    }

    #[inline]
    pub fn move_next(&mut self) {
        if !is_sentinel_ptr(self.current) {
            // SAFETY: `self.current` is valid current node (not sentinel).
            let node = unsafe { self.list.get_node_ref(self.current) };
            self.current = node.get_next();
        }
    }

    #[inline]
    pub fn move_prev(&mut self) {
        if !is_sentinel_ptr(self.current) {
            // SAFETY: `self.current` is valid current node (not sentinel).
            let node = unsafe { self.list.get_node_ref(self.current) };
            let prev = node.get_prev();
            if self.current == self.list.head {
                self.current = self.list.get_sentinel(); // Move to end.
            } else {
                self.current = prev;
            }
        } else {
            // If we are at end (sentinel), moving prev should take us to tail.
            self.current = self.list.get_tail();
        }
    }

    /// Inserts a new element after the current position.
    ///
    /// # Panics
    ///
    /// Panics if the object is already in a container, or if the cursor is positioned
    /// at the end sentinel.
    #[inline]
    pub fn insert_after(&mut self, ptr: P)
    where
        P: ManagedPtr,
    {
        debug_assert!(!ptr.get_ref().get_node().in_container());
        // SAFETY: `P` is a `ManagedPtr`, which guarantees that the pointer is valid and that the
        // object will outlive its reference from this list. `self.current` is checked to not be
        // a sentinel.
        unsafe { self.insert_after_raw(ptr) }
    }

    /// Inserts a new element after the current position.
    ///
    /// # Panics
    ///
    /// Panics if the cursor is positioned at the end sentinel, or in debug builds if the object is
    /// already in a container.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `ptr` is a valid pointer to a `T` that is not currently in any
    /// list, and that the object outlives the reference from the list. If mutable element
    /// accessors (`front_mut`, `back_mut`, `iter_mut`, `CursorMut::get_mut`, etc.) are used on a
    /// raw-pointer list, the caller must also ensure that no other references to the element are
    /// accessed while it is mutably borrowed through the list, and that the element satisfies
    /// `Pin`'s drop guarantee (it is not moved out of its memory location before being dropped,
    /// unless `P::Target: Unpin`).
    #[inline]
    pub unsafe fn insert_after_raw(&mut self, ptr: P) {
        debug_assert!(!is_sentinel_ptr(self.current), "Cannot insert after end sentinel");
        let raw = P::into_raw(ptr);
        // SAFETY: `raw` is valid.
        let node = unsafe { self.list.get_node_ref(raw) };
        debug_assert!(!node.in_container());

        // SAFETY: `self.current` is valid current node (not sentinel).
        let current_node = unsafe { self.list.get_node_ref(self.current) };
        let next = current_node.get_next();

        let current_save = self.current;
        self.current = next;
        // SAFETY: `raw` is a single node, and we are inserting it before `next`
        // (which is equivalent to inserting after `current_save`).
        unsafe {
            self.insert_chain_before(raw, raw, 1);
        }
        self.current = current_save;
    }

    /// Replaces the element at the current position with `replacement`. Returns the replaced
    /// element.
    ///
    /// # Panics
    ///
    /// Panics if the object is already in a container.
    #[inline]
    pub fn replace(&mut self, replacement: P) -> Option<P>
    where
        P: ManagedPtr,
    {
        // SAFETY: `P` is a `ManagedPtr`, which guarantees that the pointer is valid and that the
        // object will outlive its reference from this list.
        unsafe { self.replace_raw(replacement) }
    }

    /// Replaces the element at the current position with `replacement`. Returns the replaced
    /// element.
    ///
    /// # Panics
    ///
    /// Panics in debug builds if the object is already in a container.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `replacement` is a valid pointer to a `T` that is not currently
    /// in any list, and that the object outlives the reference from the list. If mutable element
    /// accessors (`front_mut`, `back_mut`, `iter_mut`, `CursorMut::get_mut`, etc.) are used on a
    /// raw-pointer list, the caller must also ensure that no other references to `replacement` are
    /// accessed while it is mutably borrowed through the list, and that `replacement` satisfies
    /// `Pin`'s drop guarantee (it is not moved out of its memory location before being dropped,
    /// unless `P::Target: Unpin`).
    #[inline]
    pub unsafe fn replace_raw(&mut self, replacement: P) -> Option<P> {
        if is_sentinel_ptr(self.current) {
            return None;
        }
        // SAFETY: `replacement` is not in any list, and we are inserting it before a valid cursor
        // position.
        unsafe {
            self.insert_before_raw(replacement);
        }
        self.erase()
    }

    /// Inserts a new element before the current position.
    ///
    /// # Panics
    ///
    /// Panics if the object is already in a container.
    #[inline]
    pub fn insert_before(&mut self, ptr: P)
    where
        P: ManagedPtr,
    {
        debug_assert!(!ptr.get_ref().get_node().in_container());
        // SAFETY: `P` is a `ManagedPtr`, which guarantees that the pointer is valid and that the
        // object will outlive its reference from this list.
        unsafe { self.insert_before_raw(ptr) }
    }

    /// Inserts a new element before the current position.
    ///
    /// # Panics
    ///
    /// Panics in debug builds if the object is already in a container.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `ptr` is a valid pointer to a `T` that is not currently in any
    /// list, and that the object outlives the reference from the list. If mutable element
    /// accessors (`front_mut`, `back_mut`, `iter_mut`, `CursorMut::get_mut`, etc.) are used on a
    /// raw-pointer list, the caller must also ensure that no other references to the element are
    /// accessed while it is mutably borrowed through the list, and that the element satisfies
    /// `Pin`'s drop guarantee (it is not moved out of its memory location before being dropped,
    /// unless `P::Target: Unpin`).
    #[inline]
    pub unsafe fn insert_before_raw(&mut self, ptr: P) {
        let raw = P::into_raw(ptr);
        // SAFETY: `raw` is valid.
        let node = unsafe { self.list.get_node_ref(raw) };
        debug_assert!(!node.in_container());

        // SAFETY: `raw` is a single node, so it is a valid chain of 1 element.
        unsafe {
            self.insert_chain_before(raw, raw, 1);
        }
    }

    /// Private helper to insert a chain of nodes before the current position.
    ///
    /// # Safety
    ///
    /// The caller must ensure:
    /// - `chain_head` and `chain_tail` are valid pointers to elements.
    /// - They form a valid doubly linked chain.
    /// - The chain is not empty.
    /// - The elements in the chain are NOT currently in any list.
    /// - `count` is the exact number of elements in the chain.
    #[inline]
    unsafe fn insert_chain_before(
        &mut self,
        chain_head: *mut P::Target,
        chain_tail: *mut P::Target,
        count: usize,
    ) {
        let head = self.list.head;
        let before = self.current;
        // SAFETY: `chain_tail` is valid from caller.
        let chain_tail_node = unsafe { self.list.get_node_ref(chain_tail) };
        // SAFETY: `chain_head` is valid from caller.
        let chain_head_node = unsafe { self.list.get_node_ref(chain_head) };

        if is_sentinel_ptr(head) {
            chain_tail_node.set_next(before);
            chain_head_node.set_prev(chain_tail);
            *self.list.as_mut().project().head = chain_head;
        } else if before == head {
            // SAFETY: `head` is not a sentinel, so it is a valid node.
            let head_node = unsafe { self.list.get_node_ref(head) };
            let tail = head_node.get_prev();
            chain_tail_node.set_next(before);
            chain_head_node.set_prev(tail);
            head_node.set_prev(chain_tail);
            *self.list.as_mut().project().head = chain_head;
        } else if is_sentinel_ptr(before) {
            // SAFETY: `head` is not a sentinel, so it is a valid node.
            let head_node = unsafe { self.list.get_node_ref(head) };
            let tail = head_node.get_prev();
            // SAFETY: `tail` is a valid node in a non-empty list.
            let tail_node = unsafe { self.list.get_node_ref(tail) };
            chain_tail_node.set_next(before);
            chain_head_node.set_prev(tail);
            tail_node.set_next(chain_head);
            head_node.set_prev(chain_tail);
        } else {
            // SAFETY: `before` is not a sentinel, so it is a valid node.
            let current_node = unsafe { self.list.get_node_ref(before) };
            let prev = current_node.get_prev();
            // SAFETY: `prev` is a valid predecessor node.
            let prev_node = unsafe { self.list.get_node_ref(prev) };
            chain_tail_node.set_next(before);
            chain_head_node.set_prev(prev);
            prev_node.set_next(chain_head);
            current_node.set_prev(chain_tail);
        }

        if S::IS_TRACKING {
            let new_size = self.list.size.get() + count;
            self.list.as_mut().project().size.set(new_size);
        }
    }

    /// Splices the elements of `other` into the list at the current cursor position.
    ///
    /// All elements from `other` are moved into `self.list` and inserted immediately
    /// *before* the element currently pointed to by the cursor.
    ///
    /// - If the cursor is positioned at a valid element, `other` is inserted before it.
    /// - If the cursor is positioned at the end sentinel (i.e., `cursor.get()` returns `None`),
    ///   `other` is appended to the end of the list (after the current tail).
    /// - If the list is empty, `other` becomes the new content of the list.
    ///
    /// Upon completion, `other` is left empty.
    ///
    /// This operation is O(1).
    pub fn splice(&mut self, other: Pin<&mut DoublyLinkedList<P, Tag, S>>) {
        if other.is_empty() {
            return;
        }

        let other_head = other.head;
        let other_tail = other.get_tail();
        let other_sentinel = other.get_sentinel();
        let count = if S::IS_TRACKING { other.size.get() } else { 0 };

        // SAFETY: We are moving elements from `other` which is a valid list,
        // so they are valid and not in any other list.
        unsafe {
            self.insert_chain_before(other_head, other_tail, count);
        }

        let other = other.project();
        *other.head = other_sentinel;
        if S::IS_TRACKING {
            other.size.set(0);
        }
    }

    /// Splits the list immediately after the current cursor position, moving all elements
    /// after the cursor into `dest`, inserted before `dest`'s current position.
    ///
    /// The current list retains all elements up to and including the current element.
    ///
    /// If the cursor is positioned at the tail of the list, no elements are moved.
    ///
    /// # Panics
    ///
    /// Panics if the cursor is positioned at the end sentinel.
    ///
    /// This operation is O(1) for lists with non-tracking size.
    pub fn split_after(&mut self, dest: &mut CursorMut<'_, P, Tag, S>) {
        debug_assert!(!is_sentinel_ptr(self.current), "Cannot split after end sentinel");

        // SAFETY: `self.current` is not sentinel.
        let curr_node = unsafe { self.list.get_node_ref(self.current) };
        let next_ptr = curr_node.get_next();

        // If cursor is at the tail, there are no elements after it.
        if is_sentinel_ptr(next_ptr) {
            return;
        }

        let chain_head = next_ptr;
        let chain_tail = self.list.get_tail();

        let mut count = 0;
        if S::IS_TRACKING {
            let mut p = chain_head;
            let sentinel = self.list.get_sentinel();
            while p != sentinel {
                count += 1;
                // SAFETY: `p` is in the chain.
                p = unsafe { self.list.get_node_ref(p) }.get_next();
            }
            let new_size = self.list.size.get() - count;
            self.list.as_mut().project().size.set(new_size);
        }

        // Update self.list: `self.current` becomes the new tail.
        curr_node.set_next(self.list.get_sentinel());
        unsafe {
            self.list.set_tail(self.current);
        }

        // Insert the extracted chain before dest's current position.
        // SAFETY: The chain [chain_head .. chain_tail] is valid, detached from self.list,
        // and count is exact.
        unsafe {
            dest.insert_chain_before(chain_head, chain_tail, count);
        }
    }

    /// Splits the list immediately before the current cursor position, moving all elements
    /// before the cursor into `dest`, inserted before `dest`'s current position.
    ///
    /// The current list retains all elements from the current position to the end of the list.
    ///
    /// - If the cursor is positioned at the head of the list, no elements are moved.
    /// - If the cursor is positioned at the end sentinel, all elements of the list are moved
    ///   into `dest`, leaving this list empty.
    /// - If the cursor is positioned at an element between head and sentinel, all elements from
    ///   head up to (and excluding) the current element are moved into `dest`.
    ///
    /// Upon completion, the cursor remains positioned at the same element (which is now the new
    /// head of this list, or the sentinel).
    ///
    /// This operation is O(1) for lists with non-tracking size.
    pub fn split_before(&mut self, dest: &mut CursorMut<'_, P, Tag, S>) {
        // Case 1: Cursor is at head. No elements before head.
        if self.current == self.list.head {
            return;
        }

        // Case 2: Cursor is at sentinel. All elements in list are before sentinel.
        if is_sentinel_ptr(self.current) {
            if self.list.is_empty() {
                return;
            }
            let chain_head = self.list.head;
            let chain_tail = self.list.get_tail();
            let sentinel = self.list.get_sentinel();
            let list = self.list.as_mut().project();
            let count = if S::IS_TRACKING {
                let c = list.size.get();
                list.size.set(0);
                c
            } else {
                0
            };

            *list.head = sentinel;

            // SAFETY: The chain [chain_head .. chain_tail] is valid, detached from self.list,
            // and count is exact.
            unsafe {
                dest.insert_chain_before(chain_head, chain_tail, count);
            }
            return;
        }

        // Case 3: Cursor is at an interior element B.
        // Elements before B are [head .. B.prev].
        let b = self.current;
        let b_node = unsafe { self.list.get_node_ref(b) };
        let chain_head = self.list.head;
        let chain_tail = b_node.get_prev();
        let old_tail = self.list.get_tail();

        let mut count = 0;
        if S::IS_TRACKING {
            let mut p = chain_head;
            while p != b {
                count += 1;
                // SAFETY: `p` is in the chain.
                p = unsafe { self.list.get_node_ref(p) }.get_next();
            }
            let new_size = self.list.size.get() - count;
            self.list.as_mut().project().size.set(new_size);
        }

        // Update self.list: B becomes the new head, and its prev points to old_tail.
        *self.list.as_mut().project().head = b;
        b_node.set_prev(old_tail);

        // Insert the extracted chain before dest's current position.
        // SAFETY: The chain [chain_head .. chain_tail] is valid, detached from self.list,
        // and count is exact.
        unsafe {
            dest.insert_chain_before(chain_head, chain_tail, count);
        }
    }

    #[inline]
    pub fn erase(&mut self) -> Option<P> {
        let ptr = self.current;
        if is_sentinel_ptr(ptr) {
            return None;
        }
        let head = self.list.head;
        // SAFETY: `ptr` is valid current node.
        let node = unsafe { self.list.get_node_ref(ptr) };
        let next = node.get_next();
        let prev = node.get_prev();

        self.list.as_mut().project().size.decrement();

        let tgt_prev_node = if is_sentinel_ptr(next) { head } else { next };
        // SAFETY: Both `head` and `next` (when not sentinel) are valid nodes in a non-empty list.
        unsafe { self.list.get_node_ref(tgt_prev_node) }.set_prev(prev);

        if ptr == head {
            *self.list.as_mut().project().head = next;
        } else {
            // SAFETY: `prev` is a valid predecessor node.
            unsafe { self.list.get_node_ref(prev) }.set_next(next);
        }

        node.set_prev(core::ptr::null_mut());
        node.set_next(core::ptr::null_mut());

        self.current = next;
        // SAFETY: `ptr` was popped, safe to reconstruct.
        Some(unsafe { P::from_raw(ptr) })
    }
}

/// An iterator over the elements of a `DoublyLinkedList`.
pub struct Iterator<'a, P, Tag = DefaultObjectTag>
where
    P: PtrTraits,
    P::Target: DoublyLinkedListContainable<P::Target, Tag>,
{
    front: ForwardIterator<'a, P, Tag>,
    back: ReverseIterator<'a, P, Tag>,
}

impl<'a, P, Tag> Iterator<'a, P, Tag>
where
    P: PtrTraits,
    P::Target: DoublyLinkedListContainable<P::Target, Tag>,
{
    fn new<S: SizeTracker>(list: &'a DoublyLinkedList<P, Tag, S>) -> Self {
        if list.is_empty() {
            Self {
                front: ForwardIterator::new(crate::make_sentinel_null()),
                back: ReverseIterator::new(crate::make_sentinel_null()),
            }
        } else {
            Self {
                front: ForwardIterator::new(list.head),
                back: ReverseIterator::new(list.get_tail()),
            }
        }
    }
}

impl<'a, P, Tag> core::iter::Iterator for Iterator<'a, P, Tag>
where
    P: PtrTraits,
    P::Target: DoublyLinkedListContainable<P::Target, Tag>,
{
    type Item = &'a P::Target;

    fn next(&mut self) -> Option<Self::Item> {
        let met = self.front.current == self.back.current;
        let item = self.front.next();
        if item.is_some() && met {
            self.front.current = crate::make_sentinel_null();
            self.back.current = crate::make_sentinel_null();
        }
        item
    }
}

impl<'a, P, Tag> core::iter::DoubleEndedIterator for Iterator<'a, P, Tag>
where
    P: PtrTraits,
    P::Target: DoublyLinkedListContainable<P::Target, Tag>,
{
    fn next_back(&mut self) -> Option<Self::Item> {
        let met = self.front.current == self.back.current;
        let item = self.back.next();
        if item.is_some() && met {
            self.front.current = crate::make_sentinel_null();
            self.back.current = crate::make_sentinel_null();
        }
        item
    }
}

/// A unidirectional forward iterator over the elements of a `DoublyLinkedList`.
pub struct ForwardIterator<'a, P, Tag = DefaultObjectTag>
where
    P: PtrTraits,
    P::Target: DoublyLinkedListContainable<P::Target, Tag>,
{
    current: *mut P::Target,
    _phantom: core::marker::PhantomData<&'a (P, Tag)>,
}

impl<'a, P, Tag> ForwardIterator<'a, P, Tag>
where
    P: PtrTraits,
    P::Target: DoublyLinkedListContainable<P::Target, Tag>,
{
    fn new(current: *mut P::Target) -> Self {
        Self { current, _phantom: core::marker::PhantomData }
    }
}

impl<'a, P, Tag> core::iter::Iterator for ForwardIterator<'a, P, Tag>
where
    P: PtrTraits,
    P::Target: DoublyLinkedListContainable<P::Target, Tag>,
{
    type Item = &'a P::Target;

    fn next(&mut self) -> Option<Self::Item> {
        if is_sentinel_ptr(self.current) {
            None
        } else {
            // SAFETY: `self.current` is not a sentinel, so it is a valid, aligned pointer to an
            // element.  The list is guaranteed to be immutable for the lifetime `'a` of the
            // iterator.
            let current = unsafe { &*self.current };
            self.current = current.get_node().get_next();
            Some(current)
        }
    }
}

/// A unidirectional reverse iterator over the elements of a `DoublyLinkedList`.
pub struct ReverseIterator<'a, P, Tag = DefaultObjectTag>
where
    P: PtrTraits,
    P::Target: DoublyLinkedListContainable<P::Target, Tag>,
{
    current: *mut P::Target,
    _phantom: core::marker::PhantomData<&'a (P, Tag)>,
}

impl<'a, P, Tag> ReverseIterator<'a, P, Tag>
where
    P: PtrTraits,
    P::Target: DoublyLinkedListContainable<P::Target, Tag>,
{
    fn new(current: *mut P::Target) -> Self {
        Self { current, _phantom: core::marker::PhantomData }
    }
}

impl<'a, P, Tag> core::iter::Iterator for ReverseIterator<'a, P, Tag>
where
    P: PtrTraits,
    P::Target: DoublyLinkedListContainable<P::Target, Tag>,
{
    type Item = &'a P::Target;

    fn next(&mut self) -> Option<Self::Item> {
        if is_sentinel_ptr(self.current) {
            None
        } else {
            // SAFETY: `self.current` is not a sentinel, so it is a valid, aligned pointer to an
            // element.  The list is guaranteed to be immutable for the lifetime `'a` of the
            // iterator.
            let current = unsafe { &*self.current };
            let prev = current.get_node().get_prev();

            // SAFETY: `prev` must be a valid pointer because `current` is in the list.  In a
            // circular doubly linked list, prev is never null.
            let prev_node = unsafe { &*prev }.get_node();
            if is_sentinel_ptr(prev_node.get_next()) {
                // We have looped around the head and landed on the tail.
                // Set current to the sentinel to terminate iteration.
                self.current = prev_node.get_next();
            } else {
                self.current = prev;
            }
            Some(current)
        }
    }
}

/// A mutable bidirectional iterator over the elements of a `DoublyLinkedList`.
///
/// Elements are yielded as `Pin<&mut P::Target>` since they must not be moved while linked.
pub struct IteratorMut<'a, P, Tag = DefaultObjectTag>
where
    P: MutPtr,
    P::Target: DoublyLinkedListContainable<P::Target, Tag>,
{
    front: ForwardIteratorMut<'a, P, Tag>,
    back: ReverseIteratorMut<'a, P, Tag>,
}

impl<'a, P, Tag> IteratorMut<'a, P, Tag>
where
    P: MutPtr,
    P::Target: DoublyLinkedListContainable<P::Target, Tag>,
{
    fn new<S: SizeTracker>(list: Pin<&'a mut DoublyLinkedList<P, Tag, S>>) -> Self {
        if list.is_empty() {
            Self {
                front: ForwardIteratorMut::new(crate::make_sentinel_null()),
                back: ReverseIteratorMut::new(
                    crate::make_sentinel_null(),
                    crate::make_sentinel_null(),
                ),
            }
        } else {
            let tail = list.get_tail();
            Self {
                front: ForwardIteratorMut::new(list.head),
                back: ReverseIteratorMut::new(tail, list.head),
            }
        }
    }
}

impl<'a, P, Tag> core::iter::Iterator for IteratorMut<'a, P, Tag>
where
    P: MutPtr,
    P::Target: DoublyLinkedListContainable<P::Target, Tag>,
{
    type Item = Pin<&'a mut P::Target>;

    fn next(&mut self) -> Option<Self::Item> {
        let met = self.front.current == self.back.current;
        let item = self.front.next();
        if item.is_some() && met {
            self.front.current = crate::make_sentinel_null();
            self.back.current = crate::make_sentinel_null();
        }
        item
    }
}

impl<'a, P, Tag> core::iter::DoubleEndedIterator for IteratorMut<'a, P, Tag>
where
    P: MutPtr,
    P::Target: DoublyLinkedListContainable<P::Target, Tag>,
{
    fn next_back(&mut self) -> Option<Self::Item> {
        let met = self.front.current == self.back.current;
        let item = self.back.next();
        if item.is_some() && met {
            self.front.current = crate::make_sentinel_null();
            self.back.current = crate::make_sentinel_null();
        }
        item
    }
}

/// A unidirectional forward mutable iterator over the elements of a `DoublyLinkedList`.
///
/// Elements are yielded as `Pin<&mut P::Target>` since they must not be moved while linked.
pub struct ForwardIteratorMut<'a, P, Tag = DefaultObjectTag>
where
    P: MutPtr,
    P::Target: DoublyLinkedListContainable<P::Target, Tag>,
{
    current: *mut P::Target,
    _phantom: core::marker::PhantomData<&'a mut (P, Tag)>,
}

impl<'a, P, Tag> ForwardIteratorMut<'a, P, Tag>
where
    P: MutPtr,
    P::Target: DoublyLinkedListContainable<P::Target, Tag>,
{
    fn new(current: *mut P::Target) -> Self {
        Self { current, _phantom: core::marker::PhantomData }
    }
}

impl<'a, P, Tag> core::iter::Iterator for ForwardIteratorMut<'a, P, Tag>
where
    P: MutPtr,
    P::Target: DoublyLinkedListContainable<P::Target, Tag>,
{
    type Item = Pin<&'a mut P::Target>;

    fn next(&mut self) -> Option<Self::Item> {
        if is_sentinel_ptr(self.current) {
            None
        } else {
            let current_ptr = self.current;
            // SAFETY: `current_ptr` is not a sentinel, so it is a valid, aligned pointer to an
            // element. No mutable reference to it is live yet.
            self.current = unsafe { &*current_ptr }.get_node().get_next();
            // SAFETY: `current_ptr` is a valid element. The list is exclusively borrowed for
            // lifetime `'a`, `P: MutPtr` guarantees that this implies exclusive access to its
            // elements, and each element in the intrusive list is distinct, so yielding
            // `&'a mut P::Target` one-by-one is sound. The list never moves its elements while
            // they are linked.
            Some(unsafe { Pin::new_unchecked(&mut *current_ptr) })
        }
    }
}

/// A unidirectional reverse mutable iterator over the elements of a `DoublyLinkedList`.
///
/// Elements are yielded as `Pin<&mut P::Target>` since they must not be moved while linked.
pub struct ReverseIteratorMut<'a, P, Tag = DefaultObjectTag>
where
    P: MutPtr,
    P::Target: DoublyLinkedListContainable<P::Target, Tag>,
{
    current: *mut P::Target,
    head: *mut P::Target,
    _phantom: core::marker::PhantomData<&'a mut (P, Tag)>,
}

impl<'a, P, Tag> ReverseIteratorMut<'a, P, Tag>
where
    P: MutPtr,
    P::Target: DoublyLinkedListContainable<P::Target, Tag>,
{
    fn new(current: *mut P::Target, head: *mut P::Target) -> Self {
        Self { current, head, _phantom: core::marker::PhantomData }
    }
}

impl<'a, P, Tag> core::iter::Iterator for ReverseIteratorMut<'a, P, Tag>
where
    P: MutPtr,
    P::Target: DoublyLinkedListContainable<P::Target, Tag>,
{
    type Item = Pin<&'a mut P::Target>;

    fn next(&mut self) -> Option<Self::Item> {
        if is_sentinel_ptr(self.current) {
            None
        } else {
            let current_ptr = self.current;
            if current_ptr == self.head {
                self.current = crate::make_sentinel_null();
            } else {
                // SAFETY: `current_ptr` is not a sentinel, so it is a valid, aligned pointer to an
                // element. No mutable reference to it is live yet.
                self.current = unsafe { &*current_ptr }.get_node().get_prev();
            }
            // SAFETY: `current_ptr` is a valid element. The list is exclusively borrowed for
            // lifetime `'a`, `P: MutPtr` guarantees that this implies exclusive access to its
            // elements, and each element in the intrusive list is distinct, so yielding
            // `&'a mut P::Target` one-by-one is sound. The list never moves its elements while
            // they are linked.
            Some(unsafe { Pin::new_unchecked(&mut *current_ptr) })
        }
    }
}

/// Removes an object from its container without a reference to the container.
///
/// # Safety
///
/// The caller must ensure that `obj` is currently in a valid list instance that does NOT
/// track its size (uses `NonTrackingSize`), and that no other mutable references to that
/// list are active.
#[inline]
pub unsafe fn remove_from_container<T, Tag, P>(obj: &T) -> Option<P>
where
    P: PtrTraits<Target = T>,
    T: DoublyLinkedListContainable<T, Tag>,
{
    let node = obj.get_node();
    let next = node.get_next();
    if next.is_null() {
        return None;
    }
    let prev = node.get_prev();
    debug_assert!(!prev.is_null());

    // SAFETY: Caller guarantees `obj` is in a valid untracked list and no other references to the
    // list are active.
    unsafe {
        let tgt_prev = if is_sentinel_ptr(next) {
            let list_ptr = crate::sentinel::unmake_sentinel::<
                DoublyLinkedList<P, Tag, NonTrackingSize>,
                T,
            >(next);
            (*(*list_ptr).head).get_node().prev.get()
        } else {
            (*next).get_node().prev.get()
        };

        let prev_node = (*prev).get_node();
        let prev_next = prev_node.get_next();
        let tgt_next = if is_sentinel_ptr(prev_next) {
            let list_ptr = crate::sentinel::unmake_sentinel::<
                DoublyLinkedList<P, Tag, NonTrackingSize>,
                T,
            >(prev_next);
            &raw mut (*list_ptr).head
        } else {
            prev_node.next.get()
        };

        *tgt_prev = prev;
        *tgt_next = next;

        node.set_next(core::ptr::null_mut());
        node.set_prev(core::ptr::null_mut());

        Some(P::from_raw(obj as *const T as *mut T))
    }
}

impl<P, Tag, S> core::fmt::Debug for DoublyLinkedList<P, Tag, S>
where
    P: PtrTraits,
    P::Target: DoublyLinkedListContainable<P::Target, Tag> + core::fmt::Debug,
    S: SizeTracker,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

#[cfg(test)]
mod tests {
    extern crate alloc;
    use super::*;
    use crate::intrusive_container_test_support::*;
    use crate::ref_ptr::RefPtr;
    use crate::unique_ptr::UniquePtr;
    use core::ffi::c_void;
    use pin_init::stack_pin_init;

    #[derive(crate::DoublyLinkedListContainable, crate::Recyclable)]
    #[pin_data]
    struct TestObject {
        value: i32,
        #[pin]
        #[dll_node]
        node: DoublyLinkedListNode<TestObject>,
    }

    impl TestObject {
        fn new(value: i32) -> Self {
            Self { value, node: DoublyLinkedListNode::new() }
        }
    }

    impl TestValue for TestObject {
        fn new(value: i32) -> Self {
            Self::new(value)
        }
    }

    ::zr::static_assert!(
        core::mem::size_of::<DoublyLinkedListNode<TestObject>>()
            == 2 * core::mem::size_of::<*mut TestObject>()
    );
    ::zr::static_assert!(
        core::mem::align_of::<DoublyLinkedListNode<TestObject>>()
            == core::mem::align_of::<*mut TestObject>()
    );

    ::zr::static_assert!(
        core::mem::size_of::<DoublyLinkedList<*mut TestObject>>()
            == core::mem::size_of::<*mut TestObject>()
    );
    ::zr::static_assert!(
        core::mem::align_of::<DoublyLinkedList<*mut TestObject>>()
            == core::mem::align_of::<*mut TestObject>()
    );

    ::zr::static_assert!(
        core::mem::size_of::<DoublyLinkedList<*mut TestObject, DefaultObjectTag, TrackingSize>>()
            == 2 * core::mem::size_of::<*mut TestObject>()
    );
    ::zr::static_assert!(
        core::mem::align_of::<DoublyLinkedList<*mut TestObject, DefaultObjectTag, TrackingSize>>()
            == core::mem::align_of::<*mut TestObject>()
    );

    ::zr::static_assert!(
        core::mem::size_of::<ForwardIterator<'_, *mut TestObject>>()
            == core::mem::size_of::<*mut TestObject>()
    );
    ::zr::static_assert!(
        core::mem::align_of::<ForwardIterator<'_, *mut TestObject>>()
            == core::mem::align_of::<*mut TestObject>()
    );

    ::zr::static_assert!(
        core::mem::size_of::<ReverseIterator<'_, *mut TestObject>>()
            == core::mem::size_of::<*mut TestObject>()
    );
    ::zr::static_assert!(
        core::mem::align_of::<ReverseIterator<'_, *mut TestObject>>()
            == core::mem::align_of::<*mut TestObject>()
    );

    #[derive(crate::DoublyLinkedListContainable, crate::Recyclable)]
    #[pin_data]
    struct UniqueTestObject {
        value: i32,
        #[pin]
        #[dll_node]
        node: DoublyLinkedListNode<UniqueTestObject>,
    }

    impl UniqueTestObject {
        fn new(value: i32) -> Self {
            Self { value, node: DoublyLinkedListNode::new() }
        }
    }

    impl TestValue for UniqueTestObject {
        fn new(value: i32) -> Self {
            Self::new(value)
        }
    }

    #[fbl::ref_counted]
    #[derive(crate::DoublyLinkedListContainable, crate::Recyclable)]
    #[repr(C)]
    pub struct RefTestObject {
        value: i32,
        #[dll_node]
        node: DoublyLinkedListNode<RefTestObject>,
    }

    impl TestValue for RefTestObject {
        fn new_ref_counted(value: i32) -> RefPtr<Self> {
            crate::make_ref_counted!(RefTestObject {
                value: value,
                node: DoublyLinkedListNode::new()
            })
            .unwrap()
        }
    }

    macro_rules! generate_list_tests {
        ($mod_name:ident, $ptr_type:ty, $factory_type:ty, $get_val:expr, $push:expr) => {
            mod $mod_name {
                use super::*;

                #[test]
                fn test_basic() {
                    let mut factory = <$factory_type>::new();
                    stack_pin_init!(let list = DoublyLinkedList::<$ptr_type>::new());
                    assert!(list.is_empty());

                    let obj1 = factory.create(1);
                    let obj2 = factory.create(2);

                    $push(list.as_mut(), obj1);
                    $push(list.as_mut(), obj2);

                    assert!(!list.is_empty());

                    let mut iter = list.iter();
                    assert_eq!(iter.next().unwrap().value, 2);
                    assert_eq!(iter.next().unwrap().value, 1);
                    assert!(iter.next().is_none());

                    list.as_mut().clear();
                    assert!(list.is_empty());
                }

                #[test]
                fn test_double_ended_iterator() {
                    let mut factory = <$factory_type>::new();
                    stack_pin_init!(let list = DoublyLinkedList::<$ptr_type>::new());
                    let obj1 = factory.create(1);
                    let obj2 = factory.create(2);
                    let obj3 = factory.create(3);

                    $push(list.as_mut(), obj1);
                    $push(list.as_mut(), obj2);
                    $push(list.as_mut(), obj3);

                    let mut iter = list.iter();
                    assert_eq!(iter.next().unwrap().value, 3);
                    assert_eq!(iter.next_back().unwrap().value, 1);
                    assert_eq!(iter.next().unwrap().value, 2);
                    assert!(iter.next().is_none());
                    assert!(iter.next_back().is_none());

                    list.as_mut().clear();
                }

                #[test]
                fn test_explicit_pops() {
                    let mut factory = <$factory_type>::new();
                    stack_pin_init!(let list = DoublyLinkedList::<$ptr_type>::new());
                    let obj1 = factory.create(1);
                    let obj2 = factory.create(2);

                    $push(list.as_mut(), obj1);
                    $push(list.as_mut(), obj2);

                    let p1 = list.as_mut().pop_front();
                    assert!(p1.is_some());
                    let p2 = list.as_mut().pop_front();
                    assert!(p2.is_some());
                    assert!(list.as_mut().pop_front().is_none());
                }

                #[test]
                fn test_cursor_move_prev() {
                    let mut factory = <$factory_type>::new();
                    stack_pin_init!(let list = DoublyLinkedList::<$ptr_type>::new());
                    let obj1 = factory.create(1);
                    let obj2 = factory.create(2);
                    let obj3 = factory.create(3);

                    $push(list.as_mut(), obj1);
                    $push(list.as_mut(), obj2);
                    $push(list.as_mut(), obj3);

                    let (a, b, c) = {
                        let mut iter = list.iter();
                        (
                            $get_val(iter.next().unwrap()),
                            $get_val(iter.next().unwrap()),
                            $get_val(iter.next().unwrap()),
                        )
                    };

                    let mut cursor = list.as_mut().cursor_front_mut();
                    assert_eq!($get_val(cursor.get().unwrap()), a);

                    cursor.move_prev();
                    assert!(cursor.get().is_none()); // Sentinel

                    cursor.move_prev();
                    assert_eq!($get_val(cursor.get().unwrap()), c);

                    cursor.move_prev();
                    assert_eq!($get_val(cursor.get().unwrap()), b);

                    cursor.move_prev();
                    assert_eq!($get_val(cursor.get().unwrap()), a);

                    list.as_mut().clear();
                }

                #[test]
                fn test_cursor_insert_after() {
                    let mut factory = <$factory_type>::new();
                    stack_pin_init!(let list = DoublyLinkedList::<$ptr_type>::new());
                    let obj1 = factory.create(1);
                    let obj2 = factory.create(2);
                    let obj3 = factory.create(3);

                    $push(list.as_mut(), obj1);

                    let mut cursor = list.as_mut().cursor_front_mut();
                    unsafe {
                        cursor.insert_after_raw(obj3);
                        cursor.insert_after_raw(obj2);
                    }

                    let mut iter = list.iter();
                    assert_eq!($get_val(iter.next().unwrap()), 1);
                    assert_eq!($get_val(iter.next().unwrap()), 2);
                    assert_eq!($get_val(iter.next().unwrap()), 3);
                    assert!(iter.next().is_none());

                    list.as_mut().clear();
                }

                #[test]
                fn test_pop_back() {
                    let mut factory = <$factory_type>::new();
                    stack_pin_init!(let list = DoublyLinkedList::<$ptr_type>::new());
                    let obj1 = factory.create(1);
                    let obj2 = factory.create(2);

                    $push(list.as_mut(), obj1);
                    $push(list.as_mut(), obj2);

                    let p1 = list.as_mut().pop_back();
                    assert!(p1.is_some());
                    let p2 = list.as_mut().pop_back();
                    assert!(p2.is_some());
                    assert!(list.as_mut().pop_back().is_none());
                }

                #[test]
                fn test_erase() {
                    let mut factory = <$factory_type>::new();
                    stack_pin_init!(let list = DoublyLinkedList::<$ptr_type>::new());
                    let obj1 = factory.create(1);
                    let obj2 = factory.create(2);
                    let obj3 = factory.create(3);

                    $push(list.as_mut(), obj1);
                    $push(list.as_mut(), obj2);
                    $push(list.as_mut(), obj3);

                    let mut cursor = list.as_mut().cursor_front_mut();
                    cursor.move_next();
                    let erased = cursor.erase();
                    assert!(erased.is_some());
                    factory.cleanup(erased.unwrap());

                    let mut iter = list.iter();
                    assert!(iter.next().is_some());
                    assert!(iter.next().is_some());
                    assert!(iter.next().is_none());

                    list.as_mut().clear();
                }

                #[test]
                fn test_erase_if() {
                    let mut factory = <$factory_type>::new();
                    stack_pin_init!(let list = DoublyLinkedList::<$ptr_type>::new());
                    let obj1 = factory.create(1);
                    let obj2 = factory.create(2);
                    let obj3 = factory.create(3);

                    $push(list.as_mut(), obj1);
                    $push(list.as_mut(), obj2);
                    $push(list.as_mut(), obj3);

                    let erased = list.as_mut().erase_if(|o| o.value == 2);
                    assert!(erased.is_some());
                    assert_eq!($get_val(erased.unwrap().get_ref()), 2);

                    let mut iter = list.iter();
                    assert_eq!($get_val(iter.next().unwrap()), 3);
                    assert_eq!($get_val(iter.next().unwrap()), 1);
                    assert!(iter.next().is_none());

                    list.as_mut().clear();
                }

                #[test]
                fn test_find_if() {
                    let mut factory = <$factory_type>::new();
                    stack_pin_init!(let list = DoublyLinkedList::<$ptr_type>::new());
                    let obj1 = factory.create(1);
                    let obj2 = factory.create(2);

                    $push(list.as_mut(), obj1);
                    $push(list.as_mut(), obj2);

                    let found = list.find_if(|o| o.value == 1);
                    assert!(found.is_some());
                    assert_eq!(found.unwrap().value, 1);

                    let found = list.find_if(|o| o.value == 3);
                    assert!(found.is_none());

                    list.as_mut().clear();
                }

                #[test]
                fn test_complete_reverse_iteration() {
                    let mut factory = <$factory_type>::new();
                    stack_pin_init!(let list = DoublyLinkedList::<$ptr_type>::new());
                    let obj1 = factory.create(1);
                    let obj2 = factory.create(2);
                    let obj3 = factory.create(3);

                    $push(list.as_mut(), obj1);
                    $push(list.as_mut(), obj2);
                    $push(list.as_mut(), obj3);

                    let (a, b, c) = {
                        let mut iter = list.iter();
                        (
                            $get_val(iter.next().unwrap()),
                            $get_val(iter.next().unwrap()),
                            $get_val(iter.next().unwrap()),
                        )
                    };

                    let mut iter = list.iter();
                    assert_eq!($get_val(iter.next_back().unwrap()), c);
                    assert_eq!($get_val(iter.next_back().unwrap()), b);
                    assert_eq!($get_val(iter.next_back().unwrap()), a);
                    assert!(iter.next_back().is_none());

                    list.as_mut().clear();
                }

                #[test]
                fn test_split_after() {
                    let mut factory = <$factory_type>::new();
                    stack_pin_init!(let list1 = DoublyLinkedList::<$ptr_type>::new());
                    stack_pin_init!(let list2 = DoublyLinkedList::<$ptr_type>::new());

                    let obj1 = factory.create(1);
                    let obj2 = factory.create(2);
                    let obj3 = factory.create(3);
                    let obj4 = factory.create(4);

                    $push(list1.as_mut(), obj4);
                    $push(list1.as_mut(), obj3);
                    $push(list1.as_mut(), obj2);
                    $push(list1.as_mut(), obj1);

                    let mut cursor = list1.as_mut().cursor_front_mut();
                    cursor.move_next(); // points to 2

                    let mut dest_cursor = list2.as_mut().cursor_back_mut();
                    cursor.split_after(&mut dest_cursor);

                    let mut iter1 = list1.iter();
                    assert_eq!($get_val(iter1.next().unwrap()), 1);
                    assert_eq!($get_val(iter1.next().unwrap()), 2);
                    assert!(iter1.next().is_none());

                    let mut iter2 = list2.iter();
                    assert_eq!($get_val(iter2.next().unwrap()), 3);
                    assert_eq!($get_val(iter2.next().unwrap()), 4);
                    assert!(iter2.next().is_none());

                    list1.as_mut().clear();
                    list2.as_mut().clear();
                }

                #[test]
                fn test_split_before() {
                    let mut factory = <$factory_type>::new();
                    stack_pin_init!(let list1 = DoublyLinkedList::<$ptr_type>::new());
                    stack_pin_init!(let list2 = DoublyLinkedList::<$ptr_type>::new());

                    let obj1 = factory.create(1);
                    let obj2 = factory.create(2);
                    let obj3 = factory.create(3);
                    let obj4 = factory.create(4);

                    $push(list1.as_mut(), obj4);
                    $push(list1.as_mut(), obj3);
                    $push(list1.as_mut(), obj2);
                    $push(list1.as_mut(), obj1);

                    let mut cursor = list1.as_mut().cursor_front_mut();
                    cursor.move_next();
                    cursor.move_next(); // points to 3

                    let mut dest_cursor = list2.as_mut().cursor_back_mut();
                    cursor.split_before(&mut dest_cursor);

                    // list2 should have elements before cursor (1, 2)
                    let mut iter2 = list2.iter();
                    assert_eq!($get_val(iter2.next().unwrap()), 1);
                    assert_eq!($get_val(iter2.next().unwrap()), 2);
                    assert!(iter2.next().is_none());

                    // list1 should have elements from cursor onwards (3, 4)
                    let mut iter1 = list1.iter();
                    assert_eq!($get_val(iter1.next().unwrap()), 3);
                    assert_eq!($get_val(iter1.next().unwrap()), 4);
                    assert!(iter1.next().is_none());

                    list1.as_mut().clear();
                    list2.as_mut().clear();
                }
            }
        };
    }

    generate_list_tests!(
        raw_ptr_tests,
        *mut TestObject,
        RawFactory<TestObject>,
        |p: &TestObject| p.value,
        |list: Pin<&mut DoublyLinkedList<*mut TestObject>>, obj| {
            // SAFETY: `obj` is a freshly allocated, unlinked object that outlives the list.
            unsafe { list.push_front_raw(obj) };
        }
    );

    generate_list_tests!(
        unique_ptr_tests,
        UniquePtr<UniqueTestObject>,
        UniqueFactory<UniqueTestObject>,
        |p: &UniqueTestObject| p.value,
        |list: Pin<&mut DoublyLinkedList<UniquePtr<UniqueTestObject>>>, obj| list.push_front(obj)
    );

    generate_list_tests!(
        ref_ptr_tests,
        RefPtr<RefTestObject>,
        RefFactory<RefTestObject>,
        |p: &RefTestObject| p.value,
        |list: Pin<&mut DoublyLinkedList<RefPtr<RefTestObject>>>, obj| list.push_front(obj)
    );

    #[test]
    fn test_tracking_size() {
        stack_pin_init!(let list =
            DoublyLinkedList::<UniquePtr<UniqueTestObject>, DefaultObjectTag, TrackingSize>::new());

        assert_eq!(list.len(), 0);
        list.as_mut().push_front(UniquePtr::try_new(UniqueTestObject::new(1)).unwrap());
        assert_eq!(list.len(), 1);
        list.as_mut().push_front(UniquePtr::try_new(UniqueTestObject::new(2)).unwrap());
        assert_eq!(list.len(), 2);
        list.as_mut().pop_front();
        assert_eq!(list.len(), 1);
        list.as_mut().clear();
        assert_eq!(list.len(), 0);
    }

    #[test]
    fn test_insert_before() {
        stack_pin_init!(let list =
            DoublyLinkedList::<UniquePtr<UniqueTestObject>, DefaultObjectTag, TrackingSize>::new());

        let mut cursor = list.as_mut().cursor_front_mut();
        cursor.insert_before(UniquePtr::try_new(UniqueTestObject::new(1)).unwrap());
        assert_eq!(list.len(), 1);
        assert_eq!(list.front().unwrap().value, 1);

        let mut cursor = list.as_mut().cursor_front_mut();
        cursor.insert_before(UniquePtr::try_new(UniqueTestObject::new(2)).unwrap());
        assert_eq!(list.len(), 2);
        assert_eq!(list.front().unwrap().value, 2);

        let mut cursor = list.as_mut().cursor_front_mut();
        cursor.move_next(); // point to obj1
        cursor.insert_before(UniquePtr::try_new(UniqueTestObject::new(3)).unwrap());
        assert_eq!(list.len(), 3);

        let mut cursor = list.as_mut().cursor_front_mut();
        while cursor.get().unwrap().value != 1 {
            cursor.move_next();
        }
        cursor.move_next(); // point to sentinel
        cursor.insert_before(UniquePtr::try_new(UniqueTestObject::new(4)).unwrap());
        assert_eq!(list.len(), 4);

        let mut iter = list.iter();
        assert_eq!(iter.next().unwrap().value, 2);
        assert_eq!(iter.next().unwrap().value, 3);
        assert_eq!(iter.next().unwrap().value, 1);
        assert_eq!(iter.next().unwrap().value, 4);
        assert!(iter.next().is_none());

        list.as_mut().clear();
    }

    #[test]
    fn test_front_mut_and_back_mut() {
        stack_pin_init!(let list =
            DoublyLinkedList::<UniquePtr<UniqueTestObject>, DefaultObjectTag, TrackingSize>::new());

        assert!(list.as_mut().front_mut().is_none());
        assert!(list.as_mut().back_mut().is_none());

        // 1-element list: head == tail.
        list.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(1)).unwrap());
        *list.as_mut().front_mut().unwrap().project().value = 2;
        assert_eq!(list.as_mut().back_mut().unwrap().value, 2);
        *list.as_mut().back_mut().unwrap().project().value = 3;
        assert_eq!(list.as_mut().front_mut().unwrap().value, 3);
        assert_eq!(list.front().unwrap().value, 3);
        assert_eq!(list.back().unwrap().value, 3);
        assert_eq!(list.as_mut().pop_front().unwrap().value, 3);
        assert!(list.is_empty());

        list.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(10)).unwrap());
        list.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(20)).unwrap());

        assert_eq!(list.as_mut().front_mut().unwrap().value, 10);
        assert_eq!(list.as_mut().back_mut().unwrap().value, 20);

        *list.as_mut().front_mut().unwrap().project().value = 100;
        *list.as_mut().back_mut().unwrap().project().value = 200;

        assert_eq!(list.front().unwrap().value, 100);
        assert_eq!(list.back().unwrap().value, 200);

        list.as_mut().clear();
    }

    // Compile-time check that `T` does NOT implement `Trait`. If `T` implemented `Trait`, the
    // `<T as AmbiguousIfImpl<_>>` path below would be ambiguous between the two impls and fail to
    // compile.
    macro_rules! assert_not_impl {
        ($t:ty: $($trait:tt)+) => {
            const _: fn() = || {
                trait AmbiguousIfImpl<A> {
                    fn some_item() {}
                }
                impl<T: ?Sized> AmbiguousIfImpl<()> for T {}
                enum Invalid {}
                impl<T: ?Sized + $($trait)+> AmbiguousIfImpl<Invalid> for T {}
                let _ = <$t as AmbiguousIfImpl<_>>::some_item;
            };
        };
    }

    // Linked elements must not be movable through safe `Pin` APIs.
    assert_not_impl!(DoublyLinkedListNode<TestObject>: Unpin);
    assert_not_impl!(TestObject: Unpin);
    assert_not_impl!(UniqueTestObject: Unpin);
    assert_not_impl!(RefTestObject: Unpin);
    // Lists must not be movable through safe `Pin` APIs, since elements point back at them.
    assert_not_impl!(DoublyLinkedList<*mut TestObject>: Unpin);
    // Shared ownership must not grant mutable element access.
    assert_not_impl!(RefPtr<RefTestObject>: MutPtr);

    #[test]
    fn test_iter_mut() {
        stack_pin_init!(let list =
            DoublyLinkedList::<UniquePtr<UniqueTestObject>, DefaultObjectTag, TrackingSize>::new());

        assert!(list.as_mut().iter_mut().next().is_none());
        assert!(list.as_mut().iter_mut().next_back().is_none());

        // 1-element list: head == tail and head.prev == head.
        list.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(7)).unwrap());
        {
            let mut iter = list.as_mut().iter_mut();
            let only = iter.next_back().unwrap();
            assert_eq!(only.value, 7);
            *only.project().value = 8;
            assert!(iter.next_back().is_none());
            assert!(iter.next().is_none());
        }
        {
            let mut iter = list.as_mut().iter_mut();
            assert_eq!(iter.next().unwrap().value, 8);
            assert!(iter.next().is_none());
            assert!(iter.next_back().is_none());
        }
        assert_eq!(list.as_mut().pop_front().unwrap().value, 8);
        assert!(list.is_empty());

        list.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(1)).unwrap());
        list.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(2)).unwrap());
        list.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(3)).unwrap());

        for obj in list.as_mut().iter_mut() {
            *obj.project().value *= 10;
        }

        let mut iter = list.as_mut().iter_mut();
        assert_eq!(iter.next().unwrap().value, 10);
        let mut last = iter.next_back().unwrap();
        assert_eq!(last.value, 30);
        *last.as_mut().project().value += 1;
        let mut middle = iter.next().unwrap();
        *middle.as_mut().project().value += 2;
        assert!(iter.next().is_none());
        assert!(iter.next_back().is_none());

        let values: alloc::vec::Vec<i32> = list.iter().map(|o| o.value).collect();
        assert_eq!(values, [10, 22, 31]);
        assert_eq!(list.len(), 3);

        // Hold every yielded reference simultaneously.
        {
            let mut items: alloc::vec::Vec<_> = list.as_mut().iter_mut().collect();
            assert_eq!(items.len(), 3);
            for item in items.iter_mut() {
                *item.as_mut().project().value += 1;
            }
            *items[0].as_mut().project().value += 100;
            *items[2].as_mut().project().value += 200;
        }
        let values: alloc::vec::Vec<i32> = list.iter().map(|o| o.value).collect();
        assert_eq!(values, [111, 23, 232]);

        list.as_mut().clear();
        assert!(list.is_empty());

        // 2-element list: hold `first` (the head) across the `next_back()` call where the front
        // and back iterators meet, and write to it afterward.
        list.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(1)).unwrap());
        list.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(2)).unwrap());
        {
            let mut iter = list.as_mut().iter_mut();
            let mut first = iter.next().unwrap();
            *first.as_mut().project().value = 10;
            let mut second = iter.next_back().unwrap();
            *first.as_mut().project().value = 11;
            *second.as_mut().project().value = 20;
            assert!(iter.next().is_none());
            assert!(iter.next_back().is_none());
            assert_eq!(first.value, 11);
            assert_eq!(second.value, 20);
        }
        let values: alloc::vec::Vec<i32> = list.iter().map(|o| o.value).collect();
        assert_eq!(values, [11, 20]);

        // Same as above, but starting from the back.
        {
            let mut iter = list.as_mut().iter_mut();
            let mut last = iter.next_back().unwrap();
            *last.as_mut().project().value = 21;
            let mut first = iter.next().unwrap();
            *last.as_mut().project().value = 22;
            *first.as_mut().project().value = 12;
            assert!(iter.next().is_none());
            assert!(iter.next_back().is_none());
        }
        let values: alloc::vec::Vec<i32> = list.iter().map(|o| o.value).collect();
        assert_eq!(values, [12, 22]);

        list.as_mut().clear();
    }

    #[test]
    fn test_forward_and_reverse_iter_mut() {
        stack_pin_init!(let list = DoublyLinkedList::<UniquePtr<UniqueTestObject>>::new());

        assert!(list.as_mut().forward_iter_mut().next().is_none());
        assert!(list.as_mut().reverse_iter_mut().next().is_none());

        // 1-element list: head == tail and head.prev == head.
        list.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(5)).unwrap());
        {
            let mut r_iter = list.as_mut().reverse_iter_mut();
            let only = r_iter.next().unwrap();
            assert_eq!(only.value, 5);
            *only.project().value = 6;
            assert!(r_iter.next().is_none());
        }
        {
            let mut f_iter = list.as_mut().forward_iter_mut();
            assert_eq!(f_iter.next().unwrap().value, 6);
            assert!(f_iter.next().is_none());
        }
        assert_eq!(list.as_mut().pop_front().unwrap().value, 6);
        assert!(list.is_empty());

        list.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(1)).unwrap());
        list.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(2)).unwrap());
        list.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(3)).unwrap());

        // Forward: add 10 to every element.
        for obj in list.as_mut().forward_iter_mut() {
            *obj.project().value += 10;
        }
        let values: alloc::vec::Vec<i32> = list.iter().map(|o| o.value).collect();
        assert_eq!(values, [11, 12, 13]);

        // Reverse: visit in reverse order and record it.
        let mut visited = alloc::vec::Vec::new();
        for obj in list.as_mut().reverse_iter_mut() {
            visited.push(obj.value);
            *obj.project().value *= 2;
        }
        assert_eq!(visited, [13, 12, 11]);
        let values: alloc::vec::Vec<i32> = list.iter().map(|o| o.value).collect();
        assert_eq!(values, [22, 24, 26]);

        // Hold every yielded reference from the reverse iterator simultaneously.
        {
            let mut items: alloc::vec::Vec<_> = list.as_mut().reverse_iter_mut().collect();
            assert_eq!(items.len(), 3);
            for item in items.iter_mut() {
                *item.as_mut().project().value += 1;
            }
            // `items[0]` is the tail and `items[2]` is the head; write to both after all
            // references were yielded.
            *items[0].as_mut().project().value += 100;
            *items[2].as_mut().project().value += 200;
            assert_eq!(items[0].value, 127);
            assert_eq!(items[1].value, 25);
            assert_eq!(items[2].value, 223);
        }
        let values: alloc::vec::Vec<i32> = list.iter().map(|o| o.value).collect();
        assert_eq!(values, [223, 25, 127]);

        // Hold every yielded reference from the forward iterator simultaneously.
        {
            let mut items: alloc::vec::Vec<_> = list.as_mut().forward_iter_mut().collect();
            assert_eq!(items.len(), 3);
            for item in items.iter_mut() {
                *item.as_mut().project().value -= 1;
            }
            *items[2].as_mut().project().value -= 100;
            *items[0].as_mut().project().value -= 200;
        }
        let values: alloc::vec::Vec<i32> = list.iter().map(|o| o.value).collect();
        assert_eq!(values, [22, 24, 26]);

        list.as_mut().clear();
    }

    #[test]
    fn test_iter_mut_raw_ptr() {
        stack_pin_init!(let list = DoublyLinkedList::<*mut TestObject>::new());
        let mut obj1 = TestObject::new(1);
        let mut obj2 = TestObject::new(2);

        // SAFETY: The objects outlive the list and are removed before the list is dropped.
        unsafe {
            list.as_mut().push_back_raw(&mut obj1);
            list.as_mut().push_back_raw(&mut obj2);
        }

        for obj in list.as_mut().iter_mut() {
            *obj.project().value += 100;
        }
        *list.as_mut().front_mut().unwrap().project().value += 1;
        *list.as_mut().back_mut().unwrap().project().value += 2;

        let values: alloc::vec::Vec<i32> = list.iter().map(|o| o.value).collect();
        assert_eq!(values, [102, 104]);

        list.as_mut().clear();
    }

    #[test]
    fn test_cursor_get_mut() {
        stack_pin_init!(let list =
            DoublyLinkedList::<UniquePtr<UniqueTestObject>, DefaultObjectTag, TrackingSize>::new());

        assert!(list.as_mut().cursor_front_mut().get_mut().is_none());

        list.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(1)).unwrap());
        list.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(2)).unwrap());

        let mut cursor = list.as_mut().cursor_front_mut();
        *cursor.get_mut().unwrap().project().value = 10;
        cursor.move_next();
        assert_eq!(cursor.get_mut().unwrap().value, 2);
        *cursor.get_mut().unwrap().project().value = 20;
        cursor.move_next();
        assert!(cursor.get_mut().is_none()); // Sentinel.

        let mut iter = list.iter();
        assert_eq!(iter.next().unwrap().value, 10);
        assert_eq!(iter.next().unwrap().value, 20);
        assert!(iter.next().is_none());
        assert_eq!(list.len(), 2);

        list.as_mut().clear();
    }

    #[test]
    fn test_cursor_front_and_back_mut() {
        stack_pin_init!(let list =
            DoublyLinkedList::<UniquePtr<UniqueTestObject>, DefaultObjectTag, TrackingSize>::new());

        assert!(list.as_mut().cursor_front_mut().get().is_none());
        assert!(list.as_mut().cursor_back_mut().get().is_none());

        list.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(10)).unwrap());
        list.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(20)).unwrap());

        let mut front_cursor = list.as_mut().cursor_front_mut();
        assert_eq!(front_cursor.get().unwrap().value, 10);
        front_cursor.move_next();
        assert_eq!(front_cursor.get().unwrap().value, 20);

        let mut back_cursor = list.as_mut().cursor_back_mut();
        assert!(back_cursor.get().is_none());
        back_cursor.move_prev();
        assert_eq!(back_cursor.get().unwrap().value, 20);

        let mut back_cursor = list.as_mut().cursor_back_mut();
        back_cursor.insert_before(UniquePtr::try_new(UniqueTestObject::new(30)).unwrap());
        assert_eq!(list.len(), 3);
        assert_eq!(list.back().unwrap().value, 30);

        list.as_mut().clear();
    }

    #[test]
    fn test_splice_middle() {
        stack_pin_init!(let list1 =
            DoublyLinkedList::<UniquePtr<UniqueTestObject>, DefaultObjectTag, TrackingSize>::new());
        stack_pin_init!(let list2 =
            DoublyLinkedList::<UniquePtr<UniqueTestObject>, DefaultObjectTag, TrackingSize>::new());

        list1.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(1)).unwrap());
        list1.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(2)).unwrap());
        list2.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(3)).unwrap());
        list2.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(4)).unwrap());

        let mut cursor = list1.as_mut().cursor_front_mut();
        cursor.move_next(); // point to obj2

        cursor.splice(list2.as_mut());

        assert!(list2.is_empty());
        assert_eq!(list1.len(), 4);

        let mut iter = list1.iter();
        assert_eq!(iter.next().unwrap().value, 1);
        assert_eq!(iter.next().unwrap().value, 3);
        assert_eq!(iter.next().unwrap().value, 4);
        assert_eq!(iter.next().unwrap().value, 2);
        assert!(iter.next().is_none());

        list1.as_mut().clear();
    }

    #[test]
    fn test_splice_head() {
        stack_pin_init!(let list1 =
            DoublyLinkedList::<UniquePtr<UniqueTestObject>, DefaultObjectTag, TrackingSize>::new());
        stack_pin_init!(let list2 =
            DoublyLinkedList::<UniquePtr<UniqueTestObject>, DefaultObjectTag, TrackingSize>::new());

        list1.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(1)).unwrap());
        list1.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(2)).unwrap());
        list2.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(3)).unwrap());
        list2.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(4)).unwrap());

        let mut cursor = list1.as_mut().cursor_front_mut();

        cursor.splice(list2.as_mut());

        assert!(list2.is_empty());
        assert_eq!(list1.len(), 4);

        let mut iter = list1.iter();
        assert_eq!(iter.next().unwrap().value, 3);
        assert_eq!(iter.next().unwrap().value, 4);
        assert_eq!(iter.next().unwrap().value, 1);
        assert_eq!(iter.next().unwrap().value, 2);
        assert!(iter.next().is_none());

        list1.as_mut().clear();
    }

    #[test]
    fn test_splice_tail() {
        stack_pin_init!(let list1 =
            DoublyLinkedList::<UniquePtr<UniqueTestObject>, DefaultObjectTag, TrackingSize>::new());
        stack_pin_init!(let list2 =
            DoublyLinkedList::<UniquePtr<UniqueTestObject>, DefaultObjectTag, TrackingSize>::new());

        list1.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(1)).unwrap());
        list1.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(2)).unwrap());
        list2.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(3)).unwrap());
        list2.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(4)).unwrap());

        let mut cursor = list1.as_mut().cursor_front_mut();
        cursor.move_next();
        cursor.move_next(); // point to sentinel

        cursor.splice(list2.as_mut());

        assert!(list2.is_empty());
        assert_eq!(list1.len(), 4);

        let mut iter = list1.iter();
        assert_eq!(iter.next().unwrap().value, 1);
        assert_eq!(iter.next().unwrap().value, 2);
        assert_eq!(iter.next().unwrap().value, 3);
        assert_eq!(iter.next().unwrap().value, 4);
        assert!(iter.next().is_none());

        list1.as_mut().clear();
    }

    #[test]
    fn test_splice_empty() {
        stack_pin_init!(let list1 =
            DoublyLinkedList::<UniquePtr<UniqueTestObject>, DefaultObjectTag, TrackingSize>::new());
        stack_pin_init!(let list2 =
            DoublyLinkedList::<UniquePtr<UniqueTestObject>, DefaultObjectTag, TrackingSize>::new());

        list2.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(1)).unwrap());
        list2.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(2)).unwrap());

        let mut cursor = list1.as_mut().cursor_front_mut();
        cursor.splice(list2.as_mut());

        assert!(list2.is_empty());
        assert_eq!(list1.len(), 2);

        let mut iter = list1.iter();
        assert_eq!(iter.next().unwrap().value, 1);
        assert_eq!(iter.next().unwrap().value, 2);
        assert!(iter.next().is_none());

        list1.as_mut().clear();
    }

    #[test]
    fn test_splice_non_tracking() {
        stack_pin_init!(let list1 =
            DoublyLinkedList::<UniquePtr<UniqueTestObject>, DefaultObjectTag, NonTrackingSize>::new());
        stack_pin_init!(let list2 =
            DoublyLinkedList::<UniquePtr<UniqueTestObject>, DefaultObjectTag, NonTrackingSize>::new());

        list1.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(1)).unwrap());
        list1.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(2)).unwrap());
        list2.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(3)).unwrap());
        list2.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(4)).unwrap());

        let mut cursor = list1.as_mut().cursor_front_mut();
        cursor.move_next(); // point to obj2

        cursor.splice(list2.as_mut());

        assert!(list2.is_empty());

        let mut iter = list1.iter();
        assert_eq!(iter.next().unwrap().value, 1);
        assert_eq!(iter.next().unwrap().value, 3);
        assert_eq!(iter.next().unwrap().value, 4);
        assert_eq!(iter.next().unwrap().value, 2);
        assert!(iter.next().is_none());

        list1.as_mut().clear();
    }

    #[test]
    fn test_list_splice() {
        // Test splicing non-empty other into non-empty self.
        stack_pin_init!(let list1 =
            DoublyLinkedList::<UniquePtr<UniqueTestObject>, DefaultObjectTag, TrackingSize>::new());
        stack_pin_init!(let list2 =
            DoublyLinkedList::<UniquePtr<UniqueTestObject>, DefaultObjectTag, TrackingSize>::new());

        list1.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(1)).unwrap());
        list1.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(2)).unwrap());
        list2.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(3)).unwrap());
        list2.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(4)).unwrap());

        list1.as_mut().splice(list2.as_mut());

        assert!(list2.is_empty());
        assert_eq!(list2.len(), 0);
        assert_eq!(list1.len(), 4);

        let mut iter = list1.iter();
        assert_eq!(iter.next().unwrap().value, 1);
        assert_eq!(iter.next().unwrap().value, 2);
        assert_eq!(iter.next().unwrap().value, 3);
        assert_eq!(iter.next().unwrap().value, 4);
        assert!(iter.next().is_none());

        // Test splicing into empty self.
        stack_pin_init!(let list3 =
            DoublyLinkedList::<UniquePtr<UniqueTestObject>, DefaultObjectTag, TrackingSize>::new());

        list3.as_mut().splice(list1.as_mut());

        assert!(list1.is_empty());
        assert_eq!(list1.len(), 0);
        assert_eq!(list3.len(), 4);

        let mut iter = list3.iter();
        assert_eq!(iter.next().unwrap().value, 1);
        assert_eq!(iter.next().unwrap().value, 2);
        assert_eq!(iter.next().unwrap().value, 3);
        assert_eq!(iter.next().unwrap().value, 4);
        assert!(iter.next().is_none());

        // Test splicing empty list into non-empty self (no-op).
        list3.as_mut().splice(list1.as_mut());
        assert_eq!(list3.len(), 4);
        assert!(list1.is_empty());

        list3.as_mut().clear();
    }

    #[test]
    fn test_split_after_at_tail() {
        stack_pin_init!(let list1 =
            DoublyLinkedList::<UniquePtr<UniqueTestObject>, DefaultObjectTag, TrackingSize>::new());
        stack_pin_init!(let list2 =
            DoublyLinkedList::<UniquePtr<UniqueTestObject>, DefaultObjectTag, TrackingSize>::new());

        list1.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(1)).unwrap());
        list1.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(2)).unwrap());

        let mut cursor = list1.as_mut().cursor_front_mut();
        cursor.move_next(); // points to 2 (tail)

        let mut dest_cursor = list2.as_mut().cursor_back_mut();
        cursor.split_after(&mut dest_cursor);

        assert!(list2.is_empty());
        assert_eq!(list2.len(), 0);
        assert_eq!(list1.len(), 2);

        list1.as_mut().clear();
    }

    #[test]
    fn test_split_after_at_head() {
        stack_pin_init!(let list1 =
            DoublyLinkedList::<UniquePtr<UniqueTestObject>, DefaultObjectTag, TrackingSize>::new());
        stack_pin_init!(let list2 =
            DoublyLinkedList::<UniquePtr<UniqueTestObject>, DefaultObjectTag, TrackingSize>::new());

        list1.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(1)).unwrap());
        list1.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(2)).unwrap());
        list1.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(3)).unwrap());

        let mut cursor = list1.as_mut().cursor_front_mut(); // points to 1 (head)

        let mut dest_cursor = list2.as_mut().cursor_back_mut();
        cursor.split_after(&mut dest_cursor);

        assert_eq!(list1.len(), 1);
        assert_eq!(list2.len(), 2);

        let mut iter1 = list1.iter();
        assert_eq!(iter1.next().unwrap().value, 1);
        assert!(iter1.next().is_none());

        let mut iter2 = list2.iter();
        assert_eq!(iter2.next().unwrap().value, 2);
        assert_eq!(iter2.next().unwrap().value, 3);
        assert!(iter2.next().is_none());

        list1.as_mut().clear();
        list2.as_mut().clear();
    }

    #[test]
    fn test_split_before_at_head() {
        stack_pin_init!(let list1 =
            DoublyLinkedList::<UniquePtr<UniqueTestObject>, DefaultObjectTag, TrackingSize>::new());
        stack_pin_init!(let list2 =
            DoublyLinkedList::<UniquePtr<UniqueTestObject>, DefaultObjectTag, TrackingSize>::new());

        list1.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(1)).unwrap());
        list1.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(2)).unwrap());

        let mut cursor = list1.as_mut().cursor_front_mut(); // points to 1 (head)

        let mut dest_cursor = list2.as_mut().cursor_back_mut();
        cursor.split_before(&mut dest_cursor);

        assert!(list2.is_empty());
        assert_eq!(list2.len(), 0);
        assert_eq!(list1.len(), 2);

        list1.as_mut().clear();
    }

    #[test]
    fn test_split_before_at_tail() {
        stack_pin_init!(let list1 =
            DoublyLinkedList::<UniquePtr<UniqueTestObject>, DefaultObjectTag, TrackingSize>::new());
        stack_pin_init!(let list2 =
            DoublyLinkedList::<UniquePtr<UniqueTestObject>, DefaultObjectTag, TrackingSize>::new());

        list1.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(1)).unwrap());
        list1.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(2)).unwrap());
        list1.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(3)).unwrap());

        let mut cursor = list1.as_mut().cursor_front_mut();
        cursor.move_next();
        cursor.move_next(); // points to 3 (tail)

        let mut dest_cursor = list2.as_mut().cursor_back_mut();
        cursor.split_before(&mut dest_cursor);

        assert_eq!(list2.len(), 2);
        assert_eq!(list1.len(), 1);

        let mut iter2 = list2.iter();
        assert_eq!(iter2.next().unwrap().value, 1);
        assert_eq!(iter2.next().unwrap().value, 2);
        assert!(iter2.next().is_none());

        let mut iter1 = list1.iter();
        assert_eq!(iter1.next().unwrap().value, 3);
        assert!(iter1.next().is_none());

        list1.as_mut().clear();
        list2.as_mut().clear();
    }

    #[test]
    fn test_split_before_at_sentinel() {
        stack_pin_init!(let list1 =
            DoublyLinkedList::<UniquePtr<UniqueTestObject>, DefaultObjectTag, TrackingSize>::new());
        stack_pin_init!(let list2 =
            DoublyLinkedList::<UniquePtr<UniqueTestObject>, DefaultObjectTag, TrackingSize>::new());

        list1.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(1)).unwrap());
        list1.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(2)).unwrap());

        let mut cursor = list1.as_mut().cursor_back_mut(); // points to sentinel

        let mut dest_cursor = list2.as_mut().cursor_back_mut();
        cursor.split_before(&mut dest_cursor);

        assert!(list1.is_empty());
        assert_eq!(list1.len(), 0);
        assert_eq!(list2.len(), 2);

        let mut iter2 = list2.iter();
        assert_eq!(iter2.next().unwrap().value, 1);
        assert_eq!(iter2.next().unwrap().value, 2);
        assert!(iter2.next().is_none());

        list2.as_mut().clear();
    }

    #[test]
    fn test_split_into_middle_of_dest_list() {
        stack_pin_init!(let list1 =
            DoublyLinkedList::<UniquePtr<UniqueTestObject>, DefaultObjectTag, TrackingSize>::new());
        stack_pin_init!(let list2 =
            DoublyLinkedList::<UniquePtr<UniqueTestObject>, DefaultObjectTag, TrackingSize>::new());

        list1.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(1)).unwrap());
        list1.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(2)).unwrap());
        list1.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(3)).unwrap());
        list1.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(4)).unwrap());

        list2.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(10)).unwrap());
        list2.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(20)).unwrap());

        // Split after 2 from list1, and insert before 20 in list2
        let mut cursor1 = list1.as_mut().cursor_front_mut();
        cursor1.move_next(); // points to 2

        let mut cursor2 = list2.as_mut().cursor_front_mut();
        cursor2.move_next(); // points to 20

        cursor1.split_after(&mut cursor2);

        // list1 should retain [1, 2]
        assert_eq!(list1.len(), 2);
        let mut iter1 = list1.iter();
        assert_eq!(iter1.next().unwrap().value, 1);
        assert_eq!(iter1.next().unwrap().value, 2);
        assert!(iter1.next().is_none());

        // list2 should now be [10, 3, 4, 20]
        assert_eq!(list2.len(), 4);
        let mut iter2 = list2.iter();
        assert_eq!(iter2.next().unwrap().value, 10);
        assert_eq!(iter2.next().unwrap().value, 3);
        assert_eq!(iter2.next().unwrap().value, 4);
        assert_eq!(iter2.next().unwrap().value, 20);
        assert!(iter2.next().is_none());

        list1.as_mut().clear();
        list2.as_mut().clear();
    }

    #[test]
    fn test_list_split_methods() {
        let mut obj1 = TestObject::new(1);
        let mut obj2 = TestObject::new(2);
        let mut obj3 = TestObject::new(3);

        stack_pin_init!(let list1 = DoublyLinkedList::<*mut TestObject>::new());
        stack_pin_init!(let list2 = DoublyLinkedList::<*mut TestObject>::new());

        unsafe {
            list1.as_mut().push_back_raw(&mut obj1);
            list1.as_mut().push_back_raw(&mut obj2);
            list1.as_mut().push_back_raw(&mut obj3);

            // Split after obj1
            let mut cursor1 = list1.as_mut().cursor_at(&obj1);
            let mut cursor2 = list2.as_mut().cursor_back_mut();
            cursor1.split_after(&mut cursor2);

            assert_eq!(list1.iter().count(), 1);
            assert_eq!(list2.iter().count(), 2);

            let mut iter1 = list1.iter();
            assert_eq!(iter1.next().unwrap().value, 1);
            assert!(iter1.next().is_none());

            let mut iter2 = list2.iter();
            assert_eq!(iter2.next().unwrap().value, 2);
            assert_eq!(iter2.next().unwrap().value, 3);
            assert!(iter2.next().is_none());

            // Splice back
            list1.as_mut().splice(list2.as_mut());
            assert_eq!(list1.iter().count(), 3);

            // Split before obj3
            let mut cursor1 = list1.as_mut().cursor_at(&obj3);
            let mut cursor2 = list2.as_mut().cursor_back_mut();
            cursor1.split_before(&mut cursor2);

            assert_eq!(list2.iter().count(), 2);
            assert_eq!(list1.iter().count(), 1);

            let mut iter2 = list2.iter();
            assert_eq!(iter2.next().unwrap().value, 1);
            assert_eq!(iter2.next().unwrap().value, 2);
            assert!(iter2.next().is_none());

            let mut iter1 = list1.iter();
            assert_eq!(iter1.next().unwrap().value, 3);
            assert!(iter1.next().is_none());

            list1.as_mut().clear();
            list2.as_mut().clear();
        }
    }

    #[test]
    fn test_cursor_get() {
        stack_pin_init!(let list = DoublyLinkedList::<UniquePtr<UniqueTestObject>>::new());

        let cursor = list.as_mut().cursor_front_mut();
        assert!(cursor.get().is_none());

        list.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(42)).unwrap());

        let mut cursor = list.as_mut().cursor_front_mut();
        assert!(cursor.get().is_some());
        assert_eq!(cursor.get().unwrap().value, 42);

        cursor.move_next();
        assert!(cursor.get().is_none());

        list.as_mut().clear();
    }

    #[test]
    fn test_pop_back() {
        stack_pin_init!(let list =
            DoublyLinkedList::<UniquePtr<UniqueTestObject>, DefaultObjectTag, TrackingSize>::new());

        list.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(1)).unwrap());
        list.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(2)).unwrap());

        assert_eq!(list.len(), 2);
        let popped = list.as_mut().pop_back();
        assert!(popped.is_some());
        assert_eq!(popped.unwrap().value, 2);
        assert_eq!(list.len(), 1);

        let popped = list.as_mut().pop_back();
        assert!(popped.is_some());
        assert_eq!(popped.unwrap().value, 1);
        assert_eq!(list.len(), 0);

        assert!(list.as_mut().pop_back().is_none());
    }

    #[test]
    fn test_erase() {
        stack_pin_init!(let list =
            DoublyLinkedList::<UniquePtr<UniqueTestObject>, DefaultObjectTag, TrackingSize>::new());

        list.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(1)).unwrap());
        list.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(2)).unwrap());
        list.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(3)).unwrap());

        assert_eq!(list.len(), 3);

        // 1. Erase middle (obj2)
        let mut cursor = list.as_mut().cursor_front_mut();
        cursor.move_next(); // point to obj2
        let erased = cursor.erase();
        assert!(erased.is_some());
        assert_eq!(erased.unwrap().value, 2);
        assert_eq!(list.len(), 2);

        let mut iter = list.iter();
        assert_eq!(iter.next().unwrap().value, 1);
        assert_eq!(iter.next().unwrap().value, 3);
        assert!(iter.next().is_none());

        // 2. Erase head (obj1)
        let mut cursor = list.as_mut().cursor_front_mut();
        let erased = cursor.erase(); // current is head (obj1)
        assert!(erased.is_some());
        assert_eq!(erased.unwrap().value, 1);
        assert_eq!(list.len(), 1);
        assert_eq!(list.front().unwrap().value, 3); // obj3 is now head!

        // 3. Erase last element (obj3)
        let mut cursor = list.as_mut().cursor_front_mut();
        let erased = cursor.erase(); // current is head/tail (obj3)
        assert!(erased.is_some());
        assert_eq!(erased.unwrap().value, 3);
        assert_eq!(list.len(), 0);
        assert!(list.is_empty());

        list.as_mut().clear();
    }

    #[test]
    fn test_erase_if() {
        stack_pin_init!(let list =
            DoublyLinkedList::<UniquePtr<UniqueTestObject>, DefaultObjectTag, TrackingSize>::new());

        list.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(1)).unwrap());
        list.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(2)).unwrap());
        list.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(3)).unwrap());

        let erased = list.as_mut().erase_if(|obj| obj.value % 2 == 0);
        assert!(erased.is_some());
        assert_eq!(erased.unwrap().value, 2);

        assert_eq!(list.len(), 2);
        let mut iter = list.iter();
        assert_eq!(iter.next().unwrap().value, 1);
        assert_eq!(iter.next().unwrap().value, 3);
        assert!(iter.next().is_none());

        list.as_mut().clear();
    }

    #[test]
    fn test_erase_by_reference() {
        stack_pin_init!(let list =
            DoublyLinkedList::<*mut TestObject, DefaultObjectTag, TrackingSize>::new());
        let mut obj1 = TestObject::new(1);
        let mut obj2 = TestObject::new(2);
        let mut obj3 = TestObject::new(3);

        unsafe {
            list.as_mut().push_back_raw(&mut obj1);
            list.as_mut().push_back_raw(&mut obj2);
            list.as_mut().push_back_raw(&mut obj3);
        }

        assert_eq!(list.len(), 3);

        // Erase obj2 directly (safe because it's unmanaged raw pointer to stack)
        let erased = unsafe { list.as_mut().erase(&obj2) };
        assert!(erased.is_some());
        assert_eq!(unsafe { &*erased.unwrap() }.value, 2);
        assert_eq!(list.len(), 2);

        let mut iter = list.iter();
        assert_eq!(iter.next().unwrap().value, 1);
        assert_eq!(iter.next().unwrap().value, 3);
        assert!(iter.next().is_none());

        list.as_mut().clear();
    }

    #[test]
    fn test_remove_from_container() {
        stack_pin_init!(let list = DoublyLinkedList::<*mut TestObject>::new());
        let mut obj1 = TestObject::new(1);
        let mut obj2 = TestObject::new(2);
        let mut obj3 = TestObject::new(3);

        // Case 1: Attempt to remove an element not in any container
        let removed = unsafe {
            remove_from_container::<TestObject, DefaultObjectTag, *mut TestObject>(&obj1)
        };
        assert!(removed.is_none());

        unsafe {
            list.as_mut().push_back_raw(&mut obj1);
            list.as_mut().push_back_raw(&mut obj2);
            list.as_mut().push_back_raw(&mut obj3);
        }

        // Case 2: Remove middle (obj2)
        let removed = unsafe {
            remove_from_container::<TestObject, DefaultObjectTag, *mut TestObject>(&obj2)
        };
        assert!(removed.is_some());
        assert_eq!(unsafe { &*removed.unwrap() }.value, 2);

        let mut iter = list.iter();
        assert_eq!(iter.next().unwrap().value, 1);
        assert_eq!(iter.next().unwrap().value, 3);
        assert!(iter.next().is_none());

        // Case 3: Remove head (obj1)
        let removed = unsafe {
            remove_from_container::<TestObject, DefaultObjectTag, *mut TestObject>(&obj1)
        };
        assert!(removed.is_some());
        assert_eq!(unsafe { &*removed.unwrap() }.value, 1);

        let mut iter = list.iter();
        assert_eq!(iter.next().unwrap().value, 3);
        assert!(iter.next().is_none());

        // Case 4: Remove last remaining element (obj3 -> leaves empty!)
        let removed = unsafe {
            remove_from_container::<TestObject, DefaultObjectTag, *mut TestObject>(&obj3)
        };
        assert!(removed.is_some());
        assert_eq!(unsafe { &*removed.unwrap() }.value, 3);
        assert!(list.is_empty());

        list.as_mut().clear();
    }

    #[test]
    fn test_replace() {
        stack_pin_init!(let list =
            DoublyLinkedList::<*mut TestObject, DefaultObjectTag, TrackingSize>::new());
        let mut obj1 = TestObject::new(1);
        let mut obj2 = TestObject::new(2);
        let mut obj3 = TestObject::new(3);

        unsafe {
            list.as_mut().push_back_raw(&mut obj1);
            list.as_mut().push_back_raw(&mut obj2);
        }

        assert_eq!(list.len(), 2);

        let old = unsafe { list.as_mut().replace_raw(&obj2, &mut obj3) };
        assert!(old.is_some());
        assert_eq!(unsafe { &*old.unwrap() }.value, 2);
        assert_eq!(list.len(), 2);

        let mut iter = list.iter();
        assert_eq!(iter.next().unwrap().value, 1);
        assert_eq!(iter.next().unwrap().value, 3);
        assert!(iter.next().is_none());

        list.as_mut().clear();
    }

    #[test]
    fn test_cursor_replace() {
        stack_pin_init!(let list = DoublyLinkedList::<UniquePtr<UniqueTestObject>>::new());

        let obj1 = UniquePtr::try_new(UniqueTestObject::new(1)).unwrap();
        let obj2 = UniquePtr::try_new(UniqueTestObject::new(2)).unwrap();
        let obj3 = UniquePtr::try_new(UniqueTestObject::new(3)).unwrap();

        list.as_mut().push_back(obj1);
        list.as_mut().push_back(obj2);

        let mut cursor = list.as_mut().cursor_front_mut();
        cursor.move_next(); // point to obj2

        let old = cursor.replace(obj3);
        assert!(old.is_some());
        assert_eq!(old.unwrap().value, 2);

        let mut iter = list.iter();
        assert_eq!(iter.next().unwrap().value, 1);
        assert_eq!(iter.next().unwrap().value, 3);
        assert!(iter.next().is_none());

        list.as_mut().clear();
    }

    struct Tag2;

    #[fbl::ref_counted]
    #[derive(crate::DoublyLinkedListContainable, crate::Recyclable)]
    #[repr(C)]
    struct MultiListObject {
        value: i32,
        #[dll_node]
        node1: DoublyLinkedListNode<MultiListObject>,
        #[dll_node(tag = Tag2)]
        node2: DoublyLinkedListNode<MultiListObject>,
    }

    #[test]
    fn test_multiple_containers() {
        stack_pin_init!(let list1 =
            DoublyLinkedList::<RefPtr<MultiListObject>, DefaultObjectTag>::new());
        stack_pin_init!(let list2 = DoublyLinkedList::<RefPtr<MultiListObject>, Tag2>::new());

        let obj1 = fbl::make_ref_counted!(MultiListObject {
            value: 1,
            node1: DoublyLinkedListNode::new(),
            node2: DoublyLinkedListNode::new(),
        })
        .unwrap();

        let obj2 = fbl::make_ref_counted!(MultiListObject {
            value: 2,
            node1: DoublyLinkedListNode::new(),
            node2: DoublyLinkedListNode::new(),
        })
        .unwrap();

        list1.as_mut().push_back(obj1.clone());
        list1.as_mut().push_back(obj2.clone());

        list2.as_mut().push_back(obj2); // obj2 is now in both lists!

        let mut iter1 = list1.iter();
        assert_eq!(iter1.next().unwrap().value, 1);
        assert_eq!(iter1.next().unwrap().value, 2);
        assert!(iter1.next().is_none());

        let mut iter2 = list2.iter();
        assert_eq!(iter2.next().unwrap().value, 2);
        assert!(iter2.next().is_none());

        list1.as_mut().clear();
        list2.as_mut().clear();
    }

    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicBool, Ordering};

    #[derive(crate::DoublyLinkedListContainable, crate::Recyclable)]
    struct LifecycleObject {
        destroyed: Arc<AtomicBool>,
        #[dll_node]
        node: DoublyLinkedListNode<LifecycleObject>,
    }

    impl LifecycleObject {
        fn new(destroyed: Arc<AtomicBool>) -> Self {
            Self { destroyed, node: DoublyLinkedListNode::new() }
        }
    }

    impl Drop for LifecycleObject {
        fn drop(&mut self) {
            self.destroyed.store(true, Ordering::Relaxed);
        }
    }

    #[test]
    fn test_lifecycle_on_drop() {
        let destroyed1 = Arc::new(AtomicBool::new(false));
        let destroyed2 = Arc::new(AtomicBool::new(false));

        {
            stack_pin_init!(let list = DoublyLinkedList::<UniquePtr<LifecycleObject>>::new());

            let obj1 = UniquePtr::try_new(LifecycleObject::new(destroyed1.clone())).unwrap();
            let obj2 = UniquePtr::try_new(LifecycleObject::new(destroyed2.clone())).unwrap();

            list.as_mut().push_back(obj1);
            list.as_mut().push_back(obj2);

            assert!(!destroyed1.load(Ordering::Relaxed));
            assert!(!destroyed2.load(Ordering::Relaxed));
        } // list drops here

        assert!(destroyed1.load(Ordering::Relaxed));
        assert!(destroyed2.load(Ordering::Relaxed));
    }

    #[test]
    fn test_sized_managed_list() {
        stack_pin_init!(let list =
            DoublyLinkedList::<UniquePtr<UniqueTestObject>, DefaultObjectTag, TrackingSize>::new());

        assert_eq!(list.len(), 0);

        let obj1 = UniquePtr::try_new(UniqueTestObject::new(1)).unwrap();
        let obj2 = UniquePtr::try_new(UniqueTestObject::new(2)).unwrap();

        list.as_mut().push_back(obj1);
        assert_eq!(list.len(), 1);

        list.as_mut().push_back(obj2);
        assert_eq!(list.len(), 2);

        let popped = list.as_mut().pop_front();
        assert!(popped.is_some());
        assert_eq!(list.len(), 1);

        list.as_mut().clear();
        assert_eq!(list.len(), 0);
    }

    #[test]
    fn test_unidirectional_iterators() {
        stack_pin_init!(let list = DoublyLinkedList::<UniquePtr<UniqueTestObject>>::new());

        list.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(1)).unwrap());
        list.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(2)).unwrap());
        list.as_mut().push_back(UniquePtr::try_new(UniqueTestObject::new(3)).unwrap());

        // 1. Test ForwardIterator from beginning
        let mut f_iter = list.forward_iter();
        assert_eq!(f_iter.next().unwrap().value, 1);
        assert_eq!(f_iter.next().unwrap().value, 2);
        assert_eq!(f_iter.next().unwrap().value, 3);
        assert!(f_iter.next().is_none());

        // 2. Test ReverseIterator from end
        let mut r_iter = list.reverse_iter();
        assert_eq!(r_iter.next().unwrap().value, 3);
        assert_eq!(r_iter.next().unwrap().value, 2);
        assert_eq!(r_iter.next().unwrap().value, 1);
        assert!(r_iter.next().is_none());

        list.as_mut().clear();
    }

    #[test]
    fn test_cursor_at() {
        stack_pin_init!(let list =
            DoublyLinkedList::<*mut TestObject, DefaultObjectTag, TrackingSize>::new());
        let mut obj1 = TestObject::new(1);
        let mut obj2 = TestObject::new(2);
        let mut obj3 = TestObject::new(3);

        unsafe {
            list.as_mut().push_back_raw(&mut obj1);
            list.as_mut().push_back_raw(&mut obj2);
            list.as_mut().push_back_raw(&mut obj3);
        }

        // Create a cursor at the second element (obj2).
        // SAFETY: `obj2` is a member of `list`.
        let mut cursor = unsafe { list.as_mut().cursor_at(&obj2) };
        assert_eq!(cursor.get().unwrap().value, 2);

        // Verify we can move next.
        cursor.move_next();
        assert_eq!(cursor.get().unwrap().value, 3);

        // Verify we can move prev from the original position.
        // SAFETY: `obj2` is a member of `list`.
        let mut cursor = unsafe { list.as_mut().cursor_at(&obj2) };
        cursor.move_prev();
        assert_eq!(cursor.get().unwrap().value, 1);

        // Verify we can erase via the cursor created at the element.
        // SAFETY: `obj2` is a member of `list`.
        let mut cursor = unsafe { list.as_mut().cursor_at(&obj2) };
        let erased = cursor.erase().unwrap();
        assert_eq!(unsafe { &*erased }.value, 2);

        // Verify list contents after erase.
        let mut iter = list.iter();
        assert_eq!(iter.next().unwrap().value, 1);
        assert_eq!(iter.next().unwrap().value, 3);
        assert!(iter.next().is_none());

        list.as_mut().clear();
    }

    #[test]
    fn test_clear_unsafe() {
        use core::mem::ManuallyDrop;
        use core::ptr::NonNull;

        stack_pin_init!(
            let list =
                DoublyLinkedList::<*mut TestObject, DefaultObjectTag, TrackingSize>::new()
        );

        // `clear_unsafe` on an empty list is a no-op and leaves the list empty and reusable.
        // SAFETY: The list is empty, so no former elements exist.
        unsafe {
            list.as_mut().clear_unsafe();
        }
        assert!(list.is_empty());
        assert_eq!(list.len(), 0);

        // Keep `obj1..=obj3` in `ManuallyDrop` so their `DoublyLinkedListNode::drop` debug
        // assertion (`!in_container()`) is not triggered after `clear_unsafe`.
        let mut obj1 = ManuallyDrop::new(TestObject::new(10));
        let mut obj2 = ManuallyDrop::new(TestObject::new(20));
        let mut obj3 = ManuallyDrop::new(TestObject::new(30));

        // SAFETY: `obj1`, `obj2`, and `obj3` are valid, unlinked objects that outlive their
        // presence in `list`.
        unsafe {
            list.as_mut().push_back_raw(&mut *obj1);
            list.as_mut().push_back_raw(&mut *obj2);
            list.as_mut().push_back_raw(&mut *obj3);
        }

        assert!(!list.is_empty());
        assert_eq!(list.len(), 3);

        // SAFETY: `obj1`, `obj2`, and `obj3` are wrapped in `ManuallyDrop` and are never used in
        // any list or cursor operation again after `clear_unsafe`.
        unsafe {
            list.as_mut().clear_unsafe();
        }

        assert!(list.is_empty());
        assert_eq!(list.len(), 0);
        assert!(list.front().is_none());
        assert!(list.back().is_none());
        assert!(list.iter().next().is_none());

        // Verify that the former nodes were not modified and still report `in_container()`.
        assert!(obj1.get_node().in_container());
        assert!(obj2.get_node().in_container());
        assert!(obj3.get_node().in_container());

        // Verify that the list can be reused with fresh nodes after `clear_unsafe`.
        let mut fresh1 = TestObject::new(40);
        let mut fresh2 = TestObject::new(50);
        // SAFETY: `fresh1` and `fresh2` are valid, unlinked stack objects cleared before drop.
        unsafe {
            list.as_mut().push_back_raw(&mut fresh1);
            list.as_mut().push_back_raw(&mut fresh2);
        }
        assert!(!list.is_empty());
        assert_eq!(list.len(), 2);
        let mut iter = list.iter();
        assert_eq!(iter.next().unwrap().value, 40);
        assert_eq!(iter.next().unwrap().value, 50);
        assert!(iter.next().is_none());
        list.as_mut().clear();
        assert!(list.is_empty());
        assert_eq!(list.len(), 0);

        // Also test a `NonNull<TestObject>` list with `NonTrackingSize`.
        stack_pin_init!(let nn_list = DoublyLinkedList::<NonNull<TestObject>>::new());
        // SAFETY: The list is empty.
        unsafe {
            nn_list.as_mut().clear_unsafe();
        }
        assert!(nn_list.is_empty());

        let mut nn_obj1 = ManuallyDrop::new(TestObject::new(1));
        let mut nn_obj2 = ManuallyDrop::new(TestObject::new(2));
        // SAFETY: `nn_obj1` and `nn_obj2` are valid, unlinked objects wrapped in `ManuallyDrop`
        // and never used again after `clear_unsafe`.
        unsafe {
            nn_list.as_mut().push_front_raw(NonNull::from(&mut *nn_obj1));
            nn_list.as_mut().push_front_raw(NonNull::from(&mut *nn_obj2));
            assert!(!nn_list.is_empty());
            nn_list.as_mut().clear_unsafe();
        }
        assert!(nn_list.is_empty());
        assert!(nn_obj1.get_node().in_container());
        assert!(nn_obj2.get_node().in_container());
    }

    // FFI Declarations
    unsafe extern "C" {
        // UniqueList Helpers
        fn cpp_create_unique_list() -> *mut c_void;
        fn cpp_destroy_unique_list(list: *mut c_void);
        fn cpp_unique_list_push_back(list: *mut c_void, item: *mut c_void);
        fn cpp_unique_list_pop_front(list: *mut c_void) -> *mut c_void;
        fn cpp_unique_list_is_empty(list: *mut c_void) -> bool;

        // RefList Helpers
        fn cpp_create_ref_list() -> *mut c_void;
        fn cpp_destroy_ref_list(list: *mut c_void);
        fn cpp_ref_list_push_back(list: *mut c_void, item: *mut c_void);
        fn cpp_ref_list_pop_front(list: *mut c_void) -> *mut c_void;
        fn cpp_ref_list_is_empty(list: *mut c_void) -> bool;

        // SharedUniqueObject Helpers
        fn cpp_create_unique_object(value: i32, destruction_flag: *mut bool) -> *mut c_void;
        fn cpp_get_unique_object_value(obj: *mut c_void) -> i32;

        // SharedRefObject Helpers
        fn cpp_create_ref_object(value: i32, destruction_flag: *mut bool) -> *mut c_void;
        fn cpp_get_ref_object_value(obj: *mut c_void) -> i32;
    }

    #[test]
    fn test_interop_rust_list_cpp_unique_objects() {
        let destroyed1 = AtomicBool::new(false);
        let destroyed2 = AtomicBool::new(false);

        unsafe {
            stack_pin_init!(let list = DoublyLinkedList::<UniquePtr<SharedUniqueObject>>::new());

            let cpp_raw1 = cpp_create_unique_object(1, destroyed1.as_ptr() as *mut bool);
            let cpp_raw2 = cpp_create_unique_object(2, destroyed2.as_ptr() as *mut bool);

            let obj1 = UniquePtr::from_raw(cpp_raw1 as *mut SharedUniqueObject);
            let obj2 = UniquePtr::from_raw(cpp_raw2 as *mut SharedUniqueObject);

            list.as_mut().push_back(obj1);
            list.as_mut().push_back(obj2);

            assert!(!destroyed1.load(Ordering::Relaxed));
            assert!(!destroyed2.load(Ordering::Relaxed));

            // Pop one
            let popped = list.as_mut().pop_front();
            assert!(popped.is_some());
            assert_eq!(popped.as_ref().unwrap().value, 1);

            // Drop popped -> should destroy in C++!
            drop(popped);
            assert!(destroyed1.load(Ordering::Relaxed));
            assert!(!destroyed2.load(Ordering::Relaxed));

            // Drop list -> should destroy remaining in C++!
        }
        assert!(destroyed2.load(Ordering::Relaxed));
    }

    #[test]
    fn test_interop_cpp_list_rust_unique_objects() {
        let destroyed1 = Arc::new(AtomicBool::new(false));
        let destroyed2 = Arc::new(AtomicBool::new(false));

        unsafe {
            let cpp_list = cpp_create_unique_list();
            assert!(cpp_unique_list_is_empty(cpp_list));

            let obj1 = UniquePtr::try_new(SharedUniqueObject::new(1)).unwrap();
            let obj2 = UniquePtr::try_new(SharedUniqueObject::new(2)).unwrap();

            // Set destruction flags
            let raw1 = UniquePtr::as_ptr(&obj1) as *mut SharedUniqueObject;
            (*raw1).destruction_flag = destroyed1.as_ptr() as *mut bool;
            let raw2 = UniquePtr::as_ptr(&obj2) as *mut SharedUniqueObject;
            (*raw2).destruction_flag = destroyed2.as_ptr() as *mut bool;

            // Push to C++ list (transfers ownership)
            cpp_unique_list_push_back(cpp_list, UniquePtr::into_raw(obj1) as *mut c_void);
            cpp_unique_list_push_back(cpp_list, UniquePtr::into_raw(obj2) as *mut c_void);

            assert!(!destroyed1.load(Ordering::Relaxed));
            assert!(!destroyed2.load(Ordering::Relaxed));

            // Pop one from C++
            let popped = cpp_unique_list_pop_front(cpp_list);
            assert!(!popped.is_null());
            assert_eq!(cpp_get_unique_object_value(popped), 1);

            // Convert back to Rust UniquePtr and drop -> should free in Rust!
            let popped_rust = UniquePtr::from_raw(popped as *mut SharedUniqueObject);
            drop(popped_rust);
            assert!(destroyed1.load(Ordering::Relaxed));
            assert!(!destroyed2.load(Ordering::Relaxed));

            // Destroy C++ list -> should destroy remaining in Rust!
            cpp_destroy_unique_list(cpp_list);
        }
        assert!(destroyed2.load(Ordering::Relaxed));
    }

    #[test]
    fn test_interop_rust_list_cpp_ref_objects() {
        let destroyed1 = AtomicBool::new(false);
        let destroyed2 = AtomicBool::new(false);

        unsafe {
            stack_pin_init!(let list = DoublyLinkedList::<RefPtr<SharedRefObject>>::new());

            let cpp_raw1 = cpp_create_ref_object(1, destroyed1.as_ptr() as *mut bool);
            let cpp_raw2 = cpp_create_ref_object(2, destroyed2.as_ptr() as *mut bool);

            let obj1 = RefPtr::from_raw(cpp_raw1 as *mut SharedRefObject);
            let obj2 = RefPtr::from_raw(cpp_raw2 as *mut SharedRefObject);

            list.as_mut().push_back(obj1);
            list.as_mut().push_back(obj2);

            assert!(!destroyed1.load(Ordering::Relaxed));
            assert!(!destroyed2.load(Ordering::Relaxed));

            // Pop one
            let popped = list.as_mut().pop_front();
            assert!(popped.is_some());
            assert_eq!(popped.as_ref().unwrap().value, 1);

            // Drop popped -> should destroy in C++!
            drop(popped);
            assert!(destroyed1.load(Ordering::Relaxed));
            assert!(!destroyed2.load(Ordering::Relaxed));

            // Drop list -> should destroy remaining in C++!
        }
        assert!(destroyed2.load(Ordering::Relaxed));
    }

    #[test]
    fn test_interop_cpp_list_rust_ref_objects() {
        let destroyed1 = Arc::new(AtomicBool::new(false));
        let destroyed2 = Arc::new(AtomicBool::new(false));

        unsafe {
            let cpp_list = cpp_create_ref_list();
            assert!(cpp_ref_list_is_empty(cpp_list));

            let obj1 = SharedRefObject::new_ref_counted(1);
            let obj2 = SharedRefObject::new_ref_counted(2);

            // Set destruction flags
            let raw1 = RefPtr::as_ptr(&obj1) as *mut SharedRefObject;
            (*raw1).destruction_flag = destroyed1.as_ptr() as *mut bool;
            let raw2 = RefPtr::as_ptr(&obj2) as *mut SharedRefObject;
            (*raw2).destruction_flag = destroyed2.as_ptr() as *mut bool;

            // Push to C++ list (transfers ownership)
            cpp_ref_list_push_back(
                cpp_list,
                RefPtr::into_raw(obj1) as *mut SharedRefObject as *mut c_void,
            );
            cpp_ref_list_push_back(
                cpp_list,
                RefPtr::into_raw(obj2) as *mut SharedRefObject as *mut c_void,
            );

            assert!(!destroyed1.load(Ordering::Relaxed));
            assert!(!destroyed2.load(Ordering::Relaxed));

            // Pop one from C++
            let popped = cpp_ref_list_pop_front(cpp_list);
            assert!(!popped.is_null());
            assert_eq!(cpp_get_ref_object_value(popped), 1);

            // Convert back to Rust RefPtr and drop -> should free in Rust!
            let popped_rust = RefPtr::from_raw(popped as *mut SharedRefObject);
            drop(popped_rust);
            assert!(destroyed1.load(Ordering::Relaxed));
            assert!(!destroyed2.load(Ordering::Relaxed));

            // Destroy C++ list -> should destroy remaining in Rust!
            cpp_destroy_ref_list(cpp_list);
        }
        assert!(destroyed2.load(Ordering::Relaxed));
    }
}
