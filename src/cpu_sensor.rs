use std::io;
use std::mem::size_of;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::{null, null_mut};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_IO_PENDING, ERROR_PIPE_BUSY,
    ERROR_PIPE_NOT_CONNECTED, ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_DOES_NOT_EXIST, HANDLE,
    INVALID_HANDLE_VALUE, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_READ_DATA, FILE_WRITE_DATA, OPEN_EXISTING, ReadFile,
    SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT, WriteFile,
};
use windows_sys::Win32::System::IO::{
    CancelIoEx, GetOverlappedResult, GetOverlappedResultEx, OVERLAPPED,
};
use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows_sys::Win32::System::Services::{
    CloseServiceHandle, OpenSCManagerW, OpenServiceW, QueryServiceStatusEx, SC_HANDLE,
    SC_MANAGER_CONNECT, SC_STATUS_PROCESS_INFO, SERVICE_QUERY_STATUS, SERVICE_RUNNING,
    SERVICE_START, SERVICE_STATUS_PROCESS, SERVICE_STOPPED, StartServiceW,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, ResetEvent, SYNCHRONIZATION_SYNCHRONIZE,
};
use windows_sys::core::w;

use crate::cpu_temperature::CpuTemperature;

const SERVICE_NAME: *const u16 = w!("ArgosCpuSensor");
const PIPE_NAME: *const u16 = w!(r"\\.\pipe\ARGOS.CpuTemperature.v1");
const SERVICE_RETRY: Duration = Duration::from_secs(1);
const PIPE_TIMEOUT: Duration = Duration::from_millis(20);
const SAMPLE_UNAVAILABLE: i32 = i32::MIN;
const SAMPLE_PENDING: i32 = i32::MIN + 1;

pub(crate) struct CpuSensor(Source);

enum Source {
    Service(Broker),
    Direct(CpuTemperature),
}

impl CpuSensor {
    pub(crate) fn open() -> io::Result<Self> {
        match Broker::open() {
            Ok(broker) => Ok(Self(Source::Service(broker))),
            Err(error) if error.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST as i32) => {
                CpuTemperature::open().map(|sensor| Self(Source::Direct(sensor)))
            }
            Err(error) => Err(service_error(error)),
        }
    }

    pub(crate) fn celsius(&mut self) -> io::Result<Option<f64>> {
        match &mut self.0 {
            Source::Service(broker) => broker.celsius().map_err(service_error),
            Source::Direct(sensor) => sensor.celsius(),
        }
    }
}

fn service_error(error: io::Error) -> io::Error {
    let kind = match error.kind() {
        io::ErrorKind::PermissionDenied => io::ErrorKind::Unsupported,
        kind => kind,
    };
    io::Error::new(kind, format!("Serviço de temperatura do ARGOS · {error}"))
}

struct ServiceHandle(SC_HANDLE);

impl ServiceHandle {
    fn check(handle: SC_HANDLE) -> io::Result<Self> {
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(handle))
    }
}

impl Drop for ServiceHandle {
    fn drop(&mut self) {
        unsafe { CloseServiceHandle(self.0) };
    }
}

struct Broker {
    service: ServiceHandle,
    server_pid: Option<u32>,
    next_check: Instant,
    start_result: Option<Receiver<io::Result<()>>>,
}

impl Broker {
    fn open() -> io::Result<Self> {
        Ok(Self {
            service: open_service(SERVICE_QUERY_STATUS)?,
            server_pid: None,
            next_check: Instant::now(),
            start_result: None,
        })
    }

    fn celsius(&mut self) -> io::Result<Option<f64>> {
        if let Some(result) = &self.start_result {
            match result.try_recv() {
                Ok(result) => {
                    self.start_result = None;
                    result?;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.start_result = None;
                    return Err(io::Error::other(
                        "A inicialização do sensor foi interrompida",
                    ));
                }
            }
        }
        let now = Instant::now();
        if now >= self.next_check {
            self.next_check = now + SERVICE_RETRY;
            self.server_pid = None;
            self.server_pid = self.find_server()?;
        }
        let Some(pid) = self.server_pid else {
            return Ok(None);
        };
        match request_sample(pid) {
            Err(error)
                if error.raw_os_error().is_some_and(|code| {
                    matches!(
                        code as u32,
                        ERROR_FILE_NOT_FOUND
                            | ERROR_PIPE_BUSY
                            | ERROR_BROKEN_PIPE
                            | ERROR_PIPE_NOT_CONNECTED
                    )
                }) =>
            {
                Ok(None)
            }
            result => result,
        }
    }

    fn find_server(&mut self) -> io::Result<Option<u32>> {
        let mut status = SERVICE_STATUS_PROCESS::default();
        let mut needed = 0;
        // The buffer matches SC_STATUS_PROCESS_INFO and lives through the synchronous call.
        let found = unsafe {
            QueryServiceStatusEx(
                self.service.0,
                SC_STATUS_PROCESS_INFO,
                (&raw mut status).cast(),
                size_of::<SERVICE_STATUS_PROCESS>() as u32,
                &mut needed,
            )
        };
        if found == 0 {
            return Err(io::Error::last_os_error());
        }
        if status.dwCurrentState == SERVICE_RUNNING && status.dwProcessId != 0 {
            return Ok(Some(status.dwProcessId));
        }
        if status.dwCurrentState == SERVICE_STOPPED && self.start_result.is_none() {
            // The SCM can block a start while another service is handling a control request.
            let (sender, result) = mpsc::channel();
            thread::Builder::new()
                .name("argos-sensor-start".into())
                .spawn(move || {
                    let _ = sender.send(start_service());
                })?;
            self.start_result = Some(result);
        }
        Ok(None)
    }
}

