use std::io;
use std::mem::size_of;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::{null, null_mut};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_IO_PENDING, ERROR_PIPE_BUSY,
    ERROR_PIPE_NOT_CONNECTED, ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_DOES_NOT_EXIST,
    ERROR_SUCCESS, HANDLE, INVALID_HANDLE_VALUE, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_READ_DATA, FILE_WRITE_DATA, OPEN_EXISTING, ReadFile,
    SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT, WriteFile,
};
use windows_sys::Win32::System::IO::{
    CancelIoEx, GetOverlappedResult, GetOverlappedResultEx, OVERLAPPED,
};
use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows_sys::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_SUBKEY_WOW6464KEY, RegGetValueW,
};
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

pub(crate) struct CpuSamples {
    pub(crate) temperature: io::Result<Option<f64>>,
    pub(crate) power: io::Result<Option<f64>>,
}

impl Default for CpuSamples {
    fn default() -> Self {
        Self {
            temperature: Ok(None),
            power: Ok(None),
        }
    }
}

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

    pub(crate) fn sample(&mut self, temperature: bool, power: bool) -> io::Result<CpuSamples> {
        match &mut self.0 {
            Source::Service(broker) => broker.sample(temperature, power).map_err(service_error),
            Source::Direct(sensor) => Ok(CpuSamples {
                temperature: if temperature {
                    sensor.celsius()
                } else {
                    Ok(None)
                },
                power: if power { sensor.watts() } else { Ok(None) },
            }),
        }
    }
}

fn service_error(error: io::Error) -> io::Error {
    let kind = match error.kind() {
        io::ErrorKind::PermissionDenied => io::ErrorKind::Unsupported,
        kind => kind,
    };
    io::Error::new(kind, format!("ARGOS Sensor de CPU · {error}"))
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
    supports_power: bool,
    legacy_retry: Option<(u32, Instant)>,
}

impl Broker {
    fn open() -> io::Result<Self> {
        Ok(Self {
            service: open_service(SERVICE_QUERY_STATUS)?,
            server_pid: None,
            next_check: Instant::now(),
            start_result: None,
            supports_power: metrics_revision() >= 2,
            legacy_retry: None,
        })
    }

    fn sample(&mut self, temperature: bool, power: bool) -> io::Result<CpuSamples> {
        if !temperature && !power {
            return Ok(CpuSamples::default());
        }
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
            if power {
                self.supports_power = metrics_revision() >= 2;
            }
            self.server_pid = None;
            if temperature || (power && self.supports_power) {
                self.server_pid = self.find_server()?;
            }
        }
        let power_unavailable = power
            && (!self.supports_power
                || self
                    .legacy_retry
                    .is_some_and(|(pid, retry)| self.server_pid == Some(pid) && now < retry));
        let mut samples = CpuSamples::default();
        if power_unavailable {
            samples.power = outdated_service();
        }
        let Some(pid) = self.server_pid else {
            return Ok(samples);
        };
        let request = match (temperature, power && !power_unavailable) {
            (true, true) => Request::Both,
            (true, false) => Request::Temperature,
            (false, true) => Request::Power,
            (false, false) => return Ok(samples),
        };
        let deadline = Instant::now() + PIPE_TIMEOUT;
        match with_legacy_fallback(request, |request| request_samples(pid, request, deadline)) {
            Ok((mut received, legacy)) => {
                if legacy {
                    self.legacy_retry = Some((pid, now + SERVICE_RETRY));
                }
                if power_unavailable {
                    received.power = samples.power;
                }
                Ok(received)
            }
            Err(error) if transient_pipe_error(&error) => Ok(samples),
            Err(error) => Err(error),
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

fn metrics_revision() -> u32 {
    let mut revision = 0;
    let mut size = size_of::<u32>() as u32;
    let result = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!(r"Software\ARGOS\CpuSensor"),
            w!("MetricsRevision"),
            RRF_RT_REG_DWORD | RRF_SUBKEY_WOW6464KEY,
            null_mut(),
            (&raw mut revision).cast(),
            &mut size,
        )
    };
    if result == ERROR_SUCCESS { revision } else { 0 }
}

fn outdated_service() -> io::Result<Option<f64>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Atualize o componente ARGOS Sensor de CPU para medir potência",
    ))
}

