//! A cross process Windows channel.

use std::{
    array,
    cell::UnsafeCell,
    fmt,
    mem::MaybeUninit,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use windows::{
    core::{Error, Result as WinResult, PCWSTR},
    Win32::Foundation::*,
    Win32::System::{Memory::*, Threading::*},
};

// NOTE: SYNCHRONIZE constant is missing from windows-rs crate.
const SYNCHRONIZE: SYNCHRONIZATION_ACCESS_RIGHTS = SYNCHRONIZATION_ACCESS_RIGHTS(0x00100000);

pub enum TrySendError<T> {
    Full(T),
    Disconnected(T),
    Other(Error),
}

impl<T> fmt::Debug for TrySendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TrySendError").finish_non_exhaustive()
    }
}

pub enum SendError<T> {
    Disconnected(T),
    Other(Error),
}

impl<T> fmt::Debug for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SendError").finish_non_exhaustive()
    }
}

pub enum RecvError {
    Disconnected,
    Other(Error),
}

impl fmt::Debug for RecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecvError").finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct Queue<T, const SIZE: usize> {
    head: AtomicUsize,
    tail: AtomicUsize,
    slots: [Slot<T>; SIZE],
}

unsafe impl<T, const SIZE: usize> Send for Queue<T, { SIZE }> {}
unsafe impl<T, const SIZE: usize> Sync for Queue<T, { SIZE }> {}

#[derive(Debug)]
struct Slot<T> {
    value: UnsafeCell<MaybeUninit<T>>,
    ready: AtomicBool,
}

impl<T, const SIZE: usize> Queue<T, { SIZE }> {
    fn new() -> Self {
        Self {
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            slots: array::from_fn(|_| Slot {
                value: UnsafeCell::new(MaybeUninit::uninit()),
                ready: AtomicBool::new(false),
            }),
        }
    }

    fn push(&self, value: T) -> Result<(), T> {
        loop {
            let head = self.head.load(Ordering::Acquire);
            let tail = self.tail.load(Ordering::Acquire);

            if head.wrapping_sub(tail) >= SIZE {
                // Queue is full.
                return Err(value);
            }

            let new_head = head.wrapping_add(1);
            if self
                .head
                .compare_exchange(head, new_head, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                let idx = head & (SIZE - 1); // Fast modulo for multiples of 2.
                let slot = &self.slots[idx];

                unsafe {
                    (*slot.value.get()).write(value);
                }

                slot.ready.store(true, Ordering::Release);
                return Ok(());
            }

            std::hint::spin_loop();
        }
    }

    // There has to be at most one Receiver.
    unsafe fn pop(&self) -> Option<T> {
        let tail = self.tail.load(Ordering::Acquire);
        let head = self.head.load(Ordering::Acquire);
        if tail == head {
            // Queue is empty.
            return None;
        }

        let slot = &self.slots[tail];
        if !slot.ready.load(Ordering::Acquire) {
            return None;
        }

        let value = unsafe { (*slot.value.get()).assume_init_read() };
        slot.ready.store(false, Ordering::Release);
        self.tail.fetch_add(1, Ordering::Release);
        return Some(value);
    }
}

fn create_or_map_shmem_queue<T, const SIZE: usize>(
    file_mapping_name: PCWSTR,
) -> Result<(MEMORY_MAPPED_VIEW_ADDRESS, HANDLE, bool), Error> {
    let size = size_of::<Queue<T, { SIZE }>>();

    let mapping = unsafe {
        CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            None,
            PAGE_READWRITE,
            0,
            size as u32,
            file_mapping_name,
        )
    };

    let Ok(mapping) = mapping else {
        return Err(Error::from_win32());
    };

    if mapping.0 == std::ptr::null_mut() {
        return Err(Error::from_win32());
    }

    let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    let ptr = unsafe { MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, size) };
    if ptr.Value.is_null() {
        return Err(Error::from_win32());
    }

    return Ok((ptr, mapping, existed));
}

unsafe fn create_event(event_name: PCWSTR) -> WinResult<HANDLE> {
    CreateEventW(None, false, false, event_name)
}

