use std::ffi::{CStr, OsString};
use std::fs::File;
use std::io::{self, Read};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Component, PathBuf, Prefix};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    ERROR_MOD_NOT_FOUND, ERROR_SUCCESS, FreeLibrary, HANDLE, HMODULE, INVALID_HANDLE_VALUE,
    WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows_sys::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RRF_SUBKEY_WOW6464KEY, RegGetValueW,
};
use windows_sys::Win32::System::Threading::{
    CreateMutexExW, MUTEX_MODIFY_STATE, ReleaseMutex, SYNCHRONIZATION_SYNCHRONIZE,
    WaitForSingleObject,
};
use windows_sys::core::w;

const MODULE_SIZE_LIMIT: u64 = 64 * 1024;
const AMD_TEMPERATURE_REGISTER: u64 = 0x59800;
const TEMPERATURE_RANGE_FLAG: u32 = 1 << 19;
const TEMPERATURE_TJ_FLAGS: u32 = 3 << 16;
const INTEL_PACKAGE_THERM_STATUS: u64 = 0x1b1;
const INTEL_TEMPERATURE_TARGET: u64 = 0x1a2;

// Public PawnIOLib C ABI. Sizes for execute are counts of u64 entries.
type Open = unsafe extern "system" fn(*mut HANDLE) -> i32;
type Load = unsafe extern "system" fn(HANDLE, *const u8, usize) -> i32;
type Execute = unsafe extern "system" fn(
    HANDLE,
    *const u8,
    *const u64,
    usize,
    *mut u64,
    usize,
    *mut usize,
) -> i32;
type ProcAddress = unsafe extern "system" fn() -> isize;

enum CpuKind {
    AmdZen,
    IntelPackageDts,
}

enum Sensor {
    AmdZen(OwnedHandle),
    IntelPackageDts,
}

pub(crate) struct CpuTemperature {
    device: OwnedHandle,
    sensor: Sensor,
    execute: Execute,
    _library: PawnIoLibrary,
}

impl CpuTemperature {
    pub(crate) fn open() -> io::Result<Self> {
        let kind =
            cpu_kind().ok_or_else(|| unsupported("Modelo de CPU sem leitura de temperatura"))?;
        let module = match kind {
            CpuKind::AmdZen => "AMDFamily17.bin",
            CpuKind::IntelPackageDts => "IntelMSR.bin",
        };
        let library = PawnIoLibrary::open()?;
        let blob = read_blob(module)?;
        // Each name is paired with its documented PawnIOLib signature.
        let open =
            unsafe { std::mem::transmute::<ProcAddress, Open>(library.symbol(c"pawnio_open")?) };
        let load =
            unsafe { std::mem::transmute::<ProcAddress, Load>(library.symbol(c"pawnio_load")?) };
        let execute = unsafe {
            std::mem::transmute::<ProcAddress, Execute>(library.symbol(c"pawnio_execute")?)
        };
        let mut device = null_mut();
        // PawnIO returns a CloseHandle-compatible handle through the writable pointer.
        check_hresult(unsafe { open(&mut device) })
            .map_err(|error| missing_component(error, "driver PawnIO indisponível"))?;
        if device.is_null() || device == INVALID_HANDLE_VALUE {
            return Err(io::Error::other("PawnIO retornou um handle inválido"));
        }
        let device = unsafe { OwnedHandle::from_raw_handle(device) };
        // The official driver validates and loads the module into this handle.
        check_hresult(unsafe { load(device.as_raw_handle(), blob.as_ptr(), blob.len()) })?;
        let sensor = match kind {
            CpuKind::AmdZen => Sensor::AmdZen(pci_mutex()?),
            CpuKind::IntelPackageDts => Sensor::IntelPackageDts,
        };

        Ok(Self {
            device,
            sensor,
            execute,
            _library: library,
        })
    }

    pub(crate) fn celsius(&self) -> io::Result<Option<f64>> {
        let temperature = match &self.sensor {
            Sensor::AmdZen(pci_mutex) => {
                let mutex = pci_mutex.as_raw_handle();
                // A busy PCI bus is skipped so it cannot hold up the dashboard.
                let _lock = match unsafe { WaitForSingleObject(mutex, 0) } {
                    WAIT_OBJECT_0 | WAIT_ABANDONED => PciLock(mutex),
                    WAIT_TIMEOUT => return Ok(None),
                    _ => return Err(io::Error::last_os_error()),
                };
                let register = self.read(c"ioctl_read_smn", AMD_TEMPERATURE_REGISTER)?;
                u32::try_from(register).ok().and_then(decode_amd_celsius)
            }
            Sensor::IntelPackageDts => {
                let target = self.read(c"ioctl_read_msr", INTEL_TEMPERATURE_TARGET)?;
                let status = self.read(c"ioctl_read_msr", INTEL_PACKAGE_THERM_STATUS)?;
                decode_intel_celsius(target, status)
            }
        };
        temperature
            .map(Some)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Temperatura inválida"))
    }

