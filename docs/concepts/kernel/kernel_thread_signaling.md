# Kernel Thread Signaling

## About

This document describes thread signaling, a Zircon kernel mechanism used to
implement thread suspend and kill operations.  Thread signaling is not related
to [object signaling](signals.md).

The target audience is kernel developers and anyone interested in understanding
how suspend and kill operations work in the kernel.

## Suspend and Kill Are Requests

Suspend and kill are operations that can be performed on threads.  Both of these
operations are asynchronous in that the caller must wait for the operation to
complete.  Inside the kernel, these operations are implemented as instance
methods on the Thread struct:

[`Thread::Suspend`] - Request a thread to suspend its execution until
it is resumed via Thread::Resume.  Suspend is used to implement
debuggers.  Once suspended, a thread's register state can be
read/written prior to resuming it.  This operation is exposed to user
mode via [`zx_task_suspend()`].

[`Thread::Kill`] - Request a thread to terminate itself.  This
operation is not directly exposed to user mode.  That is, attempting
to [`zx_task_kill()`] a thread is an error.  However, this operation
is indirectly exposed via process destruction, both voluntary and
involuntary.

Notice that both of these operations are described as requests.  The caller is
requesting that the target suspend or, in the case of kill, terminate its
execution.  The caller has no ability to forcibly suspend or terminate the
target.  While the target cannot refuse the request, it can delay action until
the appropriate time and place.  This is a key element of the design.

To understand why these operations are requests, consider the alternative of
forcibly killing or suspending a thread.  If a thread is forcibly killed while
holding a resource (like a mutex) then it won't get the chance to free the
resource before it's destroyed.  You could end up with memory leaks, permanently
locked locks, corrupted data structures, all sorts of bad stuff.

By modeling kill and suspend as requests that can only be performed by the
target thread, we provide a way for the target to free its resources and perform
any necessary cleanup before it stops executing, temporarily (in the case of
suspend) or permanently (in the case of kill).

## Safe Points

Before we cover how kill and suspend requests are issued, let's talk about the
safety of thread termination.

There is one place where it's always safe for a thread to suspend or terminate
its execution, the "edge" of the kernel, just before returning from the kernel
back to user mode.  Before returning to user mode, the thread unwinds its
callstack, executing the destructors of any RAII objects.  By the time it has
reached the edge and is about to return to user mode, there will be nothing left
on the kernel stack.  It is here that a thread may safely suspend or terminate
its execution.

Concretely, there are two safe points at which a thread may suspend or
terminate.  They are just before returning to user mode from a syscall and just
before returning to user mode from an exception/fault/interrupt handler
(exception handler, for short).

Note, exception handlers are not just invoked when executing in user mode.  They
can also be invoked when executing in kernel mode.  When returning back to
kernel mode it is not safe to suspend or terminate because the outer kernel mode
context may still be holding a resource.  In other words, an exception handler
is only a safe point when it is triggered from a user mode context.

## Sending a Signal

So we know that kill and suspend are merely requests and that it's up to the
target thread to decide when and how to fulfill the request.  We also know that
the only safe places for a thread to suspend or terminate itself are at the
edges of the kernel, just before returning to user mode.  How do thread signals
fit into all this?

Thread signals are the mechanism by which suspend and kill are requested.  Each
Thread object has a field containing the set of asserted signals.  There's a bit
for suspend, `THREAD_SIGNAL_SUSPEND`, and a bit for kill, `THREAD_SIGNAL_KILL`.

Requesting a thread to suspend or terminate is achieved by setting the
appropriate bit on the target Thread object and then, depending on the target's
state, poking it in some way to ensure it reaches a safe point in a timely
fashion.  The bit is set with an atomic OR while holding the target thread's
lock.  The exact type of poke depends on the target thread's state:
sleep/blocked, suspended, or running.  Note, there are two flavors of
sleeping/blocked, interruptible and uninterruptible.  We'll focus on
interruptible.  If the target is blocked uninterruptibly, the sender simply sets
the bit and the signal is acted on at the next safe point after the thread
wakes up on its own.

