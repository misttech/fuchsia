# Starnix kernel

The Starnix kernel is responsible for implementing the Linux Userspace API
(UAPI) on Fuchsia. The Starnix kernel [intercepts syscalls][starnix-syscalls]
from Linux processes and implements the semantics required to make Linux
programs run correctly. This document describes the internal structure of the
Starnix kernel.

## Approach

Starnix aims to run unmodified Linux binaries. To run these binaries *as they
are*, Starnix aims for bug-for-bug compatibility with the Linux kernel. Our
general approach to interoperating at this level of fidelity is extensive
testing. To understand the semantics of the Linux UAPI, we write tests to probe
the edge and corner cases of the interface. Once these tests pass on the Linux
kernel, we start running them in our continuous integration infrastructure,
marking them as failing if they do not pass on Starnix. As we improve Starnix,
these tests will eventually "pass unexpectedly," which means we can mark them as
passing.

### Unit versus userspace tests

In the vast majority of cases, we prefer to [test Starnix by writing userspace
programs][starnix-syscall-tests], which we compile as Linux binaries. Using this
approach, we can run the identical tests against Starnix and against the Linux
kernel, ensuring that the two behave the same way.

In some cases, we test Starnix using unit tests, which run inside the Starnix
kernel and depend on implementation details of Starnix that are not exposed
through the UAPI. This approach has the disadvantage that we cannot ensure the
behavior expected by these tests matches the behavior of the Linux kernel. In
addition, these tests are more likely to require maintenance as we evolve the
implementation. However, this approach is useful because there are some
scenarios that are much easier to test with access to the internals of the
kernel.

In some cases, we use integration tests that run userspace programs but have
test assertions on the Fuchsia side. These tests are uncommon but useful to
verify invariants that are not easily accessible from userspace.

### `track_stub!`

The semanatics of the Linux UAPI are vast. Even an individual syscall or
pseudofile can have more functionality that we are prepared to implement at any
given time. To keep track of which semantics we have not yet implemented, we use
the `track_stub!` macro. This macro documents which codepaths are missing
functionality, ties those codepaths to bugs in the bug tracker, and instruments
the Starnix kernel binary to let us observe when a Linux program runs these
codepaths.

Conceptually, the original implementation of the Starnix kernel was a `match`
statement on the syscall number that "implemented" every syscall by calling the
`track_stub!` macro. As we tried to run more and more sophistociated Linux
programs, we were forced to replace instances of this macro with actual syscall
implementations. However, many syscalls have options or different modes. We
pushed the `track_stub!` macro inside these syscalls to "implement" the missing
options or modes.

Internally, we have a [dashboard][starnix-not-implemented] that has statistics
for how often, and in which scenarios, the `track_stub!` macro is executed.

As we continue to implement more functionality in Starnix, we should continue to
use the `track_stub!` macro to track our progress.

## Structure

The Starnix kernel is implemented as a number of Rust crates. The Starnix kernel
itself is a crate that just contains `main.rs`, which is the main entry point,
but not much else. Instead, the core machinery for the kernel is in the
`starnix_core` crate, which is in the middle of the dependency graph.

### Process model

The Starnix kernel runs as a collection of Fuchsia processes in a job. There is
one Fuchsia process for every Linux process (technically every Linux address
space because there is no such thing as a "process" in the Linux UAPI), with one
additional Fuchsia process. The additional Fuchsia process is the *initial*
process, into which the Starnix kernel binary is loaded and begins executing.
This process has the [Starnix shared address space][shared-address-space] but
does not have a [restricted address space][restricted-address-space].

The main thread for the initial process runs a normal Fuchsia async executor and
responds to FIDL requests. For example, this thread services requests for the
kernel to run a [Starnix container][starnix-container]. This process also
contains background threads, called *kthreads*, which run background tasks for
the kernel. These threads need to run in the initial process so that they can
outlive whichever userspace process caused them to be created.

Warning: Rather than using the normal Rust facilities for spawning a thread
(e.g., `std::thread::spawn`), use `kernel.kthreads` to spawn a kthread in the
initial process, which outlives all the other processes. See [Spawning Threads
in Starnix][spawning-threads-in-starnix] for more information.

### `starnix_syscall_loop`

After being created, userspace threads enter into the main *syscall loop*, which
is implemented by the [`starnix_syscall_loop` crate][starnix-syscall-loop]. In
this loop, the thread enters user mode (i.e., restricted mode) with a particular
machine state. Eventually, the Linux program exits user mode and control of the
thread returns to the Starnix kernel. The most common reason to exit user mode
is that the program issued a syscall, but the thread can exit user mode for
other reasons, such as an exception or being *kicked* back to kernel mode.

