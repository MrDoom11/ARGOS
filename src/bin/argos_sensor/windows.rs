use std::ffi::{OsStr, OsString};
use std::io;
use std::mem::size_of;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    ERROR_CALL_NOT_IMPLEMENTED, ERROR_GEN_FAILURE, ERROR_IO_PENDING, ERROR_NO_DATA,
    ERROR_PIPE_CONNECTED, ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_EXISTS,
    ERROR_SERVICE_NOT_ACTIVE, ERROR_SUCCESS, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX, ReadFile,
    WRITE_DAC, WriteFile,
};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_REJECT_REMOTE_CLIENTS,
};
use windows_sys::Win32::System::Services::*;
use windows_sys::Win32::System::Threading::{
    CreateEventW, ResetEvent, SetEvent, WaitForMultipleObjects, WaitForSingleObject,
};
use windows_sys::Win32::UI::Shell::{FOLDERID_ProgramFilesX64, SHGetKnownFolderPath};
use windows_sys::core::w;

use crate::cpu_temperature::{self, CpuTemperature};

const SERVICE: *const u16 = w!("ArgosCpuSensor");
const PIPE: *const u16 = w!(r"\\.\pipe\ARGOS.CpuTemperature.v1");
const SERVICE_DACL: *const u16 = w!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;LCRP;;;IU)");
// CreateFile also requests READ_ATTRIBUTES. Keep clients unable to create pipe instances.
const PIPE_DACL: *const u16 = w!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x100083;;;IU)");
const IDLE_MS: u32 = 8_000;
const MESSAGE_MS: u32 = 200;
const SAMPLE_INTERVAL: Duration = Duration::from_millis(100);
const REOPEN_INTERVAL: Duration = Duration::from_secs(2);
const UNAVAILABLE: i32 = i32::MIN;
const PENDING: i32 = i32::MIN + 1;

static STOP: OnceLock<OwnedHandle> = OnceLock::new();

pub(super) fn run() -> io::Result<()> {
    let mut arguments = std::env::args_os().skip(1);
    let command = arguments.next();
    if arguments.next().is_some() {
        return Err(io::Error::other("Argumentos inválidos"));
    }
    match command.as_deref().and_then(OsStr::to_str) {
        None if command.is_none() => dispatch(),
        Some("--install") => install(),
        Some("--uninstall") => uninstall(),
        Some("--version") => {
            println!("{}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        _ => Err(io::Error::other("Comando inválido")),
    }
}

fn dispatch() -> io::Result<()> {
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: SERVICE.cast_mut(),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW::default(),
    ];
    check(unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) })
}

unsafe extern "system" fn service_main(_count: u32, _arguments: *mut *mut u16) {
    let status = unsafe { RegisterServiceCtrlHandlerExW(SERVICE, Some(control), null()) };
    if status.is_null() {
        return;
    }
    report(status, SERVICE_START_PENDING, ERROR_SUCCESS);
    let result = (|| {
        STOP.set(event()?)
            .map_err(|_| io::Error::other("Serviço já inicializado"))?;
        let stop = STOP.get().expect("evento de parada inicializado");
        let pipe = Pipe::new(PIPE)?;
        report(status, SERVICE_RUNNING, ERROR_SUCCESS);
        let mut cache = SampleCache::default();
        serve(&pipe, stop.as_raw_handle(), || cache.sample())
    })();
    let code = result.map_or_else(
        |error| {
            error
                .raw_os_error()
                .map_or(ERROR_GEN_FAILURE, |value| value as u32)
        },
        |()| ERROR_SUCCESS,
    );
    report(status, SERVICE_STOPPED, code);
}

fn report(handle: SERVICE_STATUS_HANDLE, state: SERVICE_STATUS_CURRENT_STATE, error: u32) {
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: if state == SERVICE_RUNNING {
            SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN
        } else {
            0
        },
        dwWin32ExitCode: error,
        dwWaitHint: if state == SERVICE_START_PENDING {
            2_000
        } else {
            0
        },
        ..Default::default()
    };
    unsafe { SetServiceStatus(handle, &status) };
}

unsafe extern "system" fn control(
    command: u32,
    _kind: u32,
    _data: *mut core::ffi::c_void,
    _context: *mut core::ffi::c_void,
) -> u32 {
    match command {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            if let Some(stop) = STOP.get() {
                unsafe { SetEvent(stop.as_raw_handle()) };
            }
            ERROR_SUCCESS
        }
        SERVICE_CONTROL_INTERROGATE => ERROR_SUCCESS,
        _ => ERROR_CALL_NOT_IMPLEMENTED,
    }
}

