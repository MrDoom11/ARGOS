use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, DISK_SPACE_INFORMATION, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    GetDiskSpaceInformationW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
    GetVolumeNameForVolumeMountPointW, OPEN_EXISTING,
};
use windows_sys::Win32::System::Diagnostics::Debug::{
    GetThreadErrorMode, SEM_FAILCRITICALERRORS, SetThreadErrorMode,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    DISK_PERFORMANCE, IOCTL_DISK_PERFORMANCE, IOCTL_DISK_PERFORMANCE_OFF,
};
use windows_sys::Win32::System::WindowsProgramming::{DRIVE_FIXED, DRIVE_REMOVABLE};

pub(crate) const READ: u8 = 1;
pub(crate) const WRITE: u8 = 1 << 1;
pub(crate) const ACTIVE: u8 = 1 << 2;
pub(crate) const SPACE: u8 = 1 << 3;
const PERFORMANCE: u8 = READ | WRITE | ACTIVE;
const SPACE_INTERVAL: Duration = Duration::from_secs(5);
const RETRY_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Volume {
    pub(crate) id: String,
    pub(crate) label: String,
    root: Vec<u16>,
    name: String,
}

pub(crate) fn valid_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(position, byte)| {
            if matches!(position, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
            }
        })
}

pub(crate) fn volumes(previous: &[Volume]) -> io::Result<Vec<Volume>> {
    let _error_mode = CriticalErrorMode::suppress();
    let drives = unsafe { GetLogicalDrives() };
    if drives == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut volumes = Vec::<Volume>::new();
    for letter in b'A'..=b'Z' {
        if drives & (1 << (letter - b'A')) == 0 {
            continue;
        }
        let letter_root = [u16::from(letter), b':' as u16, b'\\' as u16, 0];
        let drive_type = unsafe { GetDriveTypeW(letter_root.as_ptr()) };
        if !matches!(drive_type, DRIVE_FIXED | DRIVE_REMOVABLE) {
            continue;
        }
        let mut guid = [0; 50];
        if unsafe {
            GetVolumeNameForVolumeMountPointW(
                letter_root.as_ptr(),
                guid.as_mut_ptr(),
                guid.len() as u32,
            )
        } == 0
        {
            continue;
        }
        let path = wide_string(&guid).to_ascii_lowercase();
        let Some(id) = path
            .strip_prefix(r"\\?\volume{")
            .and_then(|path| path.strip_suffix("}\\"))
            .filter(|id| valid_id(id))
        else {
            continue;
        };
        if volumes.iter().any(|volume| volume.id == id) {
            continue;
        }
        let root = path.encode_utf16().chain([0]).collect::<Vec<_>>();
        // Only query the filesystem for a newly seen volume. Regular inventory
        // checks do not need to wake a disk just to read its label again.
        let name = previous
            .iter()
            .find(|volume| volume.id == id)
            .map_or_else(|| volume_name(&root), |volume| volume.name.clone());
        let mut label = format!("{}:", char::from(letter));
        if !name.is_empty() {
            label.push(' ');
            label.push_str(&name);
        }
        volumes.push(Volume {
            id: id.to_owned(),
            label,
            root,
            name,
        });
    }
    Ok(volumes)
}

fn wide_string(buffer: &[u16]) -> String {
    let end = buffer
        .iter()
        .position(|&code| code == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..end])
}

fn volume_name(root: &[u16]) -> String {
    let mut buffer = [0; 261];
    let success = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            null_mut(),
            null_mut(),
            null_mut(),
            null_mut(),
            0,
        )
    };
    if success == 0 {
        return String::new();
    }
    wide_string(&buffer).trim().to_owned()
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Sample {
    pub(crate) read_bytes_per_sec: Option<f64>,
    pub(crate) write_bytes_per_sec: Option<f64>,
    pub(crate) active_percent: Option<f64>,
    pub(crate) used_percent: Option<f64>,
}

pub(crate) struct Sampler {
    volumes: Vec<VolumeSampler>,
}

