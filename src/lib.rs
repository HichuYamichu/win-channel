use std::collections::VecDeque;
use std::ffi::CStr;
use std::mem::MaybeUninit;

use windows::core::Result as WinResult;
use windows::core::PCSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Security::*;
use windows::Win32::System::Memory::*;
use windows::Win32::System::SystemServices::SECURITY_DESCRIPTOR_REVISION;
use windows::Win32::System::Threading::*;
use windows::Win32::System::WindowsProgramming::*;

pub struct Config<'a> {
    pub shmem_name: &'a CStr,
    pub mutex_name: &'a CStr,
    pub event_name: &'a CStr,
}

pub struct WinSender<const S: usize, T> {
    mem: *mut ArrayQueue<S, T>,
    mutex: HANDLE,
    send_event: HANDLE,
}

// SAFETY:
// Access to raw pointers is synchronized with windows mutex and windows events.
// No references are given out from this type.
unsafe impl<const S: usize, T: Sync> Sync for WinSender<S, T> {}
unsafe impl<const S: usize, T: Send> Send for WinSender<S, T> {}

impl<const S: usize, T> WinSender<S, T> {
    pub unsafe fn new(config: Config<'_>) -> WinResult<Self> {
        let file = OpenFileMappingA(
            FILE_MAP_READ.0 | FILE_MAP_WRITE.0,
            FALSE,
            PCSTR(config.shmem_name.as_ptr() as _),
        )?;

        let queue_size = std::mem::size_of::<ArrayQueue<S, T>>();
        let mem = MapViewOfFile(file, FILE_MAP_ALL_ACCESS, 0, 0, queue_size).Value as *mut _
            as *mut ArrayQueue<S, T>;

        const SYNCHRONIZE: SYNCHRONIZATION_ACCESS_RIGHTS =
            SYNCHRONIZATION_ACCESS_RIGHTS(0x00100000);

        let mutex = OpenMutexA(SYNCHRONIZE.0, FALSE, PCSTR(config.mutex_name.as_ptr() as _));
        let send_event = OpenEventA(
            SYNCHRONIZE | EVENT_MODIFY_STATE,
            FALSE,
            PCSTR(config.event_name.as_ptr() as _),
        )?;

        Ok(Self {
            mem,
            mutex,
            send_event,
        })
    }

    pub unsafe fn send(&self, item: T) -> WinResult<()> {
        WaitForSingleObject(self.mutex, INFINITE);

        // SAFETY: We are the only thread with access to this memory at this point.
        let queue = &mut *self.mem;
        // Ignore failed push.
        let _ = queue.push_back(item);

        ReleaseMutex(self.mutex)?;
        SetEvent(self.send_event)?;
        Ok(())
    }
}

pub struct WinReceiver<const S: usize, T> {
    mem: *mut ArrayQueue<S, T>,
    mutex: HANDLE,
    send_event: HANDLE,

    local_queue: VecDeque<T>,
}

impl<const S: usize, T> WinReceiver<S, T> {
    pub unsafe fn new(config: Config<'_>) -> WinResult<Self> {
        let mut sd: SECURITY_DESCRIPTOR = std::mem::zeroed();
        let psd = PSECURITY_DESCRIPTOR(&mut sd as *mut _ as *mut _);

        InitializeSecurityDescriptor(psd, SECURITY_DESCRIPTOR_REVISION)?;
        SetSecurityDescriptorDacl(psd, TRUE, None, FALSE)?;

        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as _,
            lpSecurityDescriptor: psd.0,
            bInheritHandle: FALSE,
        };

        let queue_size = std::mem::size_of::<ArrayQueue<S, T>>();
        let file = CreateFileMappingA(
            INVALID_HANDLE_VALUE,
            Some(&sa as *const _),
            PAGE_READWRITE,
            0,
            queue_size as _,
            PCSTR(config.shmem_name.as_ptr() as _),
        )?;

        let mem = MapViewOfFile(file, FILE_MAP_ALL_ACCESS, 0, 0, queue_size).Value
            as *mut ArrayQueue<S, T>;
        let queue = ArrayQueue::<S, T>::new();
        std::ptr::write(mem, queue);

        let mutex = CreateMutexA(
            Some(&sa as *const _),
            FALSE,
            PCSTR(config.mutex_name.as_ptr() as _),
        )?;

        let send_event = CreateEventA(
            Some(&sa as *const _),
            FALSE,
            FALSE,
            PCSTR(config.event_name.as_ptr() as _),
        )?;

        Ok(Self {
            mem,
            send_event,
            mutex,
            local_queue: VecDeque::new(),
        })
    }

    pub unsafe fn recv(&mut self) -> WinResult<T> {
        match self.local_queue.pop_front() {
            Some(item) => return Ok(item),
            None => {}
        }

        loop {
            WaitForSingleObject(self.send_event, INFINITE);
            WaitForSingleObject(self.mutex, INFINITE);

            // SAFETY: We are the only thread with access to this memory at this point.
            let queue = &mut *self.mem;

            while let Some(item) = queue.pop_front() {
                self.local_queue.push_back(item);
            }

            ReleaseMutex(self.mutex)?;
            ResetEvent(self.send_event)?;

            match self.local_queue.pop_front() {
                Some(message) => return Ok(message),
                None => {} // Race happend and other Receiver stolen all messages from shared queue.
            }
        }
    }
}

#[derive(Debug)]
struct ArrayQueue<const S: usize, T> {
    data: [MaybeUninit<T>; S],
    start: usize,
    end: usize,
    count: usize,
}

impl<const S: usize, T> ArrayQueue<S, T> {
    fn new() -> Self {
        Self {
            data: unsafe { MaybeUninit::uninit().assume_init() },
            start: 0,
            end: 0,
            count: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.count == 0
    }

    fn is_full(&self) -> bool {
        self.count == S
    }

    fn push_back(&mut self, item: T) -> Result<(), T> {
        if self.is_full() {
            return Err(item);
        }

        // SAFETY: `end` is in range so our pointer write is valid.
        unsafe {
            self.data[self.end].as_mut_ptr().write(item);
        }

        self.end = (self.end + 1) % S;
        self.count += 1;
        Ok(())
    }

    fn pop_front(&mut self) -> Option<T> {
        if self.is_empty() {
            return None;
        }

        // SAFETY: There was write already so this memory is initialized.
        let item = unsafe { self.data[self.start].as_ptr().read() };
        self.start = (self.start + 1) % S;
        self.count -= 1;
        Some(item)
    }
}
