use std::ffi::c_void;
use std::io;
use std::mem::{size_of, size_of_val};
use std::ptr::null_mut;

const ALL_PROCESSOR_GROUPS: u16 = 0xffff;
const HKEY_LOCAL_MACHINE: i32 = 0x8000_0002_u32 as i32;
const RRF_RT_REG_SZ: u32 = 0x0000_0002;
const BYTES_PER_GIB: f64 = (1_u64 << 30) as f64;

#[repr(C)]
struct OsVersionInfo {
    size: u32,
    major: u32,
    minor: u32,
    build: u32,
    platform: u32,
    service_pack: [u16; 128],
}

#[repr(C)]
#[derive(Default)]
struct MemoryStatus {
    length: u32,
    memory_load: u32,
    total_phys: u64,
    avail_phys: u64,
    total_page_file: u64,
    avail_page_file: u64,
    total_virtual: u64,
    avail_virtual: u64,
    avail_extended_virtual: u64,
}

#[repr(C)]
#[derive(Default)]
struct FileTime {
    low: u32,
    high: u32,
}

impl FileTime {
    fn ticks(self) -> u64 {
        (u64::from(self.high) << 32) | u64::from(self.low)
    }
}

#[derive(Clone, Copy)]
struct CpuTimes {
    idle: u64,
    kernel: u64,
    user: u64,
}

impl CpuTimes {
    fn read() -> io::Result<Self> {
        // SAFETY: a função não recebe argumentos nem exige inicialização.
        match unsafe { GetActiveProcessorGroupCount() } {
            1 => {}
            0 => {
                return Err(io::Error::other(
                    "contagem de grupos de processadores indisponível",
                ));
            }
            // GetSystemTimes só cobre o grupo primário em sistemas com vários grupos.
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "uso total da CPU indisponível em sistemas com vários grupos de processadores",
                ));
            }
        }

        let mut idle = FileTime::default();
        let mut kernel = FileTime::default();
        let mut user = FileTime::default();
        // SAFETY: os três ponteiros referenciam buffers distintos com layout FILETIME.
        if unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            idle: idle.ticks(),
            kernel: kernel.ticks(),
            user: user.ticks(),
        })
    }

    fn usage_since(self, previous: Self) -> io::Result<Option<f64>> {
        let invalid = || {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "contadores de CPU inconsistentes",
            )
        };
        let idle = self.idle.checked_sub(previous.idle).ok_or_else(invalid)?;
        let kernel = self
            .kernel
            .checked_sub(previous.kernel)
            .ok_or_else(invalid)?;
        let user = self.user.checked_sub(previous.user).ok_or_else(invalid)?;
        let total = kernel.checked_add(user).ok_or_else(invalid)?;
        // O Windows inclui o tempo ocioso no contador do kernel.
        if idle > kernel {
            return Err(invalid());
        }
        Ok((total != 0).then(|| 100.0 * (total - idle) as f64 / total as f64))
    }
}

#[link(name = "ntdll")]
unsafe extern "system" {
    fn RtlGetVersion(version: *mut OsVersionInfo) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetActiveProcessorCount(group_number: u16) -> u32;
    fn GetActiveProcessorGroupCount() -> u16;
    fn GetSystemTimes(idle: *mut FileTime, kernel: *mut FileTime, user: *mut FileTime) -> i32;
    fn GlobalMemoryStatusEx(status: *mut MemoryStatus) -> i32;
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn RegGetValueW(
        key: *mut c_void,
        sub_key: *const u16,
        value: *const u16,
        flags: u32,
        data_type: *mut u32,
        data: *mut c_void,
        data_size: *mut u32,
    ) -> i32;
}

#[derive(Default)]
pub(crate) struct SystemSampler {
    previous: Option<CpuTimes>,
}

impl SystemSampler {
    pub(crate) fn cpu_usage(&mut self) -> io::Result<Option<f64>> {
        // Uma falha de leitura descarta a base para que a próxima amostra recomece a janela.
        let previous = self.previous.take();
        let current = CpuTimes::read()?;
        self.previous = Some(current);
        match previous {
            Some(previous) => current.usage_since(previous),
            None => Ok(None),
        }
    }
}

