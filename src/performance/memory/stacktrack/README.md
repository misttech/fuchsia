# Stacktrack

Stacktrack is a profiling tool that records the peak stack usage observed for
each thread in a process.

## VMO format

The Stacktrack VMO consists of a header followed by an array of nodes, indexed
by zero-based integers. A linked list is built on top of this array, allowing a
list of active threads to be maintained without requiring contiguous storage.

Each node represents a thread, storing the deepest stack trace observed so far.
This structure allows a reader to reconstruct the state of all active threads
simply by traversing the linked list from the header.