fn serve(pipe: &Pipe, stop: HANDLE, mut sample: impl FnMut() -> i32) -> io::Result<()> {
    while unsafe { WaitForSingleObject(stop, 0) } != WAIT_OBJECT_0 {
        match pipe.operation(stop, IDLE_MS, |overlap, _| unsafe {
            ConnectNamedPipe(pipe.handle.as_raw_handle(), overlap)
        }) {
            Ok(_) => {}
            Err(error) if is_code(&error, ERROR_PIPE_CONNECTED) => {}
            Err(error) if is_code(&error, ERROR_NO_DATA) => {
                unsafe { DisconnectNamedPipe(pipe.handle.as_raw_handle()) };
                continue;
            }
            Err(error)
                if error.kind() == io::ErrorKind::TimedOut
                    || error.kind() == io::ErrorKind::Interrupted =>
            {
                return Ok(());
            }
            Err(error) => return Err(error),
        }
        let mut request = [0];
        if matches!(pipe.read(stop, &mut request), Ok(1)) && request == [b'T'] {
            let response = sample().to_le_bytes();
            if matches!(pipe.write(stop, &response), Ok(4)) {
                // Wait for the reader to close or acknowledge before discarding unread data.
                let _ = pipe.read(stop, &mut request);
            }
        }
        unsafe { DisconnectNamedPipe(pipe.handle.as_raw_handle()) };
    }
    Ok(())
}

#[derive(Default)]
struct SampleCache {
    sensor: Option<CpuTemperature>,
    sample: Option<(Instant, i32)>,
    opened: Option<Instant>,
}

impl SampleCache {
    fn sample(&mut self) -> i32 {
        let now = Instant::now();
        if let Some((when, value)) = self.sample
            && now.duration_since(when) < SAMPLE_INTERVAL
        {
            return value;
        }
        if self.sensor.is_none()
            && cpu_temperature::supported()
            && self
                .opened
                .is_none_or(|when| now.duration_since(when) >= REOPEN_INTERVAL)
        {
            self.opened = Some(now);
            self.sensor = trusted_pawnio().and_then(|()| CpuTemperature::open()).ok();
        }
        let value = match self.sensor.as_ref().map(CpuTemperature::celsius) {
            Some(Ok(Some(celsius))) if (0.0..=125.0).contains(&celsius) => {
                (celsius * 1000.0).round() as i32
            }
            Some(Ok(None)) => PENDING,
            _ => UNAVAILABLE,
        };
        self.sample = Some((now, value));
        value
    }
}

struct Pipe {
    handle: OwnedHandle,
    event: OwnedHandle,
}

impl Pipe {
    fn new(name: *const u16) -> io::Result<Self> {
        let security = Security::new(PIPE_DACL)?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: security.0,
            bInheritHandle: 0,
        };
        let handle = unsafe {
            CreateNamedPipeW(
                name,
                PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_REJECT_REMOTE_CLIENTS,
                1,
                16,
                16,
                MESSAGE_MS,
                &attributes,
            )
        };
        Ok(Self {
            handle: owned(handle)?,
            event: event()?,
        })
    }

    fn read(&self, stop: HANDLE, bytes: &mut [u8]) -> io::Result<u32> {
        self.operation(stop, MESSAGE_MS, |overlap, count| unsafe {
            ReadFile(
                self.handle.as_raw_handle(),
                bytes.as_mut_ptr(),
                bytes.len() as u32,
                count,
                overlap,
            )
        })
    }

    fn write(&self, stop: HANDLE, bytes: &[u8]) -> io::Result<u32> {
        self.operation(stop, MESSAGE_MS, |overlap, count| unsafe {
            WriteFile(
                self.handle.as_raw_handle(),
                bytes.as_ptr(),
                bytes.len() as u32,
                count,
                overlap,
            )
        })
    }

    fn operation(
        &self,
        stop: HANDLE,
        timeout: u32,
        start: impl FnOnce(*mut OVERLAPPED, *mut u32) -> i32,
    ) -> io::Result<u32> {
        let handle = self.handle.as_raw_handle();
        let event = self.event.as_raw_handle();
        check(unsafe { ResetEvent(event) })?;
        let mut overlap = OVERLAPPED {
            hEvent: event,
            ..Default::default()
        };
        let mut count = 0;
        if start(&mut overlap, &mut count) != 0 {
            return Ok(count);
        }
        let error = io::Error::last_os_error();
        if !is_code(&error, ERROR_IO_PENDING) {
            return Err(error);
        }
        let handles = [stop, event];
        let waited =
            unsafe { WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, timeout) };
        if waited == WAIT_OBJECT_0 + 1 {
            check(unsafe { GetOverlappedResult(handle, &overlap, &mut count, 0) })?;
            return Ok(count);
        }
        let error = match waited {
            WAIT_OBJECT_0 => io::ErrorKind::Interrupted.into(),
            WAIT_TIMEOUT => io::ErrorKind::TimedOut.into(),
            _ => io::Error::last_os_error(),
        };
        // The pending operation must stop using its OVERLAPPED and buffer before return.
        unsafe {
            CancelIoEx(handle, &overlap);
            GetOverlappedResult(handle, &overlap, &mut count, 1);
        }
        Err(error)
    }
}

