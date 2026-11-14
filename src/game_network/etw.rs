use std::io;
use std::mem::{offset_of, size_of};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::ptr::null;
use std::slice;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::thread::{self, JoinHandle};

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_MORE_DATA, ERROR_NO_SYSTEM_RESOURCES,
    ERROR_PRIVILEGE_NOT_HELD, ERROR_SUCCESS, ERROR_WMI_INSTANCE_NOT_FOUND, GetLastError,
    SetLastError,
};
use windows_sys::Win32::System::Diagnostics::Etw::{
    CONTROLTRACE_HANDLE, CloseTrace, ControlTraceW, EVENT_RECORD, EVENT_TRACE_CONTROL_STOP,
    EVENT_TRACE_FLAG_NETWORK_TCPIP, EVENT_TRACE_LOGFILEW, EVENT_TRACE_LOGFILEW_0,
    EVENT_TRACE_LOGFILEW_1, EVENT_TRACE_PROPERTIES, EVENT_TRACE_REAL_TIME_MODE,
    EVENT_TRACE_SYSTEM_LOGGER_MODE, OpenTraceW, PROCESS_TRACE_MODE_EVENT_RECORD,
    PROCESS_TRACE_MODE_REAL_TIME, PROCESSTRACE_HANDLE, ProcessTrace, StartTraceW,
    WNODE_FLAG_TRACED_GUID,
};
use windows_sys::Win32::System::Threading::CreateEventW;
use windows_sys::core::{GUID, w};

use super::{Packet, Transport};

const TCP_IP: GUID = GUID::from_u128(0x9a280ac0_c8e0_11d1_84e2_00c04fb998a2);
const UDP_IP: GUID = GUID::from_u128(0xbf3a50c5_a9c9_4988_a005_2df0b7c80f80);
const LOST_EVENT: GUID = GUID::from_u128(0x6a399ae0_4bc6_4de9_870b_3657f8947e7e);
const SESSION_NAME: &str = "ARGOS-Network-d4d4efeb-5a91-4eae-a78d-2ac4c6a96ad6";
const SESSION_MARKER: *const u16 = w!("Global\\ARGOS-Network-d4d4efeb-5a91-4eae-a78d-2ac4c6a96ad6");
const NAME_CAPACITY: usize = 64;

pub(super) struct Trace {
    trace: PROCESSTRACE_HANDLE,
    session: Option<Session>,
    worker: Option<JoinHandle<()>>,
    loss: Arc<AtomicBool>,
}

impl Trace {
    pub(super) fn start<F: FnMut(Packet) + Send + 'static>(callback: F) -> Result<Self, String> {
        Self::open(callback).map_err(|error| match error.raw_os_error().map(|code| code as u32) {
            Some(ERROR_ACCESS_DENIED | ERROR_PRIVILEGE_NOT_HELD) => {
                "A captura de rede requer permissão do Windows. Abra o ARGOS como administrador"
                    .into()
            }
            Some(ERROR_ALREADY_EXISTS) => {
                "Outra instância do ARGOS já está capturando a rede".into()
            }
            Some(ERROR_NO_SYSTEM_RESOURCES) => {
                "O Windows atingiu o limite de sessões de captura".into()
            }
            _ => format!("Não foi possível iniciar a captura de rede: {error}"),
        })
    }

    fn open<F: FnMut(Packet) + Send + 'static>(callback: F) -> io::Result<Self> {
        let session = Session::start()?;
        let loss = Arc::new(AtomicBool::new(false));
        let context = Context {
            callback,
            loss: Arc::clone(&loss),
        };
        let (startup, ready) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("argos-network-etw".into())
            .spawn(move || consume(context, startup))?;
        let trace = match ready.recv() {
            Ok(Ok(trace)) => trace,
            result => {
                let _ = worker.join();
                return Err(result
                    .ok()
                    .and_then(Result::err)
                    .unwrap_or_else(|| io::Error::other("falha ao iniciar a leitura da rede")));
            }
        };
        Ok(Self {
            trace,
            session: Some(session),
            worker: Some(worker),
            loss,
        })
    }

    pub(super) fn is_running(&self) -> bool {
        self.worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
    }

    pub(super) fn take_loss(&self) -> bool {
        self.loss.swap(false, Ordering::Relaxed)
    }
}

impl Drop for Trace {
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
    name: [u16; NAME_CAPACITY],
}