pub(crate) struct MemoryUsage {
    pub(crate) used: u64,
    pub(crate) total: u64,
}

pub(crate) fn memory_usage() -> io::Result<MemoryUsage> {
    let memory = read_memory_status()?;
    if memory.total_phys == 0 || memory.avail_phys > memory.total_phys {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "contadores de RAM inconsistentes",
        ));
    }
    Ok(MemoryUsage {
        used: memory.total_phys - memory.avail_phys,
        total: memory.total_phys,
    })
}

pub(crate) fn report() {
    println!("Sistema");

    let mut version = OsVersionInfo {
        size: size_of::<OsVersionInfo>() as u32,
        major: 0,
        minor: 0,
        build: 0,
        platform: 0,
        service_pack: [0; 128],
    };
    // SAFETY: a estrutura tem o layout e o tamanho exigidos pela API.
    let status = unsafe { RtlGetVersion(&mut version) };
    if status == 0 {
        println!(
            "  Windows NT {}.{} (build {})",
            version.major, version.minor, version.build
        );
    } else {
        println!("  Versão do Windows indisponível (NTSTATUS 0x{status:08X})");
    }

    match processor_name() {
        Ok(name) => println!("  CPU: {name}"),
        Err(error) => println!("  Modelo da CPU indisponível ({error})"),
    }

    // SAFETY: ALL_PROCESSOR_GROUPS solicita a contagem em todo o sistema.
    let logical_processors = unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) };
    if logical_processors == 0 {
        println!(
            "  Contagem de processadores lógicos indisponível ({})",
            io::Error::last_os_error()
        );
    } else {
        println!("  Processadores lógicos ativos: {logical_processors}");
    }

    match read_memory_status() {
        Ok(memory) => println!(
            "  RAM física utilizável pelo sistema: {:.2} GiB",
            memory.total_phys as f64 / BYTES_PER_GIB
        ),
        Err(error) => println!("  RAM indisponível ({error})"),
    }
}

fn read_memory_status() -> io::Result<MemoryStatus> {
    let mut memory = MemoryStatus {
        length: size_of::<MemoryStatus>() as u32,
        ..Default::default()
    };
    // SAFETY: a estrutura tem o layout e o tamanho exigidos pela API.
    if unsafe { GlobalMemoryStatusEx(&mut memory) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(memory)
}

fn processor_name() -> io::Result<String> {
    let sub_key: Vec<u16> = "HARDWARE\\DESCRIPTION\\System\\CentralProcessor\\0\0"
        .encode_utf16()
        .collect();
    let value: Vec<u16> = "ProcessorNameString\0".encode_utf16().collect();
    let mut data = [0_u16; 256];
    let mut data_size = size_of_val(&data) as u32;

    // SAFETY: os nomes terminam em NUL, o tamanho do buffer está em bytes
    // e a constante HKEY deve ser estendida com sinal em 64 bits.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE as isize as *mut c_void,
            sub_key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            null_mut(),
            data.as_mut_ptr().cast(),
            &mut data_size,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status));
    }

    let end = data
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(data.len());
    let name = String::from_utf16_lossy(&data[..end]);
    let name = name.trim();
    if name.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "nome vazio"));
    }
    Ok(name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_usage_excludes_idle_time_from_kernel_time() {
        let previous = CpuTimes {
            idle: 100,
            kernel: 200,
            user: 50,
        };
        let current = CpuTimes {
            idle: 150,
            kernel: 275,
            user: 75,
        };
        assert_eq!(current.usage_since(previous).unwrap(), Some(50.0));

        let idle = CpuTimes {
            idle: 150,
            kernel: 250,
            user: 50,
        };
        assert_eq!(idle.usage_since(previous).unwrap(), Some(0.0));
        assert_eq!(previous.usage_since(previous).unwrap(), None);
        assert!(previous.usage_since(current).is_err());
    }
}