### Sleeping or Blocked

If the target thread is sleeping or blocked then by definition it's not running,
but it's in the kernel.  Since only a running thread can check its signals we
must wake or unblock it.  When a thread is unblocked or woken, it's given a
`zx_status_t`.  Usually the value is `ZX_OK` or `ZX_ERR_TIMED_OUT`.  However
when waking a thread early like this we use a special `zx_status_t` value,
`ZX_ERR_INTERNAL_INTR_KILLED` in the case of a kill operation and
`ZX_ERR_INTERNAL_INTR_RETRY` in the case of a suspend operation.

When a thread is woken/unblocked, it will see the `zx_status_t` result and begin
backing out of the kernel, unwinding its stack.  In general, any kernel function
returning one of the two special values will cause its caller to immediately
return, propagating that value.

Eventually, when the stack has unwound, the thread will be at the edge, a safe
point.  It is here, just before returning to user mode, that the thread checks
its signals once more and acts on them by calling
[`arch_iframe_process_pending_signals()`] or
[`x86_syscall_process_pending_signals()`].  Both of these end up in
[`Thread::Current::ProcessPendingSignals()`], which handles kill before suspend
and keeps going until there are no signals left.

One thing worth noting about the blocked case is that unblocking a thread
requires locking the entire priority inheritance chain that the thread is part
of.  If the sender can't get all of those locks it backs off and retries.

### Suspended

Just like the sleeping/blocked case, the thread must resume execution in order
for it to be killed.  In the case of kill, the thread will be unblocked with
`ZX_ERR_INTERNAL_INTR_KILLED` and unwind until just before returning to user
mode where it acts on the signal.

### Running

The target thread could be running user code or kernel code.  If it's running
user code, then we'll need to force it to enter the kernel where it can check
the signals field of its Thread struct.  If it's running kernel code, then we'll
have to trust that it will check for pending signals in a reasonable time frame.

The sender can't know if the target is in kernel mode or user mode so it behaves
the same in either case.  The sender sends an Inter-processor Interrupt (IPI) to
the CPU on which the target is currently running.  Part of the interrupt
handlers job is to check for and optionally process pending signals.

If the handler was invoked in a user context, that is, the CPU was in user mode
at the time of the interrupt, then it's a safe point to suspend/terminate and
the handler will call [`arch_iframe_process_pending_signals()`].

If, however, the handler was invoked in a kernel context, then the handler will
do nothing because it can't know the state of the thread at the point it was
interrupted.  It's not safe to suspend/terminate here.  Instead, the handler
will return to the kernel context from which it was invoked and rely on this
outer context to eventually notice the signal and reach a safe point.

You may be wondering if the IPI is really necessary.  There are two cases where
it's critical.  The first is when the target thread is running in user mode and
simply not entering the kernel on its own.  On a lightly loaded system with no
other interrupt traffic, a thread may not enter the kernel for extended periods
of time, or ever in the case of an infinite loop.  We need the IPI in this case
to ensure the target thread observes and processes any pending signals in a
timely manner.  The second is when the target thread is performing a long
running operation in the kernel, but not checking for pending signals.  These
are rare, but do exist.  The best example would be the execution of a guest OS
via [`zx_vcpu_enter()`].  The interrupt would cause a VMEXIT back to the host
kernel where it can check for pending signals and unwind.

## Putting It All Together

Let's walk through an example to see how this all works.  Imagine thread A is
suspending thread B, as B is performing a [`zx_port_wait()`].  Depending on
exactly when the operation is performed, we can end up in one of several
different scenarios.  We'll examine each scenario briefly.

### Scenario 1: Suspend just before syscall, running in user mode

Thread A issues the suspend just before thread B begins its [`zx_port_wait()`]
syscall.  Thread B is still in user mode and is running.  Thread A sets thread
B's `THREAD_SIGNAL_SUSPEND` bit and issues an IPI to thread B's current CPU.
Thread B's CPU takes the interrupt and calls the interrupt handler.  Just before
returning back to user mode, thread B checks its pending signals.  Seeing that
`THREAD_SIGNAL_SUSPEND` is set, it suspends itself.  Here's a sketch of thread
B's callstack:

