use std::cell::Cell;
use std::io;
use std::marker::PhantomData;
use std::mem::{offset_of, size_of};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::null;
use std::rc::Rc;
use std::slice;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::thread::{self, JoinHandle};

use windows_sys::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, ERROR_MORE_DATA, ERROR_SUCCESS, ERROR_WMI_INSTANCE_NOT_FOUND,
    WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Diagnostics::Etw::{
    CONTROLTRACE_HANDLE, CloseTrace, ControlTraceW, ENABLE_TRACE_PARAMETERS,
    ENABLE_TRACE_PARAMETERS_VERSION_2, EVENT_CONTROL_CODE_ENABLE_PROVIDER, EVENT_FILTER_DESCRIPTOR,
    EVENT_FILTER_TYPE_EVENT_ID, EVENT_FILTER_TYPE_PID, EVENT_HEADER_FLAG_32_BIT_HEADER,
    EVENT_HEADER_FLAG_64_BIT_HEADER, EVENT_RECORD, EVENT_TRACE_CONTROL_STOP,
    EVENT_TRACE_INDEPENDENT_SESSION_MODE, EVENT_TRACE_LOGFILEW, EVENT_TRACE_LOGFILEW_0,
    EVENT_TRACE_LOGFILEW_1, EVENT_TRACE_NO_PER_PROCESSOR_BUFFERING, EVENT_TRACE_PROPERTIES,
    EVENT_TRACE_REAL_TIME_MODE, EnableTraceEx2, OpenTraceW, PROCESS_TRACE_MODE_EVENT_RECORD,
    PROCESS_TRACE_MODE_RAW_TIMESTAMP, PROCESS_TRACE_MODE_REAL_TIME, PROCESSTRACE_HANDLE,
    ProcessTrace, StartTraceW, TRACE_LEVEL_VERBOSE, WNODE_FLAG_TRACED_GUID,
};
use windows_sys::Win32::System::Performance::QueryPerformanceFrequency;
use windows_sys::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};
use windows_sys::core::{GUID, w};

// Microsoft-Windows-DXGI and Microsoft-Windows-D3D9 manifest events, also used by PresentMon.
const DXGI: GUID = GUID::from_u128(0xca11c036_0102_4a2d_a6ad_f03cfed5d3c9);
const D3D9: GUID = GUID::from_u128(0x783aca0a_790e_4d7f_8451_aa850511c6b9);
const LOST_EVENT: GUID = GUID::from_u128(0x6a399ae0_4bc6_4de9_870b_3657f8947e7e);
const DXGI_START: u16 = 42;
const DXGI_STOP: u16 = 43;
const DXGI_MPO_START: u16 = 55;
const DXGI_MPO_STOP: u16 = 56;
const D3D9_START: u16 = 1;
const D3D9_STOP: u16 = 2;
const PRESENT_KEYWORD: u64 = 0x8000_0000_0000_0002;
const SESSION_NAME: &str = "ARGOS-Present-1cc63272-76b5-4aaf-98ad-c9171c1c3347";
const MUTEX_NAME: *const u16 = w!("Global\\ARGOS-Present-1cc63272-76b5-4aaf-98ad-c9171c1c3347");
const SESSION_NAME_CAPACITY: usize = 64;
const EVENT_QUEUE_CAPACITY: usize = 4096;