fn transient_pipe_error(error: &io::Error) -> bool {
    error.raw_os_error().is_some_and(|code| {
        matches!(
            code as u32,
            ERROR_FILE_NOT_FOUND | ERROR_PIPE_BUSY | ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED
        )
    })
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Request {
    Temperature,
    Power,
    Both,
}

impl Request {
    fn command(self) -> u8 {
        match self {
            Self::Temperature => b'T',
            Self::Power => b'P',
            Self::Both => b'B',
        }
    }

    fn decode(self, bytes: [u8; 8]) -> CpuSamples {
        let [a, b, c, d, e, f, g, h] = bytes;
        let first = i32::from_le_bytes([a, b, c, d]);
        match self {
            Self::Temperature => CpuSamples {
                temperature: decode_sample(first, 125_000, "Valor de temperatura inválido"),
                power: Ok(None),
            },
            Self::Power => CpuSamples {
                temperature: Ok(None),
                power: decode_sample(first, 2_000_000, "Valor de potência inválido"),
            },
            Self::Both => CpuSamples {
                temperature: decode_sample(first, 125_000, "Valor de temperatura inválido"),
                power: decode_sample(
                    i32::from_le_bytes([e, f, g, h]),
                    2_000_000,
                    "Valor de potência inválido",
                ),
            },
        }
    }
}

fn with_legacy_fallback(
    request: Request,
    mut read: impl FnMut(Request) -> io::Result<CpuSamples>,
) -> io::Result<(CpuSamples, bool)> {
    match read(request) {
        Err(error)
            if request != Request::Temperature
                && (error.kind() == io::ErrorKind::UnexpectedEof
                    || error.raw_os_error().is_some_and(|code| {
                        matches!(code as u32, ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED)
                    })) =>
        {
            // Um serviço antigo fecha a conexão ao receber P ou B.
            match read(Request::Temperature) {
                Ok(mut samples) => {
                    if request == Request::Power {
                        samples.temperature = Ok(None);
                    }
                    samples.power = outdated_service();
                    Ok((samples, true))
                }
                Err(error)
                    if transient_pipe_error(&error) || error.kind() == io::ErrorKind::TimedOut =>
                {
                    // Tente primeiro a temperatura na próxima amostra se o pipe ainda
                    // estiver desconectando ou se a tentativa esgotou o prazo.
                    Ok((CpuSamples::default(), true))
                }
                Err(error) => Err(error),
            }
        }
        result => result.map(|samples| (samples, false)),
    }
}

fn request_samples(
    expected_pid: u32,
    request: Request,
    deadline: Instant,
) -> io::Result<CpuSamples> {
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
    let command = request.command();
    let written = transfer(&pipe, &event, deadline, |handle, io| unsafe {
        WriteFile(handle, &command, 1, null_mut(), io)
    })?;
    if written != 1 {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "Pedido de leitura incompleto",
        ));
    }
    let mut response = [0; 8];
    let expected = if request == Request::Both { 8 } else { 4 };
    let mut received = 0;
    while received < expected {
        let remaining = &mut response[received..expected];
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
                "Leitura do sensor incompleta",
            ));
        }
        received += bytes;
    }
    Ok(request.decode(response))
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

fn decode_sample(value: i32, maximum: i32, invalid: &str) -> io::Result<Option<f64>> {
    match value {
        SAMPLE_PENDING => Ok(None),
        SAMPLE_UNAVAILABLE => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Leitura indisponível no PawnIO",
        )),
        value if (0..=maximum).contains(&value) => Ok(Some(f64::from(value) / 1000.0)),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            invalid.to_owned(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(first: i32, second: i32) -> [u8; 8] {
        let mut bytes = [0; 8];
        bytes[..4].copy_from_slice(&first.to_le_bytes());
        bytes[4..].copy_from_slice(&second.to_le_bytes());
        bytes
    }

    #[test]
    fn decodes_millidegrees_and_preserves_service_sample_states() {
        assert_eq!(
            Request::Temperature
                .decode(response(56_875, 0))
                .temperature
                .unwrap(),
            Some(56.875)
        );
        assert_eq!(
            Request::Temperature
                .decode(response(SAMPLE_PENDING, 0))
                .temperature
                .unwrap(),
            None
        );
        assert_eq!(
            Request::Temperature
                .decode(response(SAMPLE_UNAVAILABLE, 0))
                .temperature
                .unwrap_err()
                .kind(),
            io::ErrorKind::Unsupported
        );
        assert_eq!(
            Request::Temperature
                .decode(response(250_000, 0))
                .temperature
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn decodes_milliwatts_with_independent_metric_status() {
        let samples = Request::Both.decode(response(SAMPLE_UNAVAILABLE, 64_125));
        assert_eq!(
            samples.temperature.unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        assert_eq!(samples.power.unwrap(), Some(64.125));
        let samples = Request::Both.decode(response(53_000, 2_000_001));
        assert_eq!(samples.temperature.unwrap(), Some(53.0));
        assert_eq!(
            samples.power.unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            Request::Power.decode(response(0, 0)).power.unwrap(),
            Some(0.0)
        );
        assert_eq!(
            Request::Power.decode(response(2_000_000, 0)).power.unwrap(),
            Some(2_000.0)
        );
        assert_eq!(
            Request::Power
                .decode(response(SAMPLE_PENDING, 0))
                .power
                .unwrap(),
            None
        );
        assert_eq!(
            Request::Power
                .decode(response(-1, 0))
                .power
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn retries_temperature_when_an_old_service_rejects_a_combined_request() {
        let mut commands = Vec::new();
        let (samples, legacy) = with_legacy_fallback(Request::Both, |request| {
            commands.push(request.command());
            match request {
                Request::Temperature => Ok(request.decode(response(56_875, 0))),
                _ => Err(io::Error::from_raw_os_error(ERROR_BROKEN_PIPE as i32)),
            }
        })
        .unwrap();
        assert_eq!(commands, [b'B', b'T']);
        assert!(legacy);
        assert_eq!(samples.temperature.unwrap(), Some(56.875));
        assert_eq!(
            samples.power.unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
    }

    #[test]
    fn does_not_probe_temperature_after_a_busy_combined_request() {
        let mut commands = Vec::new();
        let result = with_legacy_fallback(Request::Both, |request| {
            commands.push(request.command());
            Err(io::Error::from_raw_os_error(ERROR_PIPE_BUSY as i32))
        });
        assert_eq!(commands, [b'B']);
        assert!(result.is_err_and(|error| transient_pipe_error(&error)));
    }
}