```
suspend_self()
interrupt_handler()
---- interrupt ----
user code
```

Later on, after being resumed, thread B will return back to user mode as if
nothing happened.

### Scenario 2: Suspend during syscall, prior to blocking

Thread A issues the suspend after thread B has entered the kernel to
perform a [`zx_port_wait()`] syscall.  Thread B is executing kernel
code and hasn't yet blocked.  Just like Scenario 1, thread A issues an
IPI, which causes thread B to check for pending signals:

```
interrupt_handler()
---- interrupt ----
Semaphore::wait()
PortDispatcher::dequeue()
sys_port_wait()
syscall_dispatch()
---- syscall ----
vdso
zx_port_wait()
user code
```

The frame names in the following diagrams are those of the Rust implementation.
The C++ `WaitQueue::BlockEtc()` and others are thin wrappers over the same
code.

However, this time the interrupt handler sees that it was invoked in kernel
context rather than user context so it does not suspend itself.  Instead it
returns back to the kernel context in which it was invoked.  Thread B reaches
the core of the `zx_port_wait()` operation, the point at which it will block if
there are no packets available.  Thread B sees there are no packets available
and prepares to block:

```
WaitQueue::block_etc_preamble()
Semaphore::wait()
PortDispatcher::dequeue()
sys_port_wait()
syscall_dispatch()
---- syscall ----
vdso
zx_port_wait()
user code
```

Just before blocking, it checks for pending signals and sees that it has been
asked to suspend.  [`Thread::check_kill_or_suspend_signal()`] returns
`ZX_ERR_INTERNAL_INTR_KILLED` if the kill bit is set and
`ZX_ERR_INTERNAL_INTR_RETRY` if only the suspend bit is set.  Instead of
blocking, thread B returns `ZX_ERR_INTERNAL_INTR_RETRY` and the callstack
unwinds to the edge, just prior to returning to user mode:

```
WaitQueue::block_etc_preamble() ZX_ERR_INTERNAL_INTR_RETRY
Semaphore::wait()                           |
PortDispatcher::dequeue()                   |
sys_port_wait()                             |
syscall_dispatch()                          V
---- syscall ----
vdso
zx_port_wait()
user code
```

Here the thread checks for pending signals and suspends itself.  Upon being
resumed, the thread returns to user mode (to the vDSO) with the status result
`ZX_ERR_INTERNAL_INTR_RETRY`.  The vDSO has [special logic] for handling
syscalls that return `ZX_ERR_INTERNAL_INTR_RETRY`, it simply reissues the
syscall with the original arguments:

```
suspend_self()                  ZX_ERR_INTERNAL_INTR_RETRY
syscall_dispatch()                                   |
---- syscall ----                                    |      A
vdso                                                 |______|
zx_port_wait()
user code
```

[special logic]: https://cs.opensource.google/fuchsia/fuchsia/+/main:zircon/kernel/lib/userabi/vdso/syscall-wrappers.cc;drc=baf4cdf84731cc863cfa7967a6f0d8e0f326a2ae;l=19

### Scenario 3: Suspend while blocked in kernel

Thread A issues the suspend after thread B has entered the kernel and blocked,
waiting for a port packet.  Thread A sees that thread B is blocked so it
unblocks thread B with the value `ZX_ERR_INTERNAL_INTR_RETRY`.  From this point
on the behavior matches that of Scenario 2.  The call returns to user mode where
it is retried by the vDSO:

```
blocked                           ZX_ERR_INTERNAL_INTR_RETRY
WaitQueueBase::block_etc_postamble()                   |
Semaphore::wait()                                      |
PortDispatcher::dequeue()                              |
sys_port_wait()                                        |
syscall_dispatch()                                     |
---- syscall ----                                      |      A
vdso                                                   |______|
zx_port_wait()
user code
```

### Scenario 4: Suspend after unblocking, before returning from kernel

