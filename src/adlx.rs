//! Read-only bindings for the ADLX C ABI provided by the AMD display driver.
//! https://github.com/GPUOpen-LibrariesAndSDKs/ADLX/tree/main/SDK/Include

use std::cell::Cell;
use std::ffi::{CStr, c_char, c_void};
use std::marker::PhantomData;
use std::mem;
use std::ptr::{NonNull, null, null_mut};
use std::time::Duration;

use windows_sys::Win32::Foundation::{FreeLibrary, HMODULE, LUID};
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows_sys::core::w;

type Ptr = *mut c_void;
type Slot = *const c_void;
type Get<T> = unsafe extern "system" fn(Ptr, *mut T) -> i32;
type GetGpuInterface = unsafe extern "system" fn(Ptr, Ptr, *mut Ptr) -> i32;
type Action = unsafe extern "system" fn(Ptr) -> i32;
type QueryFullVersion = unsafe extern "C" fn(*mut u64) -> i32;
type Initialize = unsafe extern "C" fn(u64, *mut Ptr) -> i32;
type Initialize2 = unsafe extern "C" fn(u64, *mut Ptr, *mut Ptr) -> i32;
type Terminate = unsafe extern "C" fn() -> i32;
type ProcAddress = unsafe extern "system" fn() -> isize;

const OK: i32 = 0;
const PENDING_OPERATION: i32 = 13;

#[repr(C)]
struct RefCountedVtbl {
    _acquire: Slot,
    release: Action,
    query: unsafe extern "system" fn(Ptr, *const u16, *mut Ptr) -> i32,
}

// IADLXSystem and IADLMapping are borrowed for the lifetime of ADLX.
#[repr(C)]
struct SystemVtbl {
    _hybrid_graphics_type: Slot,
    gpus: Get<Ptr>,
    _unused: [Slot; 7],
    performance_monitoring: Get<Ptr>,
}

#[repr(C)]
struct MappingVtbl {
    _unused: [Slot; 2],
    bdf_from_gpu: unsafe extern "system" fn(Ptr, Ptr, *mut i32, *mut i32, *mut i32) -> i32,
}

// The IADLXGPUList and IADLXGPUMetricsList prefixes have the same ABI.
// The final field is each list's typed At method.
#[repr(C)]
struct ListVtbl {
    _base: RefCountedVtbl,
    size: unsafe extern "system" fn(Ptr) -> u32,
    _empty: Slot,
    begin: unsafe extern "system" fn(Ptr) -> u32,
    _unused: [Slot; 5],
    at: unsafe extern "system" fn(Ptr, u32, *mut Ptr) -> i32,
}

#[repr(C)]
struct GpuVtbl {
    _unused: [Slot; 7],
    name: Get<*const c_char>,
    _driver_path: Slot,
    pnp_string: Get<*const c_char>,
    _has_desktops: Slot,
    total_vram: Get<u32>,
}

// IADLXGPU2 adds LUID after the inherited GPU and GPU1 methods (ISystem2.h).
#[repr(C)]
struct Gpu2Vtbl {
    _base: RefCountedVtbl,
    _gpu: [Slot; 16],
    _gpu1: [Slot; 4],
    _gpu2_before_luid: [Slot; 11],
    luid: Get<LUID>,
}

#[repr(C)]
struct MonitoringVtbl {
    _unused: [Slot; 5],
    sampling_interval: Get<i32>,
    _history_settings: [Slot; 5],
    start_tracking: Action,
    stop_tracking: Action,
    _all_metrics_history: Slot,
    gpu_metrics_history: unsafe extern "system" fn(Ptr, Ptr, i32, i32, *mut Ptr) -> i32,
    _unused_current: [Slot; 3],
    current_gpu_metrics: GetGpuInterface,
    _current_system_and_fps: [Slot; 2],
    supported_gpu_metrics: GetGpuInterface,
}

#[repr(C)]
struct SupportVtbl {
    _base: RefCountedVtbl,
    usage: Get<u8>,
    graphics_clock: Get<u8>,
    memory_clock: Get<u8>,
    temperature: Get<u8>,
    _hotspot_and_gpu_power: [Slot; 2],
    board_power: Get<u8>,
    _fan_speed: Slot,
    vram: Get<u8>,
}

#[repr(C)]
struct SharedMemorySupportVtbl {
    _base_and_previous_extensions: [Slot; 31],
    shared_memory: Get<u8>,
}