fn open_service(access: u32) -> io::Result<ServiceHandle> {
    let manager =
        ServiceHandle::check(unsafe { OpenSCManagerW(null(), null(), SC_MANAGER_CONNECT) })?;
    ServiceHandle::check(unsafe { OpenServiceW(manager.0, SERVICE_NAME, access) })
}

fn start_service() -> io::Result<()> {
    let service = open_service(SERVICE_START)?;
    if unsafe { StartServiceW(service.0, 0, null()) } == 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_SERVICE_ALREADY_RUNNING as i32) {
            return Err(error);
        }
    }
    Ok(())
}

fn request_sample(expected_pid: u32) -> io::Result<Option<f64>> {
    // Identification-only QoS prevents even a spoofed server from impersonating the caller.
    let pipe = unsafe {
        CreateFileW(
            PIPE_NAME,
            FILE_READ_DATA | FILE_WRITE_DATA | SYNCHRONIZATION_SYNCHRONIZE,
            0,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
            null_mut(),
        )
    };
    if pipe == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let pipe = unsafe { OwnedHandle::from_raw_handle(pipe) };
    let mut pid = 0;
    if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut pid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if pid != expected_pid {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Origem da leitura inválida",
        ));
    }

    let event = unsafe { CreateEventW(null(), 1, 0, null()) };
    if event.is_null() {
        return Err(io::Error::last_os_error());
    }
    let event = unsafe { OwnedHandle::from_raw_handle(event) };
    let deadline = Instant::now() + PIPE_TIMEOUT;
    let request = b'T';
    let written = transfer(&pipe, &event, deadline, |handle, io| unsafe {
        WriteFile(handle, &request, 1, null_mut(), io)
    })?;
    if written != 1 {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "Pedido de temperatura incompleto",
        ));
    }
    let mut response = [0; 4];
    let mut received = 0;
    while received < response.len() {
        let remaining = &mut response[received..];
        let bytes = transfer(&pipe, &event, deadline, |handle, io| unsafe {
            ReadFile(
                handle,
                remaining.as_mut_ptr(),
                remaining.len() as u32,
                null_mut(),
                io,
            )
        })? as usize;
        if bytes == 0 || bytes > remaining.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Leitura de temperatura incompleta",
            ));
        }
        received += bytes;
    }
    decode_sample(response)
}

fn transfer(
    pipe: &OwnedHandle,
    event: &OwnedHandle,
    deadline: Instant,
    start: impl FnOnce(HANDLE, *mut OVERLAPPED) -> i32,
) -> io::Result<u32> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(pipe_timeout());
    }
    let handle = pipe.as_raw_handle();
    let mut overlapped = OVERLAPPED {
        hEvent: event.as_raw_handle(),
        ..Default::default()
    };
    if unsafe { ResetEvent(overlapped.hEvent) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let started = start(handle, &mut overlapped);
    if started == 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
            return Err(error);
        }
    }
    let timeout_ms = remaining.as_micros().div_ceil(1000) as u32;
    let mut bytes = 0;
    if unsafe { GetOverlappedResultEx(handle, &overlapped, &mut bytes, timeout_ms, 0) } != 0 {
        return Ok(bytes);
    }
    let error = io::Error::last_os_error();
    // Cancellation of this local pipe operation must finish before releasing its stack buffers.
    unsafe {
        CancelIoEx(handle, &overlapped);
        GetOverlappedResult(handle, &overlapped, &mut bytes, 1);
    }
    if error.raw_os_error() == Some(WAIT_TIMEOUT as i32) {
        return Err(pipe_timeout());
    }
    Err(error)
}

fn pipe_timeout() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "Sem resposta do sensor")
}

fn decode_sample(bytes: [u8; 4]) -> io::Result<Option<f64>> {
    match i32::from_le_bytes(bytes) {
        SAMPLE_PENDING => Ok(None),
        SAMPLE_UNAVAILABLE => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Leitura indisponível no PawnIO",
        )),
        millidegrees @ 0..=125_000 => Ok(Some(f64::from(millidegrees) / 1000.0)),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Valor de temperatura inválido",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_millidegrees_and_preserves_service_sample_states() {
        assert_eq!(
            decode_sample(56_875i32.to_le_bytes()).unwrap(),
            Some(56.875)
        );
        assert_eq!(decode_sample(SAMPLE_PENDING.to_le_bytes()).unwrap(), None);
        assert_eq!(
            decode_sample(SAMPLE_UNAVAILABLE.to_le_bytes())
                .unwrap_err()
                .kind(),
            io::ErrorKind::Unsupported
        );
        assert_eq!(
            decode_sample(250_000i32.to_le_bytes()).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