Whenever the Linux program issues a syscall, the `dispatch_syscall` function in
the `starnix_syscall_loop` crate decodes the syscall and calls the appropriate
syscall implementation function. Putting the `dispatch_syscall` function in a
separate crate from the syscall implementations lets us shard the implementation
of syscalls across multiple crates. At present, the vast majority of the syscall
implementations are in the `starnix_core` crate, but we [plan][bug-470456509] to
move them out of that crate to reduce the complexity of the `starnix_core` crate
over time.

### Modules

Many features of the Starnix kernel are implemented as
[*modules*][starnix-modules]. When initializing, the Starnix kernel initializes
each module. Most modules are guarded by feature flags, which means the modules
are initialized only when the corresponding feature flag is enabled. During
initialization, modules typically register themselves with `starnix_core`. For
example, a module that implements a device will register itself as the handler
for the appropriate major and minor device numbers with the `DeviceRegistry`.
Similarly, a module that implements a file system will register itself with the
`FsRegistry`.

Modules are not called directly by `starnix_core`. Instead, they implement the
appropriate traits for the abstractions they provide. For example, modules that
provide devices implement the `DeviceOps` trait and modules that provide file
systems implement the `FileSystemOps` trait. These traits often return objects
that implement other traits, such as `FileOps` and `FsNodeOps`.

Modules that need to store kernel-global state should use the `kernel.expando`
object rather than defining their own fields on the `Kernel` struct. The kernel
expando is keyed by Rust type, which lets each module define its own storage
slot without risking colliding with other modules. Additionally, this mechanism
avoids committing resources for modules that are not being used.

### `starnix_core`

The [`starnix_core` crate][starnix-core] contains the core machinery of the
Starnix kernel. The crate is responsible for tasks, memory management, device
registration, and the virtual file system (VFS). These subsystems are all
tightly interrelated, with many circular dependencies. Much of the design of
`starnix_core` is documented in rustdoc comments in the source code.

### Libraries

When code is needed by `starnix_core`, modules, or other parts of Starnix, but
does not depend on `starnix_core`, we prefer to implement that code as a
separate crate in the [`//src/starnix/lib` directory][starnix-lib]. Using
separate crates makes the code easier to understand because the dependency graph
of the code is constrained. Additionally, using separate crates improves
incremental build times because this code does not need to be rebuilt when
`starnix_core` has been modified.

### UAPI

The [`starnix_uapi` crate][starnix-uapi] is at the bottom of the dependency
diagram for the Starnix kernel. This crate defines ergonomic Rust types for
concepts defined in the Linux UAPI. For example, the Linux UAPI might define a
`u32` with semantics for various bits. To make using this type more ergonomic,
the `starnix_uapi` crate might use the `bitflags!` macro to define a Rust type
for these same bits.

The `UserAddress` type, which represents a userspace address, is defined in the
`starnix_uapi` crate. The Starnix kernel uses this type instead of a Rust
pointer when working with pointers to userspace memory to avoid accidentally
dereferencing such a pointer. This approach avoids two hazards. First, userspace
might supply a kernel address instead of a userspace address, which could trick
the kernel into manipulating its own memory. Second, the kernel might panic when
accessing userspace address unless the kernel uses the `usercopy` machinery to
perform that access safely.

The `starnix_uapi` crate depends on the `linux_uapi` crate, which is
automatically generated, using `bindgen`, from the C definition of the Linux
UAPI used by Linux programs.

<!-- Reference links -->

[starnix-core]: https://fuchsia.googlesource.com/fuchsia/+/main/src/starnix/kernel/core/
[starnix-lib]: https://fuchsia.googlesource.com/fuchsia/+/main/src/starnix/lib/
[starnix-syscalls]: /docs/concepts/starnix/syscalls.md
[starnix-syscall-tests]: https://fuchsia.googlesource.com/fuchsia/+/main/src/starnix/tests/syscalls/cpp/
[starnix-syscall-loop]: https://fuchsia.googlesource.com/fuchsia/+/main/src/starnix/lib/starnix_syscall_loop/
[starnix-uapi]: https://fuchsia.googlesource.com/fuchsia/+/main/src/starnix/lib/starnix_uapi/
[starnix-vfs]: /docs/concepts/starnix/vfs.md
[starnix-container]: /docs/concepts/starnix/containers.md
[starnix-not-implemented]: http://go/starnix-not-implemented
[starnix-modules]: https://fuchsia.googlesource.com/fuchsia/+/main/src/starnix/modules/
[bug-470456509]: https://fxbug.dev/470456509
[shared-address-space]: /docs/concepts/starnix/syscalls.md#shared-starnix-instance
[restricted-address-space]: /docs/concepts/starnix/syscalls.md#running-a-linux-program-in-restricted-mode
[spawning-threads-in-starnix]:/docs/development/starnix/common-coding-patterns-in-starnix.md#spawning-threads-in-starnix