thread_local! {
    static SESSION_LOCK_HELD: Cell<bool> = const { Cell::new(false) };
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum Runtime {
    Dxgi,
    D3d9,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PresentKind {
    Start { swap_chain: u64, flags: u32 },
    Stop { result: i32 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PresentEvent {
    pub(super) thread_id: u32,
    pub(super) tick: u64,
    pub(super) runtime: Runtime,
    pub(super) kind: PresentKind,
}

pub(super) struct Capture {
    events: Receiver<PresentEvent>,
    loss: Arc<AtomicBool>,
    ticks_per_second: u64,
    trace: PROCESSTRACE_HANDLE,
    session: Option<Session>,
    worker: Option<JoinHandle<()>>,
}

impl Capture {
    pub(super) fn start(pid: u32) -> io::Result<Self> {
        let mut frequency = 0;
        if unsafe { QueryPerformanceFrequency(&mut frequency) } == 0 || frequency <= 0 {
            return Err(io::Error::last_os_error());
        }

        let session = Session::start()?;
        session.enable(
            pid,
            &DXGI,
            [DXGI_START, DXGI_STOP, DXGI_MPO_START, DXGI_MPO_STOP],
        )?;
        session.enable(pid, &D3D9, [D3D9_START, D3D9_STOP])?;

        let (sender, events) = mpsc::sync_channel(EVENT_QUEUE_CAPACITY);
        let loss = Arc::new(AtomicBool::new(false));
        let context = Context {
            pid,
            events: sender,
            loss: Arc::clone(&loss),
        };
        let name = session.name;
        let (startup, ready) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("argos-present-etw".into())
            .spawn(move || consume(name, context, startup))?;
        let trace = match ready.recv() {
            Ok(Ok(trace)) => trace,
            result => {
                let _ = worker.join();
                return Err(result
                    .ok()
                    .and_then(Result::err)
                    .unwrap_or_else(|| io::Error::other("falha ao iniciar a leitura de quadros")));
            }
        };

        Ok(Self {
            events,
            loss,
            ticks_per_second: frequency as u64,
            trace,
            session: Some(session),
            worker: Some(worker),
        })
    }

    pub(super) fn try_recv(&self) -> Result<PresentEvent, TryRecvError> {
        self.events.try_recv()
    }

    pub(super) fn take_loss(&self) -> bool {
        self.loss.swap(false, Ordering::Relaxed)
    }

    pub(super) fn ticks_per_second(&self) -> u64 {
        self.ticks_per_second
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // A real-time consumer can be closed while ProcessTrace is running.
        unsafe { CloseTrace(self.trace) };
        drop(self.session.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[repr(C)]
struct SessionProperties {
    native: EVENT_TRACE_PROPERTIES,
    name: [u16; SESSION_NAME_CAPACITY],
}

impl SessionProperties {
    fn new() -> Self {
        let mut native = EVENT_TRACE_PROPERTIES::default();
        native.Wnode.BufferSize = size_of::<Self>() as u32;
        native.Wnode.Flags = WNODE_FLAG_TRACED_GUID;
        native.Wnode.ClientContext = 1; // Raw timestamps use the performance counter.
        native.BufferSize = 16; // KiB per buffer.
        native.MinimumBuffers = 2;
        native.MaximumBuffers = 16;
        // A shared buffer retains Present start/stop order when a thread changes CPUs.
        native.LogFileMode = EVENT_TRACE_REAL_TIME_MODE
            | EVENT_TRACE_NO_PER_PROCESSOR_BUFFERING
            | EVENT_TRACE_INDEPENDENT_SESSION_MODE;
        native.FlushTimer = 1;
        native.LoggerNameOffset = offset_of!(Self, name) as u32;
        Self {
            native,
            name: [0; SESSION_NAME_CAPACITY],
        }
    }
}

struct Session {
    handle: CONTROLTRACE_HANDLE,
    name: [u16; SESSION_NAME_CAPACITY],
    _lock: SessionLock,
}

impl Session {
    fn start() -> io::Result<Self> {
        let lock = SessionLock::acquire()?;
        let mut name = [0; SESSION_NAME_CAPACITY];
        for (destination, unit) in name.iter_mut().zip(SESSION_NAME.encode_utf16()) {
            *destination = unit;
        }
        let mut handle = CONTROLTRACE_HANDLE::default();
        let mut result = start_session(&mut handle, &name);
        if result == ERROR_ALREADY_EXISTS {
            // The mutex is exclusive, so the session with this name was left by an older ARGOS.
            let stopped = stop_session(CONTROLTRACE_HANDLE::default(), name.as_ptr());
            if !matches!(stopped, ERROR_WMI_INSTANCE_NOT_FOUND | ERROR_MORE_DATA) {
                status(stopped)?;
            }
            result = start_session(&mut handle, &name);
        }
        status(result)?;
        Ok(Self {
            handle,
            name,
            _lock: lock,
        })
    }

    fn enable<const N: usize>(
        &self,
        pid: u32,
        provider: &GUID,
        events: [u16; N],
    ) -> io::Result<()> {
        #[repr(C)]
        struct EventIds<const N: usize> {
            filter_in: u8,
            reserved: u8,
            count: u16,
            events: [u16; N],
        }
        let events = EventIds {
            filter_in: 1,
            reserved: 0,
            count: N as u16,
            events,
        };
        let mut filters = [
            EVENT_FILTER_DESCRIPTOR {
                Ptr: &raw const pid as u64,
                Size: size_of::<u32>() as u32,
                Type: EVENT_FILTER_TYPE_PID,
            },
            EVENT_FILTER_DESCRIPTOR {
                Ptr: &raw const events as u64,
                Size: size_of::<EventIds<N>>() as u32,
                Type: EVENT_FILTER_TYPE_EVENT_ID,
            },
        ];
        let parameters = ENABLE_TRACE_PARAMETERS {
            Version: ENABLE_TRACE_PARAMETERS_VERSION_2,
            EnableFilterDesc: filters.as_mut_ptr(),
            FilterDescCount: filters.len() as u32,
            ..Default::default()
        };
        status(unsafe {
            EnableTraceEx2(
                self.handle,
                provider,
                EVENT_CONTROL_CODE_ENABLE_PROVIDER,
                TRACE_LEVEL_VERBOSE as u8,
                PRESENT_KEYWORD,
                0,
                0,
                &parameters,
            )
        })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        stop_session(self.handle, null());
    }
}

fn start_session(handle: &mut CONTROLTRACE_HANDLE, name: &[u16; SESSION_NAME_CAPACITY]) -> u32 {
    let mut properties = SessionProperties::new();
    // The writable buffer extends past the native header to LoggerNameOffset.
    unsafe { StartTraceW(handle, name.as_ptr(), &raw mut properties.native) }
}

fn stop_session(handle: CONTROLTRACE_HANDLE, name: *const u16) -> u32 {
    let mut properties = SessionProperties::new();
    unsafe {
        ControlTraceW(
            handle,
            name,
            &raw mut properties.native,
            EVENT_TRACE_CONTROL_STOP,
        )
    }
}

struct SessionLock {
    handle: OwnedHandle,
    _owner: PhantomData<Rc<()>>,
}

impl SessionLock {
    fn acquire() -> io::Result<Self> {
        // Win32 mutexes allow their owning thread to acquire them again.
        if SESSION_LOCK_HELD.get() {
            return Err(io::Error::from_raw_os_error(ERROR_ALREADY_EXISTS as i32));
        }
        let handle = unsafe { CreateMutexW(null(), 0, MUTEX_NAME) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
        match unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) } {
            WAIT_OBJECT_0 | WAIT_ABANDONED => {
                SESSION_LOCK_HELD.set(true);
                Ok(Self {
                    handle,
                    _owner: PhantomData,
                })
            }
            WAIT_TIMEOUT => Err(io::Error::from_raw_os_error(ERROR_ALREADY_EXISTS as i32)),
            _ => Err(io::Error::last_os_error()),
        }
    }
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        // The thread that acquired the mutex must release it before the handle closes.
        unsafe { ReleaseMutex(self.handle.as_raw_handle()) };
        SESSION_LOCK_HELD.set(false);
    }
}

struct Context {
    pid: u32,
    events: SyncSender<PresentEvent>,
    loss: Arc<AtomicBool>,
}

fn consume(
    mut name: [u16; SESSION_NAME_CAPACITY],
    context: Context,
    startup: SyncSender<io::Result<PROCESSTRACE_HANDLE>>,
) {
    let mut context = Box::new(context);
    let mut logfile = EVENT_TRACE_LOGFILEW {
        LoggerName: name.as_mut_ptr(),
        Anonymous1: EVENT_TRACE_LOGFILEW_0 {
            ProcessTraceMode: PROCESS_TRACE_MODE_EVENT_RECORD
                | PROCESS_TRACE_MODE_RAW_TIMESTAMP
                | PROCESS_TRACE_MODE_REAL_TIME,
        },
        Anonymous2: EVENT_TRACE_LOGFILEW_1 {
            EventRecordCallback: Some(on_event),
        },
        Context: (&raw mut *context).cast(),
        ..Default::default()
    };
    let trace = unsafe { OpenTraceW(&mut logfile) };
    if trace.Value == usize::MAX as u64 {
        let _ = startup.send(Err(io::Error::last_os_error()));
        return;
    }
    if startup.send(Ok(trace)).is_err() {
        unsafe { CloseTrace(trace) };
        return;
    }

    // The caller owns the consumer handle. The callback context lives until processing returns.
    unsafe { ProcessTrace(&trace, 1, null(), null()) };
}

unsafe extern "system" fn on_event(record: *mut EVENT_RECORD) {
    // ETW passes a live record, with Context set by OpenTrace on this thread.
    let Some(record) = (unsafe { record.as_ref() }) else {
        return;
    };
    let Some(context) = (unsafe { record.UserContext.cast::<Context>().as_ref() }) else {
        return;
    };
    let header = &record.EventHeader;
    let descriptor = &header.EventDescriptor;
    if same_provider(&header.ProviderId, &LOST_EVENT) && matches!(descriptor.Opcode, 32 | 33) {
        context.loss.store(true, Ordering::Relaxed);
        return;
    }
    if header.ProcessId != context.pid {
        return;
    }

    let (runtime, start) = if same_provider(&header.ProviderId, &DXGI) {
        match descriptor.Id {
            DXGI_START | DXGI_MPO_START => (Runtime::Dxgi, true),
            DXGI_STOP | DXGI_MPO_STOP => (Runtime::Dxgi, false),
            _ => return,
        }
    } else if same_provider(&header.ProviderId, &D3D9) {
        match descriptor.Id {
            D3D9_START => (Runtime::D3d9, true),
            D3D9_STOP => (Runtime::D3d9, false),
            _ => return,
        }
    } else {
        return;
    };
    if descriptor.Version != 0 || header.TimeStamp < 0 || record.UserData.is_null() {
        context.loss.store(true, Ordering::Relaxed);
        return;
    }
    let data =
        unsafe { slice::from_raw_parts(record.UserData.cast(), record.UserDataLength.into()) };
    let Some(kind) = decode_payload(start, header.Flags.into(), data) else {
        context.loss.store(true, Ordering::Relaxed);
        return;
    };
    let event = PresentEvent {
        thread_id: header.ThreadId,
        tick: header.TimeStamp as u64,
        runtime,
        kind,
    };
    if matches!(context.events.try_send(event), Err(TrySendError::Full(_))) {
        context.loss.store(true, Ordering::Relaxed);
    }
}

fn decode_payload(start: bool, flags: u32, data: &[u8]) -> Option<PresentKind> {
    if !start {
        return Some(PresentKind::Stop {
            result: i32::from_le_bytes(data.first_chunk().copied()?),
        });
    }

    // All supported v0 starts begin with the swap chain pointer, then the runtime's flags.
    let pointer_flags = flags & (EVENT_HEADER_FLAG_32_BIT_HEADER | EVENT_HEADER_FLAG_64_BIT_HEADER);
    let (swap_chain, pointer_size) = match pointer_flags {
        EVENT_HEADER_FLAG_32_BIT_HEADER => {
            (u32::from_le_bytes(data.first_chunk().copied()?).into(), 4)
        }
        EVENT_HEADER_FLAG_64_BIT_HEADER => (u64::from_le_bytes(data.first_chunk().copied()?), 8),
        _ => return None,
    };
    Some(PresentKind::Start {
        swap_chain,
        flags: u32::from_le_bytes(data.get(pointer_size..)?.first_chunk().copied()?),
    })
}

fn same_provider(left: &GUID, right: &GUID) -> bool {
    left.data1 == right.data1
        && left.data2 == right.data2
        && left.data3 == right.data3
        && left.data4 == right.data4
}

fn status(code: u32) -> io::Result<()> {
    if code == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code as i32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_runtime_pointer_width_independently_of_argos() {
        let bytes = [0x78, 0x56, 0x34, 0x12, 0x08, 0, 0, 0, 0x01, 0, 0, 0];
        assert_eq!(
            decode_payload(true, EVENT_HEADER_FLAG_32_BIT_HEADER, &bytes),
            Some(PresentKind::Start {
                swap_chain: 0x1234_5678,
                flags: 8,
            })
        );
        assert_eq!(
            decode_payload(true, EVENT_HEADER_FLAG_64_BIT_HEADER, &bytes),
            Some(PresentKind::Start {
                swap_chain: 0x0000_0008_1234_5678,
                flags: 1,
            })
        );
    }

    #[test]
    fn rejects_truncation_and_reads_hresult_as_signed() {
        assert_eq!(
            decode_payload(true, EVENT_HEADER_FLAG_64_BIT_HEADER, &[0; 8]),
            None
        );
        assert_eq!(decode_payload(true, 0, &[0; 12]), None);
        assert_eq!(decode_payload(false, 0, &[0; 3]), None);
        assert_eq!(
            decode_payload(false, 0, &[5, 0, 7, 128]),
            Some(PresentKind::Stop {
                result: 0x8007_0005u32 as i32
            })
        );
    }
}