#[repr(C)]
struct MetricsVtbl {
    _base: RefCountedVtbl,
    timestamp: Get<i64>,
    usage: Get<f64>,
    graphics_clock: Get<i32>,
    memory_clock: Get<i32>,
    temperature: Get<f64>,
    _hotspot_and_gpu_power: [Slot; 2],
    board_power: Get<f64>,
    _fan_speed: Slot,
    vram: Get<i32>,
}

#[repr(C)]
struct SharedMemoryVtbl {
    _base_and_previous_extensions: [Slot; 17],
    shared_memory: Get<i32>,
}

struct Library(HMODULE);

impl Library {
    fn open() -> Result<Self, String> {
        // The installed display driver supplies the DLL. Do not search the working directory.
        let module = unsafe {
            LoadLibraryExW(
                w!("amdadlx64.dll"),
                null_mut(),
                LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        };
        if module.is_null() {
            Err(format!(
                "ADLX do driver AMD indisponível ({})",
                std::io::Error::last_os_error()
            ))
        } else {
            Ok(Self(module))
        }
    }

    fn symbol(&self, name: &CStr) -> Option<ProcAddress> {
        unsafe { GetProcAddress(self.0, name.as_ptr().cast()) }
    }

    fn required(&self, name: &CStr) -> Result<ProcAddress, String> {
        self.symbol(name)
            .ok_or_else(|| format!("ADLX sem a função {}", name.to_string_lossy()))
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        unsafe { FreeLibrary(self.0) };
    }
}

struct Session {
    _library: Library,
    terminate: Terminate,
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe { (self.terminate)() };
    }
}

fn status(result: i32, operation: &str) -> Result<(), String> {
    if result == OK {
        Ok(())
    } else {
        Err(format!("ADLX: {operation} retornou código {result}"))
    }
}

// SAFETY: the caller knows the exact C interface and that the runtime is live.
unsafe fn vtable<'a, T>(interface: NonNull<c_void>) -> &'a T {
    unsafe { &**interface.cast::<*const T>().as_ptr() }
}

struct Interface<T> {
    ptr: NonNull<c_void>,
    _table: PhantomData<T>,
}

impl<T> Interface<T> {
    fn get(operation: &str, call: impl FnOnce(*mut Ptr) -> i32) -> Result<Self, String> {
        let mut ptr = null_mut();
        status(call(&mut ptr), operation)?;
        Self::from_raw(ptr, operation)
    }

    fn from_raw(ptr: Ptr, operation: &str) -> Result<Self, String> {
        NonNull::new(ptr)
            .map(|ptr| Self {
                ptr,
                _table: PhantomData,
            })
            .ok_or_else(|| format!("ADLX: {operation} não retornou uma interface"))
    }

    fn raw(&self) -> Ptr {
        self.ptr.as_ptr()
    }

    fn methods(&self) -> &T {
        // Each Interface owns one reference and is created with the matching vtable type.
        unsafe { vtable(self.ptr) }
    }

    fn query<U>(&self, id: *const u16) -> Option<Interface<U>> {
        let base: &RefCountedVtbl = unsafe { vtable(self.ptr) };
        Interface::get("QueryInterface", |out| unsafe {
            (base.query)(self.raw(), id, out)
        })
        .ok()
    }

    fn value<V: Default>(&self, get: Get<V>) -> Option<V> {
        let mut value = V::default();
        (unsafe { get(self.raw(), &mut value) } == OK).then_some(value)
    }
}

impl<T> Drop for Interface<T> {
    fn drop(&mut self) {
        // The runtime outlives every retained interface.
        let base: &RefCountedVtbl = unsafe { vtable(self.ptr) };
        unsafe { (base.release)(self.raw()) };
    }
}

impl Interface<ListVtbl> {
    fn size(&self) -> u32 {
        unsafe { (self.methods().size)(self.raw()) }
    }

    fn at<T>(&self, index: u32) -> Result<Interface<T>, String> {
        let begin = unsafe { (self.methods().begin)(self.raw()) };
        let location = begin
            .checked_add(index)
            .ok_or_else(|| "índice ADLX fora dos limites".to_string())?;
        Interface::get("At", |out| unsafe {
            (self.methods().at)(self.raw(), location, out)
        })
    }
}

#[derive(Clone, Copy)]
struct ActiveTracking {
    users: usize,
    interval_ms: i32,
}

struct Monitoring {
    interface: Interface<MonitoringVtbl>,
    active: Cell<Option<ActiveTracking>>,
}