struct Security(PSECURITY_DESCRIPTOR);

impl Security {
    fn new(descriptor: *const u16) -> io::Result<Self> {
        let mut pointer = null_mut();
        check(unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                descriptor,
                SDDL_REVISION_1,
                &mut pointer,
                null_mut(),
            )
        })?;
        Ok(Self(pointer))
    }
}

impl Drop for Security {
    fn drop(&mut self) {
        unsafe { LocalFree(self.0) };
    }
}

struct ServiceHandle(SC_HANDLE);

impl ServiceHandle {
    fn new(handle: SC_HANDLE) -> io::Result<Self> {
        if handle.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(handle))
        }
    }
}

impl Drop for ServiceHandle {
    fn drop(&mut self) {
        unsafe { CloseServiceHandle(self.0) };
    }
}

fn install() -> io::Result<()> {
    let image = service_image()?;
    let security = Security::new(SERVICE_DACL)?;
    let manager =
        ServiceHandle::new(unsafe { OpenSCManagerW(null(), null(), SC_MANAGER_CREATE_SERVICE) })?;
    let access = SERVICE_QUERY_CONFIG | WRITE_DAC | DELETE;
    let created = ServiceHandle::new(unsafe {
        CreateServiceW(
            manager.0,
            SERVICE,
            w!("ARGOS CPU Sensor"),
            access,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_DEMAND_START,
            SERVICE_ERROR_NORMAL,
            image.as_ptr(),
            null(),
            null_mut(),
            null(),
            null(),
            null(),
        )
    });
    let (service, new) = match created {
        Ok(service) => (service, true),
        Err(error) if is_code(&error, ERROR_SERVICE_EXISTS) => {
            let service = ServiceHandle::new(unsafe { OpenServiceW(manager.0, SERVICE, access) })?;
            validate_service(&service, &image)?;
            (service, false)
        }
        Err(error) => return Err(error),
    };
    let result = check(unsafe {
        SetServiceObjectSecurity(service.0, DACL_SECURITY_INFORMATION, security.0)
    });
    if result.is_err() && new {
        unsafe { DeleteService(service.0) };
    }
    result
}

