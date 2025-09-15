# Fuchsia RCU Design

This document describes how the Fuchsia RCU crate uses atomics and the C++20
memory model to synchronize access to data.

## Usage Model

You can use `RcuCell` to create a cell that can be used concurrently from
multiple threads. Many threads can read from the cell concurrently and do not
block on writers. When the cell is written, reads may continue to see the old
value of the cell for some period of time.

```rust
struct MyStruct {
  a: usize,
  b: usize,
}

struct SharedStruct {
  foo: RcuCell<MyStruct>
}
```

### Read

To read the value of the cell, use the `read()` method:

```rust
fn my_reader(x: Arc<SharedStruct>) {
  let guard = x.foo.read();
  println!("foo.a: {}", guard.a);
  println!("foo.b: {}", guard.b);
}
```

The values of `a` and `b` will be consistent with each other as long as their
are read from the same `RcuReadGuard` returned from `read()`. However, another
thread running concurrently might see different values for `a` and `b` when
reading this cell.

### Write

To write the value of the cell, use the `set()` method:

```rust
fn my_writer(x: Arc<SharedStruct>) {
  x.foo.set(MyStruct { a: 42, b: 24 });
}
```

After `set` returns, future calls to `read()` will observe the new values for
`a` and `b`, but concurrent readers can still observe the old values. The
storage for the old values will not be reclaimed until all the concurrent
readers have dropped their read guards and the RCU state machine has made
sufficient progress.

### Progress requirements

In order for the RCU state machine to make progress, the program using
`fuchsia-rcu` must periodically call `rcu_synchronize`. Otherwise, memory
allocated during `RcuCell::set` will never be freed.

## Low-level interface

`RcuCell` and similar high-level data structures are built on top of a low-level
interface to the RCU state machine. This interface synchronizes access to
objects referenced through `std::sync::atomic::AtomicPtr`.

The RCU library also provides `RcuPtr`, which is a thin wrapper around an
`AtomicPtr` and the low-level interface described below. Clients that require
low-level access to the RCU state machine should use `RcuPtr` rather than
calling the low-level interface directly.

### Read

The interface for reading objects managed by RCU is as follows:

```rust
fn rcu_read_lock()
fn rcu_read_unlock()
fn rcu_read_pointer(ptr: &AtomicPtr<T>) -> *const T
```

To read from an object, a client must first call `rcu_read_lock()` and then call
`rcu_read_pointer()`. The returned pointer remains valid until the balancing
call to `rcu_read_unlock()`, after which time the client must not dereference
the pointer.

A given thread can acquire nested RCU read locks. Threads running concurrently
might receive pointers to different objects when calling `rcu_read_pointer()`
with the same `AtomicPtr`.

## Write

The interface for writing objects managed by RCU is as follows:

```rust
fn rcu_assign_pointer(ptr: &AtomicPtr<T>, new_ptr: *mut T)
fn rcu_replace_pointer(ptr: &AtomicPtr<T>, new_ptr: *mut T) -> *mut T
fn rcu_call(callback: impl FnOnce() + Send + Sync + 'static)
fn rcu_drop<T: Send + Sync + 'static>(value: T)
```

To write an object, a client first creates a new instance of the object and then
uses either `rcu_assign_pointer()` or `rcu_replace_pointer()` to store a pointer
to that object in an `AtomicPtr`. The `rcu_assign_pointer()` operation discards
whatever value was previously stored in this pointer, whereas the
`rcu_replace_pointer()` operation returns the previous value of the `AtomicPtr`.

Typically, the caller wishes to clean up whatever object was previously stored
in the `AtomicPtr`. However, the caller cannot clean up that object immediately
because there might be other threads that are currently reading from that
object. Instead, the caller uses `rcu_call()` to schedule a callback that will
run after all the currently in-flight readers have completed.

The RCU library provides `rcu_drop()`, which is a convenience function that
schedules the given object to be dropped once all the currently in-flight
readers have completed.

### Progress

The interface for progressing the RCU state machine is as follows:

```rust
fn rcu_synchronize()
```

Clients must call at least one of these functions periodically to ensure that
callbacks scheduled with `rcu_call()` eventually happen.

The `rcu_synchronize()` function blocks until the RCU state machine has advanced
sufficiently to call all the callbacks that were scheduled prior to calling
`rcu_synchronize()`.

## State Machine

To show that the RCU state machine provides the necessary synchronization
properties, we need to show two properties about a given object `o` protected by
the RCU mechanism:

 1. All the writes to `o` happen before all the reads from `o`.
 2. All the reads from `o` happen before we run the RCU callback (i.e., the
    callback scheduled with `rcu_call()`) associated with `o`.

First, we will state the forms of the relevant operations and then we will show
that they have desired synchronization properties.

### States

#### Idle

The RCU state machine starts in the *Idle* state. In this state, readers can
begin read operations, which are counted using the `read_counters`. The state
machine remains in this state until the next call to `rcu_synchronize`.