unsafe fn open_event(event_name: PCWSTR) -> WinResult<HANDLE> {
    OpenEventW(SYNCHRONIZE | EVENT_MODIFY_STATE, false, event_name)
}

/// Configuration object used to create `Sender` or `Receiver`.
#[derive(Clone)]
pub struct Config {
    /// Name of shared memory to be used.
    pub shmem_name: PCWSTR,
    /// Name of Windows Event used to signal `send` operations.
    pub send_event_name: PCWSTR,
    /// Name of Windows Event used to signal `recv` operations.
    pub recv_event_name: PCWSTR,
    /// Name of Windows Event used to signal `shutdown` operations.
    pub disconnect_event_name: PCWSTR,
}

/// The sending side of a channel.
pub struct Sender<T, const SIZE: usize> {
    shared_queue: *mut Queue<T, { SIZE }>,
    mem_mapping: HANDLE,
    item_added_event: HANDLE,
    item_removed_event: HANDLE,
    shutdown_event: HANDLE,
}

unsafe impl<T, const SIZE: usize> Send for Sender<T, { SIZE }> {}
unsafe impl<T, const SIZE: usize> Sync for Sender<T, { SIZE }> {}

impl<T, const SIZE: usize> Sender<T, { SIZE }> {
    /// # SAFETY
    ///
    /// All `Sender` and `Receiver` structures must have the same configurations when connecting to the
    /// same shared memory (this includes `SIZE` constant).
    ///
    /// `SIZE` must be a multiple of 2.
    ///
    /// If shutdown was not complete (one of the processes hung up) and you reuse the same
    /// configuration you might connect to leftover state. If you can't guarantee that every conected
    /// instance was dropped either don't run `shutdown` or don't reuse the same config.
    pub unsafe fn new(config: Config) -> Result<Self, Error> {
        let (shmem, mapping, existed) =
            create_or_map_shmem_queue::<T, { SIZE }>(config.shmem_name)?;
        let queue_ptr = shmem.Value as *mut Queue<T, { SIZE }>;

        let s = match existed {
            false => {
                std::ptr::write(queue_ptr, Queue::new());
                Self {
                    shared_queue: queue_ptr,
                    mem_mapping: mapping,
                    item_added_event: create_event(config.send_event_name)?,
                    item_removed_event: create_event(config.recv_event_name)?,
                    shutdown_event: create_event(config.disconnect_event_name)?,
                }
            }
            true => Self {
                shared_queue: queue_ptr,
                mem_mapping: mapping,
                item_added_event: open_event(config.send_event_name)?,
                item_removed_event: open_event(config.recv_event_name)?,
                shutdown_event: open_event(config.disconnect_event_name)?,
            },
        };

        Ok(s)
    }

    /// # SAFETY
    /// You cannot send types that are dependant on current address space (types with pointers, references, etc)
    pub unsafe fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
        let is_shutting_down = WaitForSingleObject(self.shutdown_event, 0) == WAIT_OBJECT_0;
        if is_shutting_down {
            return Err(TrySendError::Disconnected(value));
        }
        let res = unsafe { (*self.shared_queue).push(value) };
        return res.map_err(|e| TrySendError::Full(e));
    }

    /// # SAFETY
    /// You cannot send types that are dependant on current address space (types with pointers, references, etc)
    pub unsafe fn send(&self, mut value: T) -> Result<(), SendError<T>> {
        while let Err(v) = (*self.shared_queue).push(value) {
            value = v;
            let wait_event = WaitForMultipleObjects(
                &[self.shutdown_event, self.item_removed_event],
                false,
                INFINITE,
            );

            if wait_event == WAIT_FAILED {
                return Err(SendError::Other(Error::from_win32()));
            } else if wait_event.0 - WAIT_OBJECT_0.0 == 0 {
                // 0 is the index of shutdown_event
                return Err(SendError::Disconnected(value));
            } else {
            } // WAIT_TIMEOUT and WAIT_ABANDONED are not aplicable so this is the good case.
        }
        SetEvent(self.item_added_event).map_err(|e| SendError::Other(e))?;
        return Ok(());
    }

    pub fn shutdown(&self) -> Result<(), Error> {
        unsafe { SetEvent(self.shutdown_event) }
    }
}

