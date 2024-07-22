# WinChannel

WinChannel is a Rust channel implementation backed by Windows shared memory and synchronization primitives. It's useful if you need to pass data and synchronize between multiple processes that you do not own (for example when foreign process loads your dll and you need to signal some events back to your main exe).

# Requirements and design considerations

Since creating global Windows events and shared memory requires special privileges, in order to use this library correctly process that creates shared structures needs to be run with administrator privileges.

If shared queue is full incoming push will fail and message will be lost (adjust the queue size and make sure a receiving end does not fall behind).

Neither Sender nor Receiver _directly_ allocate/manage any dynamic memory. 

This is not high level library, Windows types are used as both inputs and results of functions (You need to depend on windows or windows-core crate).

# Safety

There are numerous safety requirements that need to be upheld by the user. Most importantly:

- There must exist at most one Receiver.
- Receiver must be created before any Sender.
- Since Sender and Receiver halves are created independently caller must ensure their configs match exactly.
- Types that allocate internally CANNOT be passed by this channel (obviously as they are going to allocate inside address space of current process so receiving end will most likely access unmapped memory regions).
- Drop is not run for internal queue elements.

# TODO

- Timing independent Sender/Receiver (first created structure should be responsible for creating shared memory).
- More optimal implementation (currently it's a standard channel implementation with mutex protected queue and condvar (Windows Event Object)).