There are no preconditions for leaving the *Idle* state. The post condition for
leaving the *Idle* state is that the `callback_chain` has been moved to the
`waiting_callbacks` queue, the `generation` counter has been increased, and the
state machine is in the *Waiting* state.

#### Waiting

In the *Waiting* state, existing readers complete and decrement their
`read_counter`. New readers can begin read operations, and increment a different
`read_counter`. The state machine remains in this state until the precondition
for leaving the *Waiting* state has been obtained.

The precondition for leaving the *Waiting* state is that the `read_counter` for
the previous generation has reached zero. This condition indicates that all the
read operations that were in flight when the state machine entered the *Waiting*
state have completed.

The postcondition for leaving the *Waiting* state is that the front entry in the
`waiting_callbacks` queue has been removed (advancing all the callbacks in the
queue) and the state machine is in the *Idle* state.

After leaving the *Waiting* state, the set of callbacks removed from the
`waiting_callbacks` queue run, potentially on a different thread.

### Operations

#### Writes

All the writes to `o` have the following form:

```rust
[create and initialize o]
let p = [address of o];
rcu_assign_ptr(&ptr, p); // or rcu_replace_pointer()
```

#### Reads

All the reads from `o` have the following form:

```rust
rcu_read_lock();
let p = rcu_read_pointer(&ptr);
[read from o via p]
rcu_read_unlock();
```

#### Callbacks

All the callbacks associated with `o` have the following form:

```rust
let another_pointer = [address of another object or null];
let p = rcu_replace_pointer(&ptr, another_pointer);
rcu_call(|| {
  [operate on p, e.g., freeing o]
});
```

### Writes Happen Before Reads

Readers call `rcu_read_pointer()` before reading from `o`, which contains an
`Ordering::Acquire` load of the `AtomicPtr` to `o` at synchronization point
`[D]`. This load _synchronizes-with_ the `rcu_assign_ptr()` operation performed
by the writer because `rcu_assign_ptr()` contains an `Ordering::Release` store
of the same `AtomicPtr` at synchronization point `[E]`. For this reason, the
write to `o` _happens-before_ the read from `o`.

### Reads Happen Before Callbacks Run

Assume, without loss of generality, that the RCU state machine starts in the
*Idle* state and that the `generation` is even. Consider a given reader thread
reading from `o` and another thread scheduling a callback associated with `o`.
Before we run that callback, the state machine will need to advance through the
*Idle* and *Waiting* states twice.

In this sequence, there are three atomic operations with `Ordering::SeqCst`:

 1. Synchronization point `[A]` in `rcu_read_lock()`.
 2. Synchronization point `[C1]` the first time through the *Waiting* state in
    `rcu_synchronize()`.
 3. Synchronization point `[C2]` the second time through the *Waiting* state in
    `rcu_synchronize()`.

The memory model guarantees that these three operations happen in a single total
order. The mutex that protects the `rcu_control_block` ensures that `[C1]` is
always before `[C2]` in the single total order. The argument for correctness is
different depending on whether `[A]` is before or after `[C1]` in the single
total order.

#### `[A]` is before `[C1]`

When the reader loads the `generation` count, the reader will either load an
even or an odd generation number. If the `generation` count is even, then the
load at `[C1]` will _synchronize-with_ the decrement in `rcu_read_unlock()`
(synchronization point `[B]`) because both operate on `read_counter[0]` (recall
that we assumed the `generation` count was originally even). If the loaded
`generation` count is odd, then the load at `[C2]` will _synchronize-with_ the
decrement in `rcu_read_unlock()` because both operate on `read_counter[1]`.
Either way, the read from `o` _happens-before_ the state machine exits
the *Waiting* state the second time.

#### `[C1]` is before `[A]`

We will show the _happens-before_ relation for the following synchronization
points, in order: `[F]` in `rcu_replace_pointer()`, `[G]` in `rcu_call()`, `[H]`
in the *Idle* state of `rcu_synchronize()`, `[C1]` in the *Waiting* state
of `rcu_synchronize()`, `[A]` in `rcu_read_lock()`, and `[D]` in
`rcu_read_pointer()`:

 * `[F]` _happens-before_ `[G]` because `[F]` is _sequenced-before_ `[G]`.
 * `[G]` _happens-before_ `[H]` because `[G]` _synchronizes-with_ `[H]`.
 * `[H]` _happens-before_ `[C1]` because of the mutex that protects the
   `rcu_control_block`.
 * `[C1]` _happens-before_ `[A]` because `[C1]` precedes `[A]` in the single
   total order.
 * `[A]` _happens-before_ `[D]` because `[A]` is _sequenced-before_ `[D]`.

Therefore, the pointer to `o` stored in the `AtomicPtr` is replaced with another
value before being loaded by the reader, which means the reader does not
actually read from `o`. The callback runs after all the reads from `o` because
there are no reads from `o`.
