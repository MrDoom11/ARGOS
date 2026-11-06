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

#[link(name = "ntdll")]
unsafe extern "system" {
    fn RtlGetVersion(version: *mut OsVersionInfo) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetActiveProcessorCount(group_number: u16) -> u32;
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

    let mut memory = MemoryStatus {
        length: size_of::<MemoryStatus>() as u32,
        ..Default::default()
    };
    // SAFETY: a estrutura tem o layout e o tamanho exigidos pela API.
    if unsafe { GlobalMemoryStatusEx(&mut memory) } == 0 {
        println!("  RAM indisponível ({})", io::Error::last_os_error());
    } else {
        println!(
            "  RAM física utilizável pelo sistema: {:.2} GiB",
            memory.total_phys as f64 / BYTES_PER_GIB
        );
    }
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