    fn read(&self, operation: &CStr, address: u64) -> io::Result<u64> {
        let mut register = 0u64;
        let mut written = 0;
        // The module has one u64 input (register address) and one u64 output.
        check_hresult(unsafe {
            (self.execute)(
                self.device.as_raw_handle(),
                operation.as_ptr().cast(),
                &address,
                1,
                &mut register,
                1,
                &mut written,
            )
        })?;
        if written != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "resposta incompleta do PawnIO",
            ));
        }
        Ok(register)
    }
}

fn pci_mutex() -> io::Result<OwnedHandle> {
    // The AMD module uses indexed PCI access and requires the shared Access_PCI mutex.
    let mutex = unsafe {
        CreateMutexExW(
            null(),
            w!(r"Global\Access_PCI"),
            0,
            MUTEX_MODIFY_STATE | SYNCHRONIZATION_SYNCHRONIZE,
        )
    };
    if mutex.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(mutex) })
}

struct PciLock(HANDLE);

impl Drop for PciLock {
    fn drop(&mut self) {
        // Only the thread that acquired the mutex releases it.
        unsafe { ReleaseMutex(self.0) };
    }
}

struct PawnIoLibrary(HMODULE);

impl PawnIoLibrary {
    fn open() -> io::Result<Self> {
        let dll = installation_dir()?.join("PawnIOLib.dll");
        let wide: Vec<_> = dll.as_os_str().encode_wide().chain([0]).collect();
        // Use only the absolute machine-wide install location and system dependencies.
        let module = unsafe {
            LoadLibraryExW(
                wide.as_ptr(),
                null_mut(),
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        };
        if module.is_null() {
            return Err(missing_component(
                io::Error::last_os_error(),
                "biblioteca PawnIO indisponível",
            ));
        }
        Ok(Self(module))
    }

    fn symbol(&self, name: &CStr) -> io::Result<ProcAddress> {
        // The module owns a valid loader reference, and the export name is NUL-terminated.
        unsafe { GetProcAddress(self.0, name.as_ptr().cast()) }
            .ok_or_else(|| unsupported("versão incompatível da biblioteca PawnIO"))
    }
}

impl Drop for PawnIoLibrary {
    fn drop(&mut self) {
        // This releases the reference from our successful LoadLibraryExW call.
        unsafe { FreeLibrary(self.0) };
    }
}

pub(crate) fn installation_dir() -> io::Result<PathBuf> {
    let mut buffer = vec![0u16; 32_768];
    let mut size = (buffer.len() * size_of::<u16>()) as u32;
    // The installer documents this machine-wide key and stores a UTF-16 REG_SZ.
    let result = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\PawnIO"),
            w!("InstallLocation"),
            RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY,
            null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if result != ERROR_SUCCESS {
        return Err(missing_component(
            io::Error::from_raw_os_error(result as i32),
            "PawnIO não instalado",
        ));
    }
    let length = size as usize / size_of::<u16>();
    let path = buffer
        .get(..length)
        .and_then(|value| value.strip_suffix(&[0]))
        .filter(|value| !value.contains(&0))
        .map(|value| PathBuf::from(OsString::from_wide(value)))
        .filter(|path| {
            path.is_absolute()
                && matches!(path.components().next(), Some(Component::Prefix(prefix))
                    if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)))
        });
    path.ok_or_else(|| unsupported("pasta de instalação do PawnIO inválida"))
}

fn read_blob(module: &str) -> io::Result<Vec<u8>> {
    let path = std::env::current_exe()?.with_file_name(module);
    let file = File::open(path).map_err(|error| {
        missing_component(error, "Módulo de temperatura do PawnIO indisponível")
    })?;
    let mut blob = Vec::new();
    file.take(MODULE_SIZE_LIMIT + 1).read_to_end(&mut blob)?;
    if blob.is_empty() || blob.len() as u64 > MODULE_SIZE_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Tamanho inválido do módulo de temperatura do PawnIO",
        ));
    }
    Ok(blob)
}