impl Sampler {
    pub(crate) fn open(volumes: &[(&Volume, u8)]) -> Self {
        let mut sampler = Self {
            volumes: Vec::new(),
        };
        sampler.set_volumes(volumes);
        sampler
    }

    pub(crate) fn set_volumes(&mut self, volumes: &[(&Volume, u8)]) {
        let mut previous = std::mem::take(&mut self.volumes);
        self.volumes = volumes
            .iter()
            .map(|&(volume, mask)| {
                let mut sampler = previous
                    .iter()
                    .position(|sampler| sampler.id == volume.id)
                    .map_or_else(
                        || VolumeSampler::new(volume),
                        |index| previous.swap_remove(index),
                    );
                sampler.set_mask(mask);
                sampler
            })
            .collect();
    }

    pub(crate) fn sample(&mut self) -> Vec<Sample> {
        let _error_mode = CriticalErrorMode::suppress();
        self.volumes.iter_mut().map(VolumeSampler::sample).collect()
    }
}

struct VolumeSampler {
    id: String,
    root: Vec<u16>,
    device_path: Vec<u16>,
    mask: u8,
    source: Option<Device>,
    previous: Option<Counters>,
    retry_at: Option<Instant>,
    space: Option<(Instant, Option<f64>)>,
}

impl VolumeSampler {
    fn new(volume: &Volume) -> Self {
        let mut device_path = volume.root.clone();
        // Leave the volume GUID path's trailing backslash on the filesystem
        // root only. CreateFileW opens the actual volume without it.
        let backslash = device_path.len() - 2;
        device_path[backslash] = 0;
        Self {
            id: volume.id.clone(),
            root: volume.root.clone(),
            device_path,
            mask: 0,
            source: None,
            previous: None,
            retry_at: None,
            space: None,
        }
    }

    fn set_mask(&mut self, mask: u8) {
        if mask & PERFORMANCE == 0 {
            self.release_source();
            self.previous = None;
            self.retry_at = None;
        }
        if mask & SPACE == 0 {
            self.space = None;
        }
        self.mask = mask;
    }

    fn sample(&mut self) -> Sample {
        let now = Instant::now();
        let mut sample = Sample::default();
        if self.mask & PERFORMANCE != 0 && self.retry_at.is_none_or(|at| now >= at) {
            let current = self.read_counters();
            match (self.previous, current) {
                (Some(previous), Some(current)) => sample = current.since(previous, self.mask),
                (_, None) => self.retry_at = Some(now + RETRY_INTERVAL),
                (None, Some(_)) => {}
            }
            self.previous = current;
        }
        if self.mask & SPACE != 0 {
            if self.space.is_none_or(|(at, _)| now >= at) {
                self.space = Some((now + SPACE_INTERVAL, used_percent(&self.root)));
            }
            sample.used_percent = self.space.and_then(|(_, used)| used);
        }
        sample
    }

    fn read_counters(&mut self) -> Option<Counters> {
        // Close the volume handle after each query so it does not obstruct
        // ejecting removable drives between samples.
        let handle = open_volume(&self.device_path)?;
        let mut data = DISK_PERFORMANCE::default();
        let mut returned = 0;
        let size = size_of::<DISK_PERFORMANCE>() as u32;
        let success = unsafe {
            DeviceIoControl(
                handle.as_raw_handle(),
                IOCTL_DISK_PERFORMANCE,
                null(),
                0,
                (&mut data as *mut DISK_PERFORMANCE).cast(),
                size,
                &mut returned,
                null_mut(),
            )
        };
        if success == 0 {
            return None;
        }
        let Some(current) = (returned >= size)
            .then(|| Counters::from_raw(data))
            .flatten()
        else {
            release_performance(&handle);
            return None;
        };
        // Every successful query requests collection in the driver. Retain
        // one reference for the interval and balance the additional requests.
        if self.source == Some(current.device) {
            release_performance(&handle);
        } else {
            self.source = Some(current.device);
        }
        Some(current)
    }