impl SessionProperties {
    fn new() -> Self {
        let mut native = EVENT_TRACE_PROPERTIES::default();
        native.Wnode.BufferSize = size_of::<Self>() as u32;
        native.Wnode.Flags = WNODE_FLAG_TRACED_GUID;
        native.Wnode.ClientContext = 1;
        native.Wnode.Guid = GUID::from_u128(0xd4d4efeb_5a91_4eae_a78d_2ac4c6a96ad6);
        native.BufferSize = 16; // KiB per buffer.
        native.LogFileMode = EVENT_TRACE_REAL_TIME_MODE | EVENT_TRACE_SYSTEM_LOGGER_MODE;
        native.EnableFlags = EVENT_TRACE_FLAG_NETWORK_TCPIP;
        native.FlushTimer = 1;
        native.LoggerNameOffset = offset_of!(Self, name) as u32;
        Self {
            native,
            name: [0; NAME_CAPACITY],
        }
    }
}

struct Session {
    handle: CONTROLTRACE_HANDLE,
    _marker: OwnedHandle,
}

impl Session {
    fn start() -> io::Result<Self> {
        let name = session_name();
        // The marker lives as long as the owner, including while stopping the session.
        let marker = acquire_marker()?;
        let mut handle = CONTROLTRACE_HANDLE::default();
        let mut result = start_session(&mut handle, &name);
        if result == ERROR_ALREADY_EXISTS {
            // With exclusive ownership of the marker, this named session was left by ARGOS.
            let stopped = stop_session(CONTROLTRACE_HANDLE::default(), name.as_ptr());
            if !matches!(stopped, ERROR_WMI_INSTANCE_NOT_FOUND | ERROR_MORE_DATA) {
                status(stopped)?;
            }
            result = start_session(&mut handle, &name);
        }
        status(result)?;
        Ok(Self {
            handle,
            _marker: marker,
        })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        stop_session(self.handle, null());
    }
}

fn session_name() -> [u16; NAME_CAPACITY] {
    let mut name = [0; NAME_CAPACITY];
    for (destination, unit) in name.iter_mut().zip(SESSION_NAME.encode_utf16()) {
        *destination = unit;
    }
    name
}

