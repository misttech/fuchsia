# MBMQ: Memory attribution for channel messages

See the [contents page](index.md) for other sections in this document.

[TOC]

## Introduction

The MBMQ model provides a basis for implementing *memory attribution*
for channel messages.

Zircon originally had no limit on the number of messages that could be
enqueued on a channel.  This meant the system would sometimes be
brought down by a misbehaving process that sends requests faster than
they are processed, allocating excessive amounts of memory for the
request messages.  Later a fixed limit was added, but that introduced
a new problem.  We discuss that problem and potential solutions below,
and relate them to the broader problem of memory attribution.

## Memory attribution: background and goals

Memory attribution is the problem of attributing who is responsible
for memory allocations, at some level of granularity such as processes
or components.  It is also known as *memory accounting* or *resource
accounting*; the latter may cover other resources besides memory.

The simpler use cases for memory attribution only involve measurement
of memory usage, for development or debugging purposes.

In more complex use cases, we may also want to prevent denial of
service (DoS) or handle out-of-memory (OOM) situations better.  For
this, we may want to do the following:

*   Prevent OOM from bringing down the whole system (whole-system OOM
    DoS)
*   Allow processes to communicate safely without one being able to
    cause DoS of the other.  This includes:
    *   Prevent DoS of a server process by a client process
    *   Prevent DoS of a client process by a server process

This may involve imposing limits on allocations, or it may involve
finding processes to blame for high memory use and to terminate in an
OOM situation (the "OOM killer" approach, like Linux's OOM killer).
We may or may not want guarantees that we can always reclaim memory.

The draft RFC "[Kernel-mediated Memory
Attribution](https://fuchsia-review.googlesource.com/c/fuchsia/+/867858)"
proposes a memory attribution system for Fuchsia that covers memory
allocated through VMOs.  Below we describe how a system like that
could be extended to cover channel messages.

## Problems with Fuchsia's current IPC mechanisms

As mentioned above, Zircon originally had no limit on the number of
messages that may be enqueued on a channel.  This had the problem that
a process could accidentally cause an out-of-memory DoS (denial of
service) of the entire system by queuing messages onto a channel
faster than they are processed.

That problem was partly addressed in March 2020 by the
[introduction](https://fuchsia-review.googlesource.com/c/fuchsia/+/369103)
of a fixed limit on the number of messages per channel, currently 3500
([kMaxPendingMessageCount](https://fuchsia.googlesource.com/fuchsia/+/7120a02d257174e8618fd9bfec60123b3f7e33d4/zircon/kernel/object/channel_dispatcher.cc#47)).
When a process tries to enqueue a message on a channel that is already
at that limit, the process receives a policy exception that usually
kills the process.  (It is possible to handle that exception, but that
is usually not done.)

That leaves the problem that a process can still cause DoS of the
entire system by writing many messages to multiple channels.

It also creates a new DoS problem.  A client process can now cause a
server process to be killed by sending a large number of request
messages to the server but never unqueuing the reply messages that the
server sends.  The reply messages will build up on the channel, and
the server will be terminated when it tries to enqueue one more reply
beyond the channel message limit.

## Attribution using MBOs

With MBOs, attribution works as follows:

The storage allocated by an MBO, including for its message contents,
is attributed to the MBO's creator, usually a client process.  (An
extension to this is to allow allocating MBOs from explicit
attribution objects, discussed below.)

Consequently, any request message sent using the MBO is attributed to
the client process, as one would expect.

Furthermore, a reply message sent by a server to a client is also
attributed to the client.  This may be counterintuitive -- because the
resource cost of allocating a reply message is attributed to the
receiver, not the sender -- but it prevents DoS of the server by the
client.

In addition, as long as the kernel imposes a moderate size limit on
messages (such as Zircon's current limit of 64k), this also prevents
DoS of the client by the server, because the server cannot allocate a
message greater than that size limit on the client's behalf.  (The
implications of relaxing that size limit are discussed below.)

Note that MBOs are not fixed-size.  An MBO is resized dynamically when
message contents are written into it.  This means that (for example)
if a client sends an MBO containing a request message of size 1k to a
server (and does so without prereserving extra space in the MBO), then
if the server writes a 64k reply into the MBO, this action by the
server will cause a further 63k to be attributed to the client.

In effect, MBOs allow a client to temporarily delegate to a server a
limited ability to allocate memory on the client's behalf.

## Relaxing the message size limit

If we relax the kernel's message size limit to allow large or
arbitrary sized messages, we get the problem that a server can cause
DoS of a client process by allocating a large reply message on the
client's behalf.

There are a couple of ways we could address that:

*   Introduce per-MBO size limits, allowing the client to set an
    explicit size limit on the MBO it sends to a server.

*   If we have explicit attribution objects, the client can create a
    separate attribution object and allocate MBOs from it for the
    purposes of interacting with a particular server.

    The benefits of this depend on how we are using attribution
    objects.  It could mean that if the server misbehaves by creating
    an excessively large reply that contributes to an OOM situation,
    the separate attribution object gets blamed and reclaimed without
    killing the client process.

    This approach avoids the need to set a specific size limit.