impl Monitoring {
    fn acquire(&self) -> Result<Tracking<'_>, String> {
        let mut active = if let Some(active) = self.active.get() {
            active
        } else {
            let interval_ms = self
                .interface
                .value(self.interface.methods().sampling_interval)
                .filter(|interval| *interval > 0)
                .ok_or_else(|| "ADLX não informou um intervalo de amostragem válido".to_string())?;
            status(
                unsafe { (self.interface.methods().start_tracking)(self.interface.raw()) },
                "StartPerformanceMetricsTracking",
            )?;
            ActiveTracking {
                users: 0,
                interval_ms,
            }
        };
        active.users = active
            .users
            .checked_add(1)
            .ok_or_else(|| "limite de amostradores ADLX excedido".to_string())?;
        self.active.set(Some(active));
        Ok(Tracking {
            monitoring: self,
            interval_ms: active.interval_ms,
        })
    }

    fn stop(&self) -> bool {
        unsafe { (self.interface.methods().stop_tracking)(self.interface.raw()) == OK }
    }
}

impl Drop for Monitoring {
    fn drop(&mut self) {
        if self.active.get().is_some() {
            self.stop();
        }
    }
}

struct Tracking<'a> {
    monitoring: &'a Monitoring,
    interval_ms: i32,
}

impl Drop for Tracking<'_> {
    fn drop(&mut self) {
        let monitoring = self.monitoring;
        let mut active = monitoring
            .active
            .get()
            .expect("an active ADLX tracking lease");
        active.users -= 1;
        // ADLX tracks every GPU in one buffer. Hold one driver claim for all local samplers.
        // A failed stop does not release the claim, so keep it for reuse or final cleanup.
        if active.users == 0 && monitoring.stop() {
            monitoring.active.set(None);
        } else {
            monitoring.active.set(Some(active));
        }
    }
}

pub(crate) struct Adlx {
    gpus: Interface<ListVtbl>,
    monitoring: Monitoring,
    mapping: Option<NonNull<c_void>>,
    // Drop the reference-counted interfaces before terminating/unloading ADLX.
    _session: Session,
}

impl Adlx {
    pub(crate) fn load() -> Result<Self, String> {
        let library = Library::open()?;
        // Signatures match the C entry points in ADLX.h.
        let full_version = unsafe {
            mem::transmute::<ProcAddress, QueryFullVersion>(
                library.required(c"ADLXQueryFullVersion")?,
            )
        };
        let terminate = unsafe {
            mem::transmute::<ProcAddress, Terminate>(library.required(c"ADLXTerminate")?)
        };
        let mut version = 0;
        status(
            unsafe { full_version(&mut version) },
            "ADLXQueryFullVersion",
        )?;

        let mut system = null_mut();
        let mut mapping = null_mut();
        let result = if let Some(address) = library.symbol(c"ADLXInitialize2") {
            let initialize = unsafe { mem::transmute::<ProcAddress, Initialize2>(address) };
            unsafe { initialize(version, &mut system, &mut mapping) }
        } else {
            let initialize = unsafe {
                mem::transmute::<ProcAddress, Initialize>(library.required(c"ADLXInitialize")?)
            };
            unsafe { initialize(version, &mut system) }
        };
        // A second caller is not the owner of a previously initialized runtime.
        status(result, "ADLXInitialize")?;
        let session = Session {
            _library: library,
            terminate,
        };
        let ptr = NonNull::new(system)
            .ok_or_else(|| "ADLX não retornou a interface de sistema".to_string())?;
        let methods: &SystemVtbl = unsafe { vtable(ptr) };
        let gpus = Interface::get("GetGPUs", |out| unsafe { (methods.gpus)(system, out) })?;
        let monitoring = Interface::get("GetPerformanceMonitoringServices", |out| unsafe {
            (methods.performance_monitoring)(system, out)
        })?;
        Ok(Self {
            gpus,
            monitoring: Monitoring {
                interface: monitoring,
                active: Cell::new(None),
            },
            mapping: NonNull::new(mapping),
            _session: session,
        })
    }

    pub(crate) fn device_count(&self) -> u32 {
        self.gpus.size()
    }

