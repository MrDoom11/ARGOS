use std::ffi::{CStr, OsString};
use std::fs::File;
use std::io::{self, Read};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Component, PathBuf, Prefix};
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    ERROR_INSUFFICIENT_BUFFER, ERROR_MOD_NOT_FOUND, ERROR_SUCCESS, FreeLibrary, HANDLE, HMODULE,
    INVALID_HANDLE_VALUE, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows_sys::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RRF_SUBKEY_WOW6464KEY, RegGetValueW,
};
use windows_sys::Win32::System::SystemInformation::{
    GetLogicalProcessorInformationEx, LOGICAL_PROCESSOR_RELATIONSHIP, RelationProcessorDie,
    RelationProcessorPackage, SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
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
const INTEL_TEMPERATURE_VALID: u64 = 1 << 31;
const MAX_PACKAGE_WATTS: f64 = 2_000.0;
const MAX_POWER_SAMPLE_GAP: Duration = Duration::from_secs(5);

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
    power: Option<PackagePower>,
    execute: Execute,
    _library: PawnIoLibrary,
}

struct PackagePower {
    registers: EnergyRegisters,
    joules_per_tick: Option<f64>,
    previous: Option<EnergySample>,
}

#[derive(Clone, Copy)]
struct EnergyRegisters {
    unit: u64,
    counter: u64,
}

#[derive(Clone, Copy)]
struct EnergySample {
    at: Instant,
    counter: u32,
}

impl PackagePower {
    fn sample(&mut self, mut read: impl FnMut(u64) -> io::Result<u64>) -> io::Result<Option<f64>> {
        // A falha de leitura interrompe o intervalo usado para calcular potência.
        let previous = self.previous.take();
        let joules_per_tick = match self.joules_per_tick {
            Some(unit) => unit,
            None => {
                let register = read(self.registers.unit)?;
                if register == 0 || register == u64::MAX {
                    return Err(invalid_energy_counter());
                }
                let exponent = ((register >> 8) & 0x1f) as i32;
                let unit = 2_f64.powi(-exponent);
                self.joules_per_tick = Some(unit);
                unit
            }
        };
        let register = read(self.registers.counter)?;
        if register == u64::MAX {
            return Err(invalid_energy_counter());
        }
        let current = EnergySample {
            at: Instant::now(),
            counter: register as u32,
        };
        self.previous = Some(current);
        Ok(previous.and_then(|previous| current.watts_since(previous, joules_per_tick)))
    }
}

impl EnergySample {
    fn watts_since(self, previous: Self, joules_per_tick: f64) -> Option<f64> {
        let interval = self.at.checked_duration_since(previous.at)?;
        if interval.is_zero() || interval > MAX_POWER_SAMPLE_GAP {
            return None;
        }
        let seconds = interval.as_secs_f64();
        let counter_span_joules = (f64::from(u32::MAX) + 1.0) * joules_per_tick;
        if seconds * MAX_PACKAGE_WATTS >= counter_span_joules {
            return None;
        }
        let ticks = self.counter.wrapping_sub(previous.counter);
        let watts = f64::from(ticks) * joules_per_tick / seconds;
        (watts <= MAX_PACKAGE_WATTS).then_some(watts)
    }
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
        let power = energy_registers(&kind)
            .filter(|_| single_processor_unit(RelationProcessorPackage))
            // Alguns Intel têm um contador por die em um mesmo pacote.
            .filter(|_| {
                matches!(kind, CpuKind::AmdZen) || single_processor_unit(RelationProcessorDie)
            })
            .map(|registers| PackagePower {
                registers,
                joules_per_tick: None,
                previous: None,
            });
        let sensor = match kind {
            CpuKind::AmdZen => Sensor::AmdZen(pci_mutex()?),
            CpuKind::IntelPackageDts => Sensor::IntelPackageDts,
        };

        Ok(Self {
            device,
            sensor,
            power,
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
                return decode_intel_celsius(target, status);
            }
        };
        temperature.map(Some).ok_or_else(invalid_temperature)
    }

    pub(crate) fn watts(&mut self) -> io::Result<Option<f64>> {
        let power = self
            .power
            .as_mut()
            .ok_or_else(|| unsupported("Potência do pacote indisponível neste processador"))?;
        let device = self.device.as_raw_handle();
        let execute = self.execute;
        power.sample(|address| read_register(device, execute, c"ioctl_read_msr", address))
    }

    fn read(&self, operation: &CStr, address: u64) -> io::Result<u64> {
        read_register(
            self.device.as_raw_handle(),
            self.execute,
            operation,
            address,
        )
    }
}