impl<T, const SIZE: usize> Drop for Sender<T, { SIZE }> {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.item_added_event);
            let _ = CloseHandle(self.item_removed_event);
            let _ = CloseHandle(self.shutdown_event);
            let map = MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.shared_queue as *mut _,
            };
            let _ = UnmapViewOfFile(map);
            let _ = CloseHandle(self.mem_mapping);
        }
    }
}

/// The receiving side of a channel.
pub struct Receiver<T, const SIZE: usize> {
    shared_queue: *mut Queue<T, { SIZE }>,
    mem_mapping: HANDLE,
    item_added_event: HANDLE,
    item_removed_event: HANDLE,
    shutdown_event: HANDLE,
}

unsafe impl<T, const SIZE: usize> Send for Receiver<T, { SIZE }> {}
unsafe impl<T, const SIZE: usize> Sync for Receiver<T, { SIZE }> {}

impl<T, const SIZE: usize> Receiver<T, { SIZE }> {
    /// # SAFETY
    ///
    /// All `Sender` and `Receiver` structures must have the same configurations when connecting to the
    /// same shared memory (this includes `SIZE` constant).
    ///
    /// `SIZE` must be a multiple of 2.
    ///
    /// If shutdown was not complete (one of the processes hung up) and you reuse the same
    /// configuration you might connect to leftover state. If you can't guarantee that every conected
    /// instance was dropped either don't run `shutdown` or don't reuse the same config.
    ///
    /// There can be at most one Receiver at a time.
    pub unsafe fn new(config: Config) -> Result<Self, Error> {
        let (shmem, mapping, existed) =
            create_or_map_shmem_queue::<T, { SIZE }>(config.shmem_name)?;
        let queue_ptr = shmem.Value as *mut Queue<T, { SIZE }>;

        let s = match existed {
            false => {
                std::ptr::write(queue_ptr, Queue::new());
                Self {
                    shared_queue: queue_ptr,
                    mem_mapping: mapping,
                    item_added_event: create_event(config.send_event_name)?,
                    item_removed_event: create_event(config.recv_event_name)?,
                    shutdown_event: create_event(config.disconnect_event_name)?,
                }
            }
            true => Self {
                shared_queue: queue_ptr,
                mem_mapping: mapping,
                item_added_event: open_event(config.send_event_name)?,
                item_removed_event: open_event(config.recv_event_name)?,
                shutdown_event: open_event(config.disconnect_event_name)?,
            },
        };

        Ok(s)
    }

    /// SAFETY: You cannot receive types that are dependant on current address space (types with pointers, references, etc)
    pub unsafe fn try_recv(&self) -> Option<T> {
        unsafe { (*self.shared_queue).pop() }
    }

    /// SAFETY: You cannot receive types that are dependant on current address space (types with pointers, references, etc)
    pub unsafe fn recv(&self) -> Result<T, RecvError> {
        loop {
            unsafe {
                if let Some(item) = (*self.shared_queue).pop() {
                    SetEvent(self.item_removed_event).map_err(|e| RecvError::Other(e))?;
                    return Ok(item);
                }

                let wait_event = WaitForMultipleObjects(
                    &[self.shutdown_event, self.item_added_event],
                    false,
                    INFINITE,
                );

                if wait_event == WAIT_FAILED {
                    return Err(RecvError::Other(Error::from_win32()));
                } else if wait_event.0 - WAIT_OBJECT_0.0 == 0 {
                    // 0 is the index of shutdown_event
                    return Err(RecvError::Disconnected);
                } else {
                } // WAIT_TIMEOUT and WAIT_ABANDONED are not aplicable so this is the good case.
            }
        }
    }

    pub fn shutdown(&self) -> Result<(), Error> {
        unsafe { SetEvent(self.shutdown_event) }
    }
}

impl<T, const SIZE: usize> Drop for Receiver<T, { SIZE }> {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.item_added_event);
            let _ = CloseHandle(self.item_removed_event);
            let _ = CloseHandle(self.shutdown_event);
            let map = MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.shared_queue as *mut _,
            };
            let _ = UnmapViewOfFile(map);
            let _ = CloseHandle(self.mem_mapping);
        }
    }
}