fn acquire_marker() -> io::Result<OwnedHandle> {
    // A named event is used for its atomic creation and kernel-managed lifetime.
    unsafe { SetLastError(ERROR_SUCCESS) };
    let handle = unsafe { CreateEventW(null(), 0, 0, SESSION_MARKER) };
    let error = unsafe { GetLastError() };
    if handle.is_null() {
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
    if error == ERROR_ALREADY_EXISTS {
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    Ok(handle)
}

fn start_session(handle: &mut CONTROLTRACE_HANDLE, name: &[u16; NAME_CAPACITY]) -> u32 {
    let mut properties = SessionProperties::new();
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

struct Context<F> {
    callback: F,
    loss: Arc<AtomicBool>,
}

fn consume<F: FnMut(Packet)>(
    mut context: Context<F>,
    ready: SyncSender<io::Result<PROCESSTRACE_HANDLE>>,
) {
    let mut name = session_name();
    let mut logfile = EVENT_TRACE_LOGFILEW {
        LoggerName: name.as_mut_ptr(),
        Anonymous1: EVENT_TRACE_LOGFILEW_0 {
            ProcessTraceMode: PROCESS_TRACE_MODE_EVENT_RECORD | PROCESS_TRACE_MODE_REAL_TIME,
        },
        Anonymous2: EVENT_TRACE_LOGFILEW_1 {
            EventRecordCallback: Some(on_event::<F>),
        },
        Context: (&raw mut context).cast(),
        ..Default::default()
    };
    let trace = unsafe { OpenTraceW(&mut logfile) };
    if trace.Value == usize::MAX as u64 {
        let _ = ready.send(Err(io::Error::last_os_error()));
        return;
    }
    if ready.send(Ok(trace)).is_err() {
        unsafe { CloseTrace(trace) };
        return;
    }
    // The callback context stays at a stable address until ProcessTrace returns.
    unsafe { ProcessTrace(&trace, 1, null(), null()) };
}

unsafe extern "system" fn on_event<F: FnMut(Packet)>(record: *mut EVENT_RECORD) {
    let Some(record) = (unsafe { record.as_ref() }) else {
        return;
    };
    // ProcessTrace invokes this on the consumer thread with the context set by OpenTrace.
    let Some(context) = (unsafe { record.UserContext.cast::<Context<F>>().as_mut() }) else {
        return;
    };
    let header = &record.EventHeader;
    let descriptor = &header.EventDescriptor;
    let transport = if same_provider(&header.ProviderId, &TCP_IP) {
        Transport::Tcp
    } else if same_provider(&header.ProviderId, &UDP_IP) {
        Transport::Udp
    } else {
        if same_provider(&header.ProviderId, &LOST_EVENT) && matches!(descriptor.Opcode, 32 | 33) {
            context.loss.store(true, Ordering::Relaxed);
        }
        return;
    };
    if descriptor.Version != 2 || record.UserData.is_null() {
        return;
    }
    let data =
        unsafe { slice::from_raw_parts(record.UserData.cast(), record.UserDataLength.into()) };
    if let Some(packet) = decode_packet(transport, descriptor.Opcode, data) {
        (context.callback)(packet);
    }
}

fn decode_packet(transport: Transport, opcode: u8, data: &[u8]) -> Option<Packet> {
    // The classic TCP/IP and UDP/IP MOF events have a common prefix for send and receive.
    let (ipv6, inbound) = match opcode {
        10 => (false, false),
        11 => (false, true),
        26 => (true, false),
        27 => (true, true),
        _ => return None,
    };
    let pid = u32::from_le_bytes(data.first_chunk().copied()?);
    let (destination, source, ports) = if ipv6 {
        (
            IpAddr::V6(Ipv6Addr::from(read(data, 8)?)),
            IpAddr::V6(Ipv6Addr::from(read(data, 24)?)),
            40,
        )
    } else {
        (
            IpAddr::V4(Ipv4Addr::from(read(data, 8)?)),
            IpAddr::V4(Ipv4Addr::from(read(data, 12)?)),
            16,
        )
    };
    let destination = SocketAddr::new(destination, u16::from_be_bytes(read(data, ports)?));
    let source = SocketAddr::new(source, u16::from_be_bytes(read(data, ports + 2)?));
    // TCP receive events keep the local socket in the source fields. UDP receives use
    // packet direction instead, so their destination is the local socket.
    let (local, remote) = if inbound && matches!(transport, Transport::Udp) {
        (destination, source)
    } else {
        (source, destination)
    };
    Some(Packet {
        pid,
        transport,
        local,
        remote,
        inbound,
    })
}

fn read<const N: usize>(data: &[u8], offset: usize) -> Option<[u8; N]> {
    data.get(offset..)?.first_chunk().copied()
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
    fn normalizes_tcp_and_udp_ipv4_directions() {
        let data = [
            0x34, 0x12, 0, 0, 32, 0, 0, 0, 192, 0, 2, 1, 10, 0, 0, 1, 0x69, 0x87, 0xc0, 0x01,
        ];
        let remote: SocketAddr = "192.0.2.1:27015".parse().unwrap();
        let local: SocketAddr = "10.0.0.1:49153".parse().unwrap();
        for (transport, opcode, expected_local, expected_remote) in [
            (Transport::Tcp, 10, local, remote),
            (Transport::Tcp, 11, local, remote),
            (Transport::Udp, 10, local, remote),
            (Transport::Udp, 11, remote, local),
        ] {
            let packet = decode_packet(transport, opcode, &data).unwrap();
            assert_eq!(packet.pid, 0x1234);
            assert_eq!(packet.local, expected_local);
            assert_eq!(packet.remote, expected_remote);
            assert_eq!(packet.inbound, opcode == 11);
        }
    }

    #[test]
    fn decodes_ipv6_and_rejects_incomplete_or_unrelated_events() {
        let remote: Ipv6Addr = "2001:db8::1".parse().unwrap();
        let local: Ipv6Addr = "fd00::2".parse().unwrap();
        let mut data = [0; 44];
        data[8..24].copy_from_slice(&local.octets());
        data[24..40].copy_from_slice(&remote.octets());
        data[40..42].copy_from_slice(&49153u16.to_be_bytes());
        data[42..44].copy_from_slice(&27015u16.to_be_bytes());
        let packet = decode_packet(Transport::Udp, 27, &data).unwrap();
        assert!(packet.inbound);
        assert_eq!(packet.local, SocketAddr::new(local.into(), 49153));
        assert_eq!(packet.remote, SocketAddr::new(remote.into(), 27015));
        assert!(decode_packet(Transport::Udp, 27, &data[..43]).is_none());
        assert!(decode_packet(Transport::Tcp, 28, &data).is_none());
    }
}