    pub(crate) fn device(&self, index: u32) -> Result<Device<'_>, String> {
        if index >= self.device_count() {
            return Err(format!("índice ADLX {index} fora da lista de GPUs Radeon"));
        }
        let gpu: Interface<GpuVtbl> = self.gpus.at(index)?;
        let name = gpu
            .text(gpu.methods().name)
            .ok_or_else(|| "ADLX não retornou o nome da GPU".to_string())?;
        let persistent_id = self.persistent_id(&gpu);
        let total_vram_mib = gpu
            .value(gpu.methods().total_vram)
            .filter(|total| *total > 0)
            .map(f64::from);
        Ok(Device {
            gpu,
            gpus: &self.gpus,
            index,
            monitoring: &self.monitoring,
            name,
            persistent_id,
            total_vram_mib,
        })
    }

    fn persistent_id(&self, gpu: &Interface<GpuVtbl>) -> Option<String> {
        // The Windows instance suffix distinguishes two adapters of the same model.
        if let Some(pnp) = gpu.text(gpu.methods().pnp_string) {
            let pnp = pnp.to_ascii_uppercase();
            let mut parts = pnp.split('\\');
            if parts.next() == Some("PCI")
                && parts
                    .next()
                    .is_some_and(|part| part.starts_with("VEN_1002&DEV_"))
                && parts.next().is_some_and(|part| !part.is_empty())
                && parts.next().is_none()
            {
                return Some(format!("pnp:{pnp}"));
            }
        }

        let mapping = self.mapping?;
        let methods: &MappingVtbl = unsafe { vtable(mapping) };
        let (mut bus, mut device, mut function) = (-1, -1, -1);
        let result = unsafe {
            (methods.bdf_from_gpu)(
                mapping.as_ptr(),
                gpu.raw(),
                &mut bus,
                &mut device,
                &mut function,
            )
        };
        (result == OK
            && (0..=255).contains(&bus)
            && (0..=31).contains(&device)
            && (0..=7).contains(&function))
        .then(|| format!("pci:{bus:02x}:{device:02x}.{function}"))
    }
}

impl Interface<GpuVtbl> {
    fn adapter_luid(&self) -> Option<LUID> {
        let gpu2: Interface<Gpu2Vtbl> = self.query(w!("IADLXGPU2"))?;
        gpu2.value(gpu2.methods().luid)
            .filter(|luid| luid.LowPart != 0 || luid.HighPart != 0)
    }

    fn text(&self, get: Get<*const c_char>) -> Option<String> {
        let mut value = null();
        if unsafe { get(self.raw(), &mut value) } != OK || value.is_null() {
            return None;
        }
        // ADLX returns a NUL-terminated string owned by the GPU interface.
        let text = unsafe { CStr::from_ptr(value) }.to_string_lossy();
        let text = text.trim();
        (!text.is_empty()).then(|| text.to_owned())
    }
}

struct Capabilities {
    temperature: bool,
    usage: bool,
    graphics_clock: bool,
    memory_clock: bool,
    board_power: bool,
    vram: bool,
    shared_memory: bool,
}

impl Capabilities {
    fn read(support: &Interface<SupportVtbl>) -> Self {
        let methods = support.methods();
        let available = |get| support.value(get) == Some(1);
        let shared_memory = support
            .query::<SharedMemorySupportVtbl>(w!("IADLXGPUMetricsSupport2"))
            .is_some_and(|extension| extension.value(extension.methods().shared_memory) == Some(1));
        Self {
            temperature: available(methods.temperature),
            usage: available(methods.usage),
            graphics_clock: available(methods.graphics_clock),
            memory_clock: available(methods.memory_clock),
            board_power: available(methods.board_power),
            vram: available(methods.vram),
            shared_memory,
        }
    }