fn uninstall() -> io::Result<()> {
    let image = service_image()?;
    let manager =
        ServiceHandle::new(unsafe { OpenSCManagerW(null(), null(), SC_MANAGER_CONNECT) })?;
    let service = ServiceHandle::new(unsafe {
        OpenServiceW(
            manager.0,
            SERVICE,
            SERVICE_QUERY_CONFIG | SERVICE_QUERY_STATUS | SERVICE_STOP | DELETE,
        )
    });
    let service = match service {
        Ok(service) => service,
        Err(error) if is_code(&error, ERROR_SERVICE_DOES_NOT_EXIST) => return Ok(()),
        Err(error) => return Err(error),
    };
    validate_service(&service, &image)?;
    let mut status = SERVICE_STATUS::default();
    if let Err(error) =
        check(unsafe { ControlService(service.0, SERVICE_CONTROL_STOP, &mut status) })
        && !is_code(&error, ERROR_SERVICE_NOT_ACTIVE)
    {
        return Err(error);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        check(unsafe { QueryServiceStatus(service.0, &mut status) })?;
        if status.dwCurrentState == SERVICE_STOPPED {
            break;
        }
        if Instant::now() >= deadline {
            return Err(io::ErrorKind::TimedOut.into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    check(unsafe { DeleteService(service.0) })
}

fn validate_service(service: &ServiceHandle, expected: &[u16]) -> io::Result<()> {
    let mut buffer = [0usize; 1024]; // QueryServiceConfig requires an aligned buffer up to 8 KiB.
    let mut needed = 0;
    let config = buffer.as_mut_ptr().cast::<QUERY_SERVICE_CONFIGW>();
    check(unsafe {
        QueryServiceConfigW(
            service.0,
            config,
            size_of::<[usize; 1024]>() as u32,
            &mut needed,
        )
    })?;
    let config = unsafe { &*config };
    let matches = !config.lpBinaryPathName.is_null()
        && !config.lpServiceStartName.is_null()
        && unsafe { wide_to_os(config.lpBinaryPathName) }
            == OsString::from_wide(&expected[..expected.len() - 1])
        && unsafe { wide_to_os(config.lpServiceStartName) }.eq_ignore_ascii_case("LocalSystem")
        && config.dwServiceType == SERVICE_WIN32_OWN_PROCESS
        && config.dwStartType == SERVICE_DEMAND_START;
    if matches {
        Ok(())
    } else {
        Err(io::Error::other(
            "Configuração inesperada do serviço ARGOS Sensor",
        ))
    }
}

fn service_image() -> io::Result<Vec<u16>> {
    let program_files = program_files_path()?;
    let canonical_root = program_files.canonicalize()?;
    let expected = canonical_root.join("ARGOS Sensor").join("argos-sensor.exe");
    if std::env::current_exe()?.canonicalize()? != expected {
        return Err(io::Error::other(
            "O serviço requer a pasta protegida ARGOS Sensor",
        ));
    }
    // The service manager expects a DOS path. Avoid the extended-path prefix in ImagePath.
    let expected = program_files.join("ARGOS Sensor").join("argos-sensor.exe");
    let image = std::iter::once(u16::from(b'"'))
        .chain(expected.as_os_str().encode_wide())
        .chain([u16::from(b'"'), 0])
        .collect();
    Ok(image)
}

fn trusted_pawnio() -> io::Result<()> {
    let expected = program_files_path()?
        .canonicalize()?
        .join("PawnIO")
        .join("PawnIOLib.dll");
    let actual = cpu_temperature::installation_dir()?
        .join("PawnIOLib.dll")
        .canonicalize()?;
    if actual == expected {
        Ok(())
    } else {
        Err(io::Error::other("PawnIOLib.dll fora da pasta protegida"))
    }
}

fn program_files_path() -> io::Result<PathBuf> {
    let mut path = null_mut();
    let result =
        unsafe { SHGetKnownFolderPath(&FOLDERID_ProgramFilesX64, 0, null_mut(), &mut path) };
    if result < 0 || path.is_null() {
        return Err(io::Error::other("Program Files indisponível"));
    }
    let value = unsafe { wide_to_os(path) };
    unsafe { CoTaskMemFree(path.cast()) };
    Ok(PathBuf::from(value))
}

unsafe fn wide_to_os(value: *const u16) -> OsString {
    let mut length = 0;
    while unsafe { *value.add(length) } != 0 {
        length += 1;
    }
    OsString::from_wide(unsafe { std::slice::from_raw_parts(value, length) })
}

fn event() -> io::Result<OwnedHandle> {
    owned(unsafe { CreateEventW(null(), 1, 0, null()) })
}

fn owned(handle: HANDLE) -> io::Result<OwnedHandle> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

fn check(success: i32) -> io::Result<()> {
    if success == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn is_code(error: &io::Error, code: u32) -> bool {
    error.raw_os_error() == Some(code as i32)
}

#[cfg(test)]
mod tests {
    use std::fs::{File, OpenOptions};
    use std::io::{Read, Write};
    use std::os::windows::fs::OpenOptionsExt;

    use windows_sys::Win32::Foundation::{ERROR_PIPE_BUSY, ERROR_PIPE_NOT_CONNECTED};
    use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;

    use super::*;

    fn connect(path: &str) -> File {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match OpenOptions::new().access_mode(0x100003).open(path) {
                Ok(file) => return file,
                Err(error) if is_code(&error, ERROR_PIPE_BUSY) && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("{error}"),
            }
        }
    }

    #[test]
    fn pipe_identifies_its_peer_and_only_answers_temperature_requests() {
        let path = format!(r"\\.\pipe\ARGOS.CpuTemperature.Test.{}", std::process::id());
        let name: Vec<u16> = path.encode_utf16().chain([0]).collect();
        let pipe = Pipe::new(name.as_ptr()).unwrap();
        let stop = event().unwrap();
        std::thread::scope(|scope| {
            let server = scope.spawn(|| serve(&pipe, stop.as_raw_handle(), || 43_125));
            let mut invalid = connect(&path);
            invalid.write_all(b"X").unwrap();
            let mut response = [0; 4];
            match invalid.read(&mut response) {
                Ok(0) => {}
                Err(error)
                    if error.kind() == io::ErrorKind::BrokenPipe
                        || is_code(&error, ERROR_PIPE_NOT_CONNECTED) => {}
                result => panic!("{result:?}"),
            }
            drop(invalid);
            let mut client = connect(&path);
            let mut pid = 0;
            check(unsafe { GetNamedPipeServerProcessId(client.as_raw_handle(), &mut pid) })
                .unwrap();
            assert_eq!(pid, std::process::id());
            client.write_all(b"T").unwrap();
            client.read_exact(&mut response).unwrap();
            assert_eq!(i32::from_le_bytes(response), 43_125);
            drop(client);
            check(unsafe { SetEvent(stop.as_raw_handle()) }).unwrap();
            server.join().unwrap().unwrap();
        });
    }
}