    fn release_source(&mut self) {
        if self.source.take().is_some() {
            let _error_mode = CriticalErrorMode::suppress();
            if let Some(handle) = open_volume(&self.device_path) {
                release_performance(&handle);
            }
        }
    }
}

impl Drop for VolumeSampler {
    fn drop(&mut self) {
        self.release_source();
    }
}

fn open_volume(device_path: &[u16]) -> Option<OwnedHandle> {
    let handle = unsafe {
        CreateFileW(
            device_path.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null(),
            OPEN_EXISTING,
            0,
            null_mut(),
        )
    };
    (handle != INVALID_HANDLE_VALUE).then(|| unsafe { OwnedHandle::from_raw_handle(handle) })
}

fn release_performance(handle: &OwnedHandle) {
    let mut returned = 0;
    unsafe {
        DeviceIoControl(
            handle.as_raw_handle(),
            IOCTL_DISK_PERFORMANCE_OFF,
            null(),
            0,
            null_mut(),
            0,
            &mut returned,
            null_mut(),
        )
    };
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct Device {
    number: u32,
    manager: [u16; 8],
}

#[derive(Clone, Copy)]
struct Counters {
    device: Device,
    read: u64,
    written: u64,
    idle: u64,
    time: u64,
}

impl Counters {
    fn from_raw(data: DISK_PERFORMANCE) -> Option<Self> {
        let manager = data
            .StorageManagerName
            .map(|code| u8::try_from(code).unwrap_or_default());
        // A physical device's totals cannot be attributed to this volume.
        if manager.starts_with(b"PARTMGR") || manager.eq_ignore_ascii_case(b"PhysDisk") {
            return None;
        }
        Some(Self {
            device: Device {
                number: data.StorageDeviceNumber,
                manager: data.StorageManagerName,
            },
            read: data.BytesRead.try_into().ok()?,
            written: data.BytesWritten.try_into().ok()?,
            idle: data.IdleTime.try_into().ok()?,
            time: data.QueryTime.try_into().ok()?,
        })
    }

    fn since(self, previous: Self, mask: u8) -> Sample {
        if self.device != previous.device || self.time <= previous.time {
            return Sample::default();
        }
        let elapsed_100ns = (self.time - previous.time) as f64;
        let elapsed = elapsed_100ns / 10_000_000.0;
        Sample {
            read_bytes_per_sec: self
                .read
                .checked_sub(previous.read)
                .filter(|_| mask & READ != 0)
                .map(|bytes| bytes as f64 / elapsed),
            write_bytes_per_sec: self
                .written
                .checked_sub(previous.written)
                .filter(|_| mask & WRITE != 0)
                .map(|bytes| bytes as f64 / elapsed),
            active_percent: self
                .idle
                .checked_sub(previous.idle)
                .filter(|_| mask & ACTIVE != 0)
                .map(|idle| (100.0 * (1.0 - idle as f64 / elapsed_100ns)).clamp(0.0, 100.0)),
            used_percent: None,
        }
    }
}

fn used_percent(root: &[u16]) -> Option<f64> {
    let mut data = DISK_SPACE_INFORMATION::default();
    if unsafe { GetDiskSpaceInformationW(root.as_ptr(), &mut data) } != 0
        || data.ActualTotalAllocationUnits == 0
    {
        return None;
    }
    let used = data
        .ActualTotalAllocationUnits
        .saturating_sub(data.ActualAvailableAllocationUnits);
    Some(100.0 * used as f64 / data.ActualTotalAllocationUnits as f64)
}

struct CriticalErrorMode(Option<u32>);

impl CriticalErrorMode {
    fn suppress() -> Self {
        // A drive may disappear while a filesystem query is in flight.
        let mode = unsafe { GetThreadErrorMode() };
        let mut previous = 0;
        let changed = unsafe { SetThreadErrorMode(mode | SEM_FAILCRITICALERRORS, &mut previous) };
        Self((changed != 0).then_some(previous))
    }
}

impl Drop for CriticalErrorMode {
    fn drop(&mut self) {
        if let Some(previous) = self.0 {
            unsafe { SetThreadErrorMode(previous, null_mut()) };
        }
    }
}