fn check_hresult(result: i32) -> io::Result<()> {
    if result >= 0 {
        Ok(())
    } else if (result as u32 & 0x1fff_0000) == 0x0007_0000 {
        Err(io::Error::from_raw_os_error(result & 0xffff))
    } else {
        Err(io::Error::other(format!(
            "PawnIO retornou HRESULT 0x{:08x}",
            result as u32
        )))
    }
}

fn missing_component(error: io::Error, message: &'static str) -> io::Error {
    if matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::Unsupported
    ) || error.raw_os_error() == Some(ERROR_MOD_NOT_FOUND as i32)
    {
        unsupported(message)
    } else {
        error
    }
}

fn unsupported(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, message)
}

pub(crate) fn supported() -> bool {
    cpu_kind().is_some()
}

#[cfg(target_arch = "x86_64")]
fn cpu_kind() -> Option<CpuKind> {
    use std::arch::x86_64::__cpuid;

    // CPUID is available on x86_64 and does not require kernel access.
    let vendor = __cpuid(0);
    let id = (
        vendor.ebx.to_le_bytes(),
        vendor.edx.to_le_bytes(),
        vendor.ecx.to_le_bytes(),
    );
    if id == (*b"Genu", *b"ineI", *b"ntel") && vendor.eax >= 6 {
        return (__cpuid(6).eax & (1 << 6) != 0).then_some(CpuKind::IntelPackageDts);
    }
    if id != (*b"Auth", *b"enti", *b"cAMD") || vendor.eax < 1 {
        return None;
    }
    // Zen families share the Tctl register in the System Management Network.
    let signature = __cpuid(1).eax;
    let base_family = (signature >> 8) & 0xf;
    let family = base_family + ((signature >> 20) & 0xff);
    (base_family == 0xf && matches!(family, 0x17 | 0x19 | 0x1a)).then_some(CpuKind::AmdZen)
}

#[cfg(not(target_arch = "x86_64"))]
fn cpu_kind() -> Option<CpuKind> {
    None
}

fn decode_amd_celsius(register: u32) -> Option<f64> {
    let ticks = register >> 21;
    if ticks == 0 || register == u32::MAX {
        return None;
    }
    // The Zen register uses 0.125 °C increments with an alternate range offset.
    let mut celsius = f64::from(ticks) * 0.125;
    if register & TEMPERATURE_RANGE_FLAG != 0
        || register & TEMPERATURE_TJ_FLAGS == TEMPERATURE_TJ_FLAGS
    {
        celsius -= 49.0;
    }
    (0.0..=125.0).contains(&celsius).then_some(celsius)
}

fn decode_intel_celsius(target: u64, status: u64) -> Option<f64> {
    // IA32_TEMPERATURE_TARGET supplies TjMax. Package DTS reports degrees below it.
    let tj_max = ((target >> 16) & 0xff) as i16;
    let delta = ((status >> 16) & 0x7f) as i16;
    (status != u64::MAX && (40..=125).contains(&tj_max) && delta <= tj_max)
        .then_some(f64::from(tj_max - delta))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_zen_temperature_and_applies_the_alternate_range() {
        assert_eq!(decode_amd_celsius(421 << 21), Some(52.625));
        assert_eq!(
            decode_amd_celsius((813 << 21) | TEMPERATURE_RANGE_FLAG),
            Some(52.625)
        );
        assert_eq!(
            decode_amd_celsius((813 << 21) | TEMPERATURE_TJ_FLAGS),
            Some(52.625)
        );
    }

    #[test]
    fn rejects_empty_and_out_of_range_registers() {
        assert_eq!(decode_amd_celsius(0), None);
        assert_eq!(decode_amd_celsius(u32::MAX), None);
        assert_eq!(decode_amd_celsius(1_100 << 21), None);
    }

    #[test]
    fn intel_package_dts_uses_tjmax_and_rejects_invalid_readings() {
        assert_eq!(decode_intel_celsius(100 << 16, 42 << 16), Some(58.0));
        assert_eq!(decode_intel_celsius(105 << 16, 0), Some(105.0));
        assert_eq!(decode_intel_celsius(0, 42 << 16), None);
        assert_eq!(decode_intel_celsius(100 << 16, 127 << 16), None);
        assert_eq!(decode_intel_celsius(100 << 16, u64::MAX), None);
    }
}