    fn sample(&self, metrics: &Interface<MetricsVtbl>) -> Sample {
        let methods = metrics.methods();
        let double = |supported: bool, get, min, max| {
            supported
                .then(|| metrics.value(get))
                .flatten()
                .filter(|value| (min..=max).contains(value))
        };
        let integer = |supported: bool, get| {
            supported
                .then(|| metrics.value(get))
                .flatten()
                .filter(|value| *value >= 0)
                .map(f64::from)
        };
        let shared_memory_mib = if self.shared_memory {
            metrics
                .query::<SharedMemoryVtbl>(w!("IADLXGPUMetrics2"))
                .and_then(|extension| extension.value(extension.methods().shared_memory))
                .filter(|value| *value >= 0)
                .map(f64::from)
        } else {
            None
        };
        Sample {
            temperature: double(self.temperature, methods.temperature, -100.0, 200.0),
            usage_percent: double(self.usage, methods.usage, 0.0, 100.0),
            graphics_clock_mhz: integer(self.graphics_clock, methods.graphics_clock),
            memory_clock_mhz: integer(self.memory_clock, methods.memory_clock),
            // Total board power includes VRAM. ADLX GPUPower has a different scope.
            board_power_watts: double(self.board_power, methods.board_power, 0.0, 10_000.0),
            vram_used_mib: integer(self.vram, methods.vram),
            shared_memory_mib,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Sample {
    pub(crate) temperature: Option<f64>,
    pub(crate) usage_percent: Option<f64>,
    pub(crate) graphics_clock_mhz: Option<f64>,
    pub(crate) memory_clock_mhz: Option<f64>,
    pub(crate) board_power_watts: Option<f64>,
    pub(crate) vram_used_mib: Option<f64>,
    pub(crate) shared_memory_mib: Option<f64>,
}

pub(crate) struct Device<'a> {
    gpu: Interface<GpuVtbl>,
    gpus: &'a Interface<ListVtbl>,
    index: u32,
    monitoring: &'a Monitoring,
    name: String,
    persistent_id: Option<String>,
    total_vram_mib: Option<f64>,
}

impl<'a> Device<'a> {
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn persistent_id(&self) -> Option<&str> {
        self.persistent_id.as_deref()
    }

    pub(crate) fn adapter_luid(&self) -> Option<LUID> {
        let luid = self.gpu.adapter_luid()?;
        // A logical adapter shared by multiple physical GPUs cannot identify this GPU alone.
        for index in 0..self.gpus.size() {
            if index != self.index {
                let other: Interface<GpuVtbl> = self.gpus.at(index).ok()?;
                let other_luid = other.adapter_luid()?;
                if (other_luid.LowPart, other_luid.HighPart) == (luid.LowPart, luid.HighPart) {
                    return None;
                }
            }
        }
        Some(luid)
    }

    pub(crate) fn total_vram_mib(&self) -> Option<f64> {
        self.total_vram_mib
    }

    pub(crate) fn current_sample(&self) -> Result<Sample, String> {
        let supported = self.capabilities()?;
        let monitoring = &self.monitoring.interface;
        let metrics = Interface::get("GetCurrentGPUMetrics", |out| unsafe {
            (monitoring.methods().current_gpu_metrics)(monitoring.raw(), self.gpu.raw(), out)
        })?;
        Ok(supported.sample(&metrics))
    }

    pub(crate) fn start(self) -> Result<Sampler<'a>, String> {
        let supported = self.capabilities()?;
        let tracking = self.monitoring.acquire()?;
        Ok(Sampler {
            tracking,
            device: self,
            supported,
            last_timestamp: None,
        })
    }

    fn capabilities(&self) -> Result<Capabilities, String> {
        let monitoring = &self.monitoring.interface;
        let support = Interface::get("GetSupportedGPUMetrics", |out| unsafe {
            (monitoring.methods().supported_gpu_metrics)(monitoring.raw(), self.gpu.raw(), out)
        })?;
        Ok(Capabilities::read(&support))
    }
}

pub(crate) struct Sampler<'a> {
    tracking: Tracking<'a>,
    device: Device<'a>,
    supported: Capabilities,
    last_timestamp: Option<i64>,
}

impl Sampler<'_> {
    pub(crate) fn interval(&self) -> Duration {
        Duration::from_millis(self.tracking.interval_ms as u64)
    }

    pub(crate) fn total_vram_mib(&self) -> Option<f64> {
        self.device.total_vram_mib()
    }

    pub(crate) fn read(&mut self) -> Result<Option<Sample>, String> {
        let monitoring = &self.tracking.monitoring.interface;
        // A small lookback accommodates timer jitter without copying the whole driver history.
        let mut history = null_mut();
        let result = unsafe {
            (monitoring.methods().gpu_metrics_history)(
                monitoring.raw(),
                self.device.gpu.raw(),
                self.tracking.interval_ms.saturating_mul(2),
                0,
                &mut history,
            )
        };
        if result == PENDING_OPERATION {
            return Ok(None);
        }
        status(result, "GetGPUMetricsHistory")?;
        let history: Interface<ListVtbl> = Interface::from_raw(history, "GetGPUMetricsHistory")?;
        let mut newest = None;
        let mut latest = self.last_timestamp;
        for index in 0..history.size() {
            let sample: Interface<MetricsVtbl> = history.at(index)?;
            let timestamp = sample
                .value(sample.methods().timestamp)
                .ok_or_else(|| "ADLX não informou o horário da amostra".to_string())?;
            if latest.is_none_or(|previous| timestamp > previous) {
                newest = Some(sample);
                latest = Some(timestamp);
            }
        }
        self.last_timestamp = latest;
        Ok(newest.map(|sample| self.supported.sample(&sample)))
    }
}
