# WinChannel

A channel-like API allowing you to send messages not only between different threads but also between processes. Given the nature of the problem, Senders and Receivers are created separately and have to have matching configurations; this cannot be enforced by the compiler, so the user has to make sure to uphold numerous safety requirements.

# Safety requirements

- This is MPSC channel; there can be at most one consumer/receiver at any given time.
- WinChannel is generic; thus, the user MUST make sure that every instance connected through the same shared memory block uses exactly the same generic parameters.
- Queue size MUST be a multiple of 2.
- You CANNOT send any type with internal pointers or references or anything dependent on your address space.
- Hung-up processes might prevent cleanup of kernel objects; thus, it is highly recommended to not reuse the same configurations for new instances after tearing down previous ones, or you might connect to leftover shmem.
- Once the shutdown message is received, you should drop your instance, or it will prevent a full cleanup.

# Features overview

- Independent creation—you do not need to synchronize creation of Senders/Receivers; the first one to be created will initialize required structures.
- Blocking and non-blocking API—non-blocking functions fail when there's not enough space inside the shared queue.
- Manual teardown—you can send a shutdown signal to every connected instance; this is not guaranteed to actually release any resources. A hung-up process might prevent complete cleanup; in any case, process termination releases all handles held by that process; therefore, all kernel objects can eventually be freed.

# Implementation

- WinChannel is a fairly standard Sync Queue. Both Senders and Receivers share a Ring Buffer with atomic head/tail indices. A CAS loop is used for element insertion, while an atomic flag is used for safe handoff (which is sufficient under the assumption that there's only one receiver).