fn read_register(
    device: HANDLE,
    execute: Execute,
    operation: &CStr,
    address: u64,
) -> io::Result<u64> {
    let mut register = 0;
    let mut written = 0;
    // The module has one u64 input (register address) and one u64 output.
    check_hresult(unsafe {
        execute(
            device,
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

fn invalid_energy_counter() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "Contador de energia inválido")
}

fn single_processor_unit(relationship: LOGICAL_PROCESSOR_RELATIONSHIP) -> bool {
    type ProcessorInfo = SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX;
    let mut bytes = 0;
    // A chamada inicial informa o espaço para os registros de tamanho variável.
    let queried = unsafe { GetLogicalProcessorInformationEx(relationship, null_mut(), &mut bytes) };
    if queried != 0
        || io::Error::last_os_error().raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32)
        || !(8..=65_536).contains(&bytes)
    {
        return false;
    }
    let mut info =
        vec![ProcessorInfo::default(); (bytes as usize).div_ceil(size_of::<ProcessorInfo>())];
    // O buffer tem o alinhamento e a capacidade exigidos pela API.
    let received =
        unsafe { GetLogicalProcessorInformationEx(relationship, info.as_mut_ptr(), &mut bytes) };
    // Só há uma unidade se o primeiro registro ocupar todo o espaço preenchido.
    received != 0 && info[0].Relationship == relationship && info[0].Size == bytes
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

#[cfg(target_arch = "x86_64")]
fn energy_registers(kind: &CpuKind) -> Option<EnergyRegisters> {
    use std::arch::x86_64::__cpuid;

    match kind {
        CpuKind::AmdZen => {
            // CPUID.80000007H:EDX[14] anuncia os contadores de energia AMD RAPL.
            let supported = __cpuid(0x8000_0000).eax >= 0x8000_0007
                && __cpuid(0x8000_0007).edx & (1 << 14) != 0;
            supported.then_some(EnergyRegisters {
                unit: 0xc001_0299,
                counter: 0xc001_029b,
            })
        }
        CpuKind::IntelPackageDts => {
            let signature = __cpuid(1).eax;
            let family = (signature >> 8) & 0xf;
            let model = ((signature >> 12) & 0xf0) | ((signature >> 4) & 0xf);
            // Os Atom Silvermont e Airmont abaixo usam outra codificação de unidade.
            (family == 6 && !matches!(model, 0x37 | 0x4a | 0x4c | 0x5a)).then_some(
                EnergyRegisters {
                    unit: 0x606,
                    counter: 0x611,
                },
            )
        }
    }
}

#[cfg(not(target_arch = "x86_64"))]
fn energy_registers(_: &CpuKind) -> Option<EnergyRegisters> {
    None
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

fn decode_intel_celsius(target: u64, status: u64) -> io::Result<Option<f64>> {
    if status == u64::MAX {
        return Err(invalid_temperature());
    }
    if status & INTEL_TEMPERATURE_VALID == 0 {
        return Ok(None);
    }
    // IA32_TEMPERATURE_TARGET supplies TjMax. Package DTS reports degrees below it.
    let tj_max = ((target >> 16) & 0xff) as i16;
    let delta = ((status >> 16) & 0xff) as i16;
    ((40..=125).contains(&tj_max) && delta <= tj_max)
        .then_some(Some(f64::from(tj_max - delta)))
        .ok_or_else(invalid_temperature)
}

fn invalid_temperature() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "Temperatura inválida")
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
        let valid = INTEL_TEMPERATURE_VALID;
        assert_eq!(
            decode_intel_celsius(100 << 16, valid | (42 << 16)).unwrap(),
            Some(58.0)
        );
        assert_eq!(decode_intel_celsius(105 << 16, valid).unwrap(), Some(105.0));
        assert_eq!(decode_intel_celsius(105 << 16, 0).unwrap(), None);
        assert_eq!(decode_intel_celsius(100 << 16, 42 << 16).unwrap(), None);
        assert!(decode_intel_celsius(0, valid | (42 << 16)).is_err());
        assert!(decode_intel_celsius(100 << 16, valid | (128 << 16)).is_err());
        assert!(decode_intel_celsius(100 << 16, u64::MAX).is_err());
    }

    #[test]
    fn package_power_uses_elapsed_seconds_and_handles_counter_wrap() {
        let start = Instant::now();
        let previous = EnergySample {
            at: start,
            counter: u32::MAX - 1023,
        };
        let current = EnergySample {
            at: start + Duration::from_millis(500),
            counter: 1024,
        };
        assert_eq!(current.watts_since(previous, 2_f64.powi(-10)), Some(4.0));
        assert_eq!(previous.watts_since(previous, 2_f64.powi(-10)), None);

        let after_sleep = EnergySample {
            at: start + Duration::from_secs(60),
            ..current
        };
        assert_eq!(after_sleep.watts_since(previous, 2_f64.powi(-10)), None);
        assert_eq!(current.watts_since(previous, 2_f64.powi(-31)), None);
    }

    #[test]
    fn package_power_recovers_after_a_failed_reading() {
        let registers = EnergyRegisters {
            unit: 0x606,
            counter: 0x611,
        };
        let mut power = PackagePower {
            registers,
            joules_per_tick: None,
            previous: None,
        };
        assert_eq!(
            power
                .sample(|register| match register {
                    0x606 => Ok(16 << 8),
                    0x611 => Ok(1000),
                    _ => unreachable!(),
                })
                .unwrap(),
            None
        );
        assert!(power.previous.is_some());
        assert!(
            power
                .sample(|_| Err(io::Error::other("falha temporária")))
                .is_err()
        );
        assert!(power.previous.is_none());
        assert_eq!(
            power
                .sample(|register| {
                    assert_eq!(register, registers.counter);
                    Ok(2000)
                })
                .unwrap(),
            None
        );
        assert!(power.previous.is_some());
    }
}