While thread B was blocked, waiting on a port packet, a packet arrived,
unblocking it (with `ZX_OK`):

```
blocked                            ZX_OK
WaitQueueBase::block_etc_postamble() |
Semaphore::wait()                    |
PortDispatcher::dequeue()            V
sys_port_wait()
syscall_dispatch()
---- syscall ----
vdso
zx_port_wait()
user code
```

Thread B is now unwinding toward user mode when thread A issues a suspend.
Thread A sets the bit, see that thread B is marked as running so it sends an
IPI.  Similar to the "Suspend just before syscall" case, the interrupt handler
executes:

```
interrupt_handler()
---- interrupt ----
PortDispatcher::dequeue()
sys_port_wait()
syscall_dispatch()
---- syscall ----
vdso
zx_port_wait()
user code
```

However, this time it does not check for pending signals because the handler
interrupted kernel context rather than user context.  The handler completes and
thread B continues to unwind.  Eventually, thread B reaches the edge and is
about to return from the syscall to user mode.  Here it checks for pending
signals, sees `THREAD_SIGNAL_SUSPEND` and suspends itself:

```
suspend_self()
syscall_dispatch()
---- syscall ----
vdso
zx_port_wait()
user code
```

Upon being resumed, it will return to user mode with the status result that
unblocked it (`ZX_OK`):

```
syscall_dispatch()    ZX_OK
---- syscall ----       |
vdso                    V
zx_port_wait()
user code
```

## Recap

The key points to take away are:

1. You can't forcibly suspend or kill a thread.  You can only ask it to suspend
   or terminate itself.

2. Thread signals are the mechanism for asking a thread to suspend or terminate.

3. Threads should only suspend or terminate their execution at specific points
   within the kernel.  In particular, a thread may only suspend or terminate
   execution when it holds no resources (e.g. locks) and is about to return from
   kernel mode to user mode.

4. In order to remain responsive, long running kernel operations must
   periodically check for pending signals and return if any are set.


[`Thread::Suspend`]: https://cs.opensource.google/fuchsia/fuchsia/+/main:zircon/kernel/include/kernel/thread.h;l=858;drc=f16b887f4148eb346aec8ed4abc5d8825de154f4
[`Thread::Kill`]: https://cs.opensource.google/fuchsia/fuchsia/+/main:zircon/kernel/include/kernel/thread.h;l=868;drc=f16b887f4148eb346aec8ed4abc5d8825de154f4
[`Thread::SuspendOrKillInternal`]: https://cs.opensource.google/fuchsia/fuchsia/+/main:zircon/kernel/kernel/thread.cc;l=512;drc=f16b887f4148eb346aec8ed4abc5d8825de154f4
[`Thread::Current::ProcessPendingSignals()`]: https://cs.opensource.google/fuchsia/fuchsia/+/main:zircon/kernel/kernel/thread.cc;l=1380;drc=f16b887f4148eb346aec8ed4abc5d8825de154f4
[`Thread::check_kill_or_suspend_signal()`]: https://cs.opensource.google/fuchsia/fuchsia/+/main:zircon/kernel/kernel/thread.rs;l=1083;drc=f16b887f4148eb346aec8ed4abc5d8825de154f4
[`arch_iframe_process_pending_signals()`]: https://cs.opensource.google/fuchsia/fuchsia/+/main:zircon/kernel/arch/arm64/exceptions_c.cc;l=615;drc=b07c2008ea00c1854c66fc30648e518a27b6fd34
[`x86_syscall_process_pending_signals()`]: https://cs.opensource.google/fuchsia/fuchsia/+/main:zircon/kernel/arch/x86/exceptions_c.cc;l=607;drc=80c3a245853c67fc4dfdc68e6470f26951f506c1
[`zx_task_suspend()`]: /reference/syscalls/task_suspend.md
[`zx_task_kill()`]: /reference/syscalls/task_kill.md
[`zx_port_wait()`]: /reference/syscalls/port_wait.md
[`zx_vcpu_enter()`]: /reference/syscalls/vcpu_enter.md
