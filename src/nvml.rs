use std::{
    ffi::{CStr, OsStr, c_char, c_void},
    fmt, io, mem,
    os::windows::ffi::OsStrExt,
    path::PathBuf,
    ptr, thread,
    time::{Duration, Instant},
};

// NVIDIA NVML C declarations: https://github.com/NVIDIA/go-nvml/blob/main/pkg/nvml/nvml.h
type Status = i32;
const SUCCESS: Status = 0;
const NOT_SUPPORTED: Status = 3;
const FUNCTION_NOT_FOUND: Status = 13;
const NO_DATA: Status = 21;
const CLOCK_GRAPHICS: u32 = 0;
const CLOCK_MEMORY: u32 = 2;
const TEMPERATURE_GPU: u32 = 0;
const GPM_VERSION: u32 = 1;
const GPM_METRIC_MAX: usize = 477;
const GPM_MIN_EXCLUSIVE: Duration = Duration::from_millis(100);

#[repr(C)]
#[derive(Clone, Copy)]
struct DeviceHandle(*mut c_void);

#[repr(C)]
#[derive(Clone, Copy)]
struct SampleHandle(*mut c_void);

type Init = unsafe extern "C" fn() -> Status;
type SystemString = unsafe extern "C" fn(*mut c_char, u32) -> Status;
type DeviceCount = unsafe extern "C" fn(*mut u32) -> Status;
type DeviceByIndex = unsafe extern "C" fn(u32, *mut DeviceHandle) -> Status;
type DeviceString = unsafe extern "C" fn(DeviceHandle, *mut c_char, u32) -> Status;
type DeviceValue<T> = unsafe extern "C" fn(DeviceHandle, *mut T) -> Status;
type DeviceSelectorValue = unsafe extern "C" fn(DeviceHandle, u32, *mut u32) -> Status;
type DevicePair = unsafe extern "C" fn(DeviceHandle, *mut u32, *mut u32) -> Status;
type SampleAlloc = unsafe extern "C" fn(*mut SampleHandle) -> Status;
type SampleFree = unsafe extern "C" fn(SampleHandle) -> Status;
type SampleGet = unsafe extern "C" fn(DeviceHandle, SampleHandle) -> Status;
type MetricsGet = unsafe extern "C" fn(*mut MetricsRequest) -> Status;

#[repr(C)]
pub struct Memory {
    pub total: u64,
    pub free: u64,
    pub used: u64,
}

#[repr(C)]
pub struct Utilization {
    pub gpu: u32,
    pub memory: u32,
}

pub struct MediaUtilization {
    pub percent: u32,
    pub sampling_period_us: u32,
}

#[repr(C)]
struct GpmSupport {
    version: u32,
    supported: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct MetricOutput {
    id: u32,
    status: Status,
    value: f64,
    short_name: *mut c_char,
    long_name: *mut c_char,
    unit: *mut c_char,
}

const EMPTY_METRIC: MetricOutput = MetricOutput {
    id: 0,
    status: NO_DATA,
    value: f64::NAN,
    short_name: ptr::null_mut(),
    long_name: ptr::null_mut(),
    unit: ptr::null_mut(),
};

#[repr(C)]
struct MetricsRequest {
    version: u32,
    count: u32,
    sample1: SampleHandle,
    sample2: SampleHandle,
    metrics: [MetricOutput; GPM_METRIC_MAX],
}

#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(mem::size_of::<MetricOutput>() == 40);
    assert!(mem::offset_of!(MetricsRequest, metrics) == 24);
    assert!(mem::size_of::<MetricsRequest>() == 19_104);
};

pub struct GpmMetric {
    pub id: u32,
    pub name: &'static str,
    pub(crate) key: &'static str,
}

pub(crate) static GPM_METRICS: [GpmMetric; 5] = [
    GpmMetric {
        id: 12,
        name: "Atividade FP32",
        key: "gpm-fp32",
    },
    GpmMetric {
        id: 5,
        name: "Atividade Tensor",
        key: "gpm-tensor",
    },
    GpmMetric {
        id: 2,
        name: "SMs ativos",
        key: "gpm-sm-active",
    },
    GpmMetric {
        id: 3,
        name: "Ocupação de warps nos SMs",
        key: "gpm-sm-occupancy",
    },
    GpmMetric {
        id: 10,
        name: "Uso da banda da VRAM",
        key: "gpm-dram-bandwidth",
    },
];

#[derive(Debug)]
pub enum Error {
    Load {
        system32: io::Error,
        legacy: Option<io::Error>,
    },
    MissingSymbol(&'static CStr),
    Nvml {
        function: &'static CStr,
        status: Status,
    },
    InvalidData(&'static str),
    InvalidInterval,
}

impl Error {
    pub(crate) fn is_unsupported(&self) -> bool {
        matches!(
            self,
            Self::MissingSymbol(_)
                | Self::Nvml {
                    status: NOT_SUPPORTED | FUNCTION_NOT_FOUND,
                    ..
                }
        )
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Load { system32, legacy } => {
                write!(f, "falha ao carregar nvml.dll em System32 ({system32})")?;
                if let Some(legacy) = legacy {
                    write!(f, " e no diretório NVSMI ({legacy})")?;
                }
                Ok(())
            }
            Self::MissingSymbol(name) => {
                write!(
                    f,
                    "função {} ausente na NVML instalada",
                    name.to_string_lossy()
                )
            }
            Self::Nvml { function, status } => {
                let reason = match *status {
                    1 => "NVML não inicializada",
                    2 => "argumento inválido",
                    NOT_SUPPORTED => "recurso não suportado",
                    4 => "permissão insuficiente",
                    9 => "driver NVIDIA não carregado",
                    FUNCTION_NOT_FOUND => "função indisponível",
                    15 => "GPU inacessível",
                    17 => "acesso bloqueado pelo sistema operacional",
                    18 => "versões incompatíveis do driver e da biblioteca",
                    21 => "sem dados",
                    25 => "versão de estrutura incompatível",
                    27 => "sistema ainda não preparado",
                    28 => "nenhuma GPU encontrada",
                    _ => "falha na NVML",
                };
                write!(
                    f,
                    "{reason} ({} retornou {status})",
                    function.to_string_lossy()
                )
            }
            Self::InvalidData(reason) => f.write_str(reason),
            Self::InvalidInterval => {
                f.write_str("a amostragem GPM exige intervalo superior a 100 ms")
            }
        }
    }
}

type Module = *mut c_void;
type ProcAddress = unsafe extern "system" fn() -> isize;
const LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR: u32 = 0x0000_0100;
const LOAD_LIBRARY_SEARCH_SYSTEM32: u32 = 0x0000_0800;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn LoadLibraryExW(name: *const u16, file: *mut c_void, flags: u32) -> Module;
    fn GetProcAddress(module: Module, name: *const c_char) -> Option<ProcAddress>;
    fn FreeLibrary(module: Module) -> i32;
}

struct Library(Module);

impl Library {
    fn open() -> Result<Self, Error> {
        let system32 = match Self::load(OsStr::new("nvml.dll"), LOAD_LIBRARY_SEARCH_SYSTEM32) {
            Ok(library) => return Ok(library),
            Err(error) => error,
        };

        let legacy = std::env::var_os("ProgramW6432")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .map(|path| path.join(r"NVIDIA Corporation\NVSMI\nvml.dll"));
        let legacy = if let Some(path) = legacy {
            match Self::load(
                path.as_os_str(),
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
            ) {
                Ok(library) => return Ok(library),
                Err(error) => Some(error),
            }
        } else {
            None
        };
        Err(Error::Load { system32, legacy })
    }

    fn load(name: &OsStr, flags: u32) -> io::Result<Self> {
        let wide_name: Vec<_> = name.encode_wide().chain(std::iter::once(0)).collect();
        // The UTF-16 name is NUL-terminated and stays alive through the call.
        let module = unsafe { LoadLibraryExW(wide_name.as_ptr(), ptr::null_mut(), flags) };
        if module.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(module))
        }
    }

    // The caller pairs every export name with its NVML C function-pointer type.
    unsafe fn symbol<T: Copy>(&self, name: &'static CStr) -> Symbol<T> {
        assert_eq!(mem::size_of::<T>(), mem::size_of::<ProcAddress>());
        // The module is loaded, and name is a NUL-terminated C string.
        let address = unsafe { GetProcAddress(self.0, name.as_ptr()) };
        Symbol {
            // The signature is determined by the NVML export.
            function: address.map(|address| unsafe { mem::transmute_copy(&address) }),
            name,
        }
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        // This module handle owns one successful LoadLibraryExW reference.
        unsafe { FreeLibrary(self.0) };
    }
}

#[derive(Clone, Copy)]
struct Symbol<T> {
    function: Option<T>,
    name: &'static CStr,
}

impl<T: Copy> Symbol<T> {
    fn get(self) -> Result<T, Error> {
        self.function.ok_or(Error::MissingSymbol(self.name))
    }

    fn check(self, status: Status) -> Result<(), Error> {
        if status == SUCCESS {
            Ok(())
        } else {
            Err(Error::Nvml {
                function: self.name,
                status,
            })
        }
    }
}

pub struct Nvml {
    _library: Library,
    shutdown: Init,
    count: Symbol<DeviceCount>,
    by_index: Symbol<DeviceByIndex>,
    driver_version: Symbol<SystemString>,
    library_version: Symbol<SystemString>,
    name: Symbol<DeviceString>,
    uuid: Symbol<DeviceString>,
    driver_model: Symbol<DevicePair>,
    memory: Symbol<DeviceValue<Memory>>,
    utilization: Symbol<DeviceValue<Utilization>>,
    temperature: Symbol<DeviceSelectorValue>,
    power: Symbol<DeviceValue<u32>>,
    power_constraints: Symbol<DevicePair>,
    clock: Symbol<DeviceSelectorValue>,
    max_clock: Symbol<DeviceSelectorValue>,
    encoder: Symbol<DevicePair>,
    decoder: Symbol<DevicePair>,
    gpm_support: Symbol<DeviceValue<GpmSupport>>,
    gpm_alloc: Symbol<SampleAlloc>,
    gpm_free: Symbol<SampleFree>,
    gpm_sample: Symbol<SampleGet>,
    gpm_metrics: Symbol<MetricsGet>,
}

impl Nvml {
    pub fn load() -> Result<Self, Error> {
        let library = Library::open()?;
        // Every symbol name below is paired with its declaration in nvml.h.
        unsafe {
            let init: Symbol<Init> = library.symbol(c"nvmlInit_v2");
            let shutdown: Init = library.symbol(c"nvmlShutdown").get()?;
            let count: Symbol<DeviceCount> = library.symbol(c"nvmlDeviceGetCount_v2");
            let by_index: Symbol<DeviceByIndex> = library.symbol(c"nvmlDeviceGetHandleByIndex_v2");
            count.get()?;
            by_index.get()?;
            init.check(init.get()?())?;

            let mut driver_model: Symbol<DevicePair> =
                library.symbol(c"nvmlDeviceGetDriverModel_v2");
            if driver_model.function.is_none() {
                driver_model = library.symbol(c"nvmlDeviceGetDriverModel");
            }

            Ok(Self {
                shutdown,
                count,
                by_index,
                driver_version: library.symbol(c"nvmlSystemGetDriverVersion"),
                library_version: library.symbol(c"nvmlSystemGetNVMLVersion"),
                name: library.symbol(c"nvmlDeviceGetName"),
                uuid: library.symbol(c"nvmlDeviceGetUUID"),
                driver_model,
                memory: library.symbol(c"nvmlDeviceGetMemoryInfo"),
                utilization: library.symbol(c"nvmlDeviceGetUtilizationRates"),
                temperature: library.symbol(c"nvmlDeviceGetTemperature"),
                power: library.symbol(c"nvmlDeviceGetPowerUsage"),
                power_constraints: library.symbol(c"nvmlDeviceGetPowerManagementLimitConstraints"),
                clock: library.symbol(c"nvmlDeviceGetClockInfo"),
                max_clock: library.symbol(c"nvmlDeviceGetMaxClockInfo"),
                encoder: library.symbol(c"nvmlDeviceGetEncoderUtilization"),
                decoder: library.symbol(c"nvmlDeviceGetDecoderUtilization"),
                gpm_support: library.symbol(c"nvmlGpmQueryDeviceSupport"),
                gpm_alloc: library.symbol(c"nvmlGpmSampleAlloc"),
                gpm_free: library.symbol(c"nvmlGpmSampleFree"),
                gpm_sample: library.symbol(c"nvmlGpmSampleGet"),
                gpm_metrics: library.symbol(c"nvmlGpmMetricsGet"),
                _library: library,
            })
        }
    }

    pub fn driver_version(&self) -> Result<String, Error> {
        Self::system_string(self.driver_version)
    }

    pub fn library_version(&self) -> Result<String, Error> {
        Self::system_string(self.library_version)
    }

    fn system_string(query: Symbol<SystemString>) -> Result<String, Error> {
        let function = query.get()?;
        read_string(|buffer, length| {
            // buffer points to length bytes of writable storage.
            query.check(unsafe { function(buffer, length) })
        })
    }

    pub fn device_count(&self) -> Result<u32, Error> {
        let mut count = 0;
        // NVML is initialized and count is valid writable storage.
        self.count.check(unsafe { self.count.get()?(&mut count) })?;
        Ok(count)
    }

    pub fn device(&self, index: u32) -> Result<Device<'_>, Error> {
        let mut handle = DeviceHandle(ptr::null_mut());
        // NVML validates the index and writes its opaque handle to handle.
        self.by_index
            .check(unsafe { self.by_index.get()?(index, &mut handle) })?;
        if handle.0.is_null() {
            return Err(Error::InvalidData(
                "a NVML devolveu um identificador de GPU nulo",
            ));
        }
        Ok(Device { nvml: self, handle })
    }
}

impl Drop for Nvml {
    fn drop(&mut self) {
        // The module stays loaded and this object owns one NVML initialization reference.
        unsafe { (self.shutdown)() };
    }
}

fn read_string(query: impl FnOnce(*mut c_char, u32) -> Result<(), Error>) -> Result<String, Error> {
    let mut buffer = [0; 128];
    query(buffer.as_mut_ptr().cast(), buffer.len() as u32)?;
    let text = CStr::from_bytes_until_nul(&buffer)
        .map_err(|_| Error::InvalidData("a NVML devolveu texto sem terminador"))?;
    if text.is_empty() {
        return Err(Error::InvalidData("a NVML devolveu texto vazio"));
    }
    Ok(text.to_string_lossy().into_owned())
}

pub struct Device<'a> {
    nvml: &'a Nvml,
    handle: DeviceHandle,
}

impl<'a> Device<'a> {
    pub fn name(&self) -> Result<String, Error> {
        self.device_string(self.nvml.name)
    }

    pub fn uuid(&self) -> Option<String> {
        self.device_string(self.nvml.uuid).ok()
    }

    fn device_string(&self, query: Symbol<DeviceString>) -> Result<String, Error> {
        let function = query.get()?;
        read_string(|buffer, length| {
            // The handle belongs to a live session, and the output holds length bytes.
            query.check(unsafe { function(self.handle, buffer, length) })
        })
    }

    pub fn driver_model(&self) -> Result<&'static str, Error> {
        let query = self.nvml.driver_model;
        let mut current = u32::MAX;
        // The handle belongs to a live session. NVML allows a NULL pending-model output.
        query.check(unsafe { query.get()?(self.handle, &mut current, ptr::null_mut()) })?;
        match current {
            0 => Ok("WDDM"),
            1 => Ok("TCC"),
            2 => Ok("MCDM"),
            _ => Err(Error::InvalidData("modelo do driver desconhecido")),
        }
    }

    pub fn memory(&self) -> Result<Memory, Error> {
        let value = self.value(
            self.nvml.memory,
            Memory {
                total: u64::MAX,
                free: u64::MAX,
                used: u64::MAX,
            },
        )?;
        if [value.total, value.free, value.used].contains(&u64::MAX) {
            return Err(Error::InvalidData("valor de VRAM indisponível na NVML"));
        }
        Ok(value)
    }

    pub fn utilization(&self) -> Result<Utilization, Error> {
        let value = self.value(
            self.nvml.utilization,
            Utilization {
                gpu: u32::MAX,
                memory: u32::MAX,
            },
        )?;
        if value.gpu > 100 || value.memory > 100 {
            return Err(Error::InvalidData(
                "utilização NVML fora do intervalo de 0 a 100%",
            ));
        }
        Ok(value)
    }

    pub fn temperature(&self) -> Result<u32, Error> {
        self.selector_value(self.nvml.temperature, TEMPERATURE_GPU)
    }

    pub fn power_milliwatts(&self) -> Result<u32, Error> {
        let value = self.value(self.nvml.power, u32::MAX)?;
        Self::available(value)
    }

    pub(crate) fn max_power_milliwatts(&self) -> Result<u32, Error> {
        let query = self.nvml.power_constraints;
        let (mut min, mut max) = (u32::MAX, u32::MAX);
        // The handle belongs to a live session and both outputs point to live u32 values.
        query.check(unsafe { query.get()?(self.handle, &mut min, &mut max) })?;
        if min > max {
            return Err(Error::InvalidData(
                "limites de potência inconsistentes na NVML",
            ));
        }
        Self::available(max)
    }

    pub fn graphics_clock_mhz(&self) -> Result<u32, Error> {
        self.selector_value(self.nvml.clock, CLOCK_GRAPHICS)
    }

    pub fn memory_clock_mhz(&self) -> Result<u32, Error> {
        self.selector_value(self.nvml.clock, CLOCK_MEMORY)
    }

    pub(crate) fn max_graphics_clock_mhz(&self) -> Result<u32, Error> {
        self.selector_value(self.nvml.max_clock, CLOCK_GRAPHICS)
    }

    pub(crate) fn max_memory_clock_mhz(&self) -> Result<u32, Error> {
        self.selector_value(self.nvml.max_clock, CLOCK_MEMORY)
    }

    pub fn encoder_utilization(&self) -> Result<MediaUtilization, Error> {
        self.media_utilization(self.nvml.encoder)
    }

    pub fn decoder_utilization(&self) -> Result<MediaUtilization, Error> {
        self.media_utilization(self.nvml.decoder)
    }

    fn value<T>(&self, query: Symbol<DeviceValue<T>>, mut value: T) -> Result<T, Error> {
        // The handle belongs to a live session and value matches the query's C output type.
        query.check(unsafe { query.get()?(self.handle, &mut value) })?;
        Ok(value)
    }

    fn selector_value(
        &self,
        query: Symbol<DeviceSelectorValue>,
        selector: u32,
    ) -> Result<u32, Error> {
        let mut value = u32::MAX;
        // The handle belongs to a live session. NVML validates the selector.
        query.check(unsafe { query.get()?(self.handle, selector, &mut value) })?;
        Self::available(value)
    }

    fn available(value: u32) -> Result<u32, Error> {
        if value == u32::MAX {
            Err(Error::InvalidData("valor indisponível na NVML"))
        } else {
            Ok(value)
        }
    }

    fn media_utilization(&self, query: Symbol<DevicePair>) -> Result<MediaUtilization, Error> {
        let mut percent = u32::MAX;
        let mut sampling_period_us = 0;
        // Both outputs point to live u32 values and the handle belongs to a live session.
        query.check(unsafe { query.get()?(self.handle, &mut percent, &mut sampling_period_us) })?;
        if percent > 100 {
            return Err(Error::InvalidData(
                "utilização NVML fora do intervalo de 0 a 100%",
            ));
        }
        Ok(MediaUtilization {
            percent,
            sampling_period_us,
        })
    }

    pub fn gpm_supported(&self) -> Result<bool, Error> {
        let output = self.value(
            self.nvml.gpm_support,
            GpmSupport {
                version: GPM_VERSION,
                supported: u32::MAX,
            },
        )?;
        match output.supported {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Error::InvalidData(
                "a NVML devolveu uma indicação de suporte GPM inválida",
            )),
        }
    }

    pub fn gpm_sampler(&self) -> Result<GpmSampler<'a>, Error> {
        self.nvml.gpm_sample.get()?;
        self.nvml.gpm_metrics.get()?;
        let free = self.nvml.gpm_free.get()?;
        let first = Sample::alloc(self.nvml.gpm_alloc, free)?;
        let second = Sample::alloc(self.nvml.gpm_alloc, free)?;
        Ok(GpmSampler {
            nvml: self.nvml,
            device: self.handle,
            first,
            second,
            baseline: None,
        })
    }
}

struct Sample {
    handle: SampleHandle,
    free: SampleFree,
}

impl Sample {
    fn alloc(alloc: Symbol<SampleAlloc>, free: SampleFree) -> Result<Self, Error> {
        let mut handle = SampleHandle(ptr::null_mut());
        // The session is live and handle can store the allocated sample.
        alloc.check(unsafe { alloc.get()?(&mut handle) })?;
        if handle.0.is_null() {
            return Err(Error::InvalidData("a NVML devolveu uma amostra GPM nula"));
        }
        Ok(Self { handle, free })
    }
}

impl Drop for Sample {
    fn drop(&mut self) {
        // The live sample has exactly one owner.
        unsafe { (self.free)(self.handle) };
    }
}

pub struct GpmSampler<'a> {
    nvml: &'a Nvml,
    device: DeviceHandle,
    first: Sample,
    second: Sample,
    baseline: Option<Instant>,
}

pub struct GpmReading {
    pub elapsed: Duration,
    pub metrics: [(&'static GpmMetric, Result<f64, Error>); GPM_METRICS.len()],
}

impl GpmSampler<'_> {
    pub fn read(&mut self, interval: Duration) -> Result<GpmReading, Error> {
        Self::validate_interval(interval)?;
        let start = match self.baseline {
            Some(start) => start,
            None => self.capture(self.first.handle)?,
        };
        thread::sleep(interval.saturating_sub(start.elapsed()));
        self.finish_read(start)
    }

    /// Starts or completes a GPM window without sleeping for the next sample.
    /// `None` indicates a new baseline or a window that is still in progress.
    pub(crate) fn try_read(&mut self, interval: Duration) -> Result<Option<GpmReading>, Error> {
        Self::validate_interval(interval)?;
        match self.baseline {
            Some(start) if start.elapsed() >= interval => self.finish_read(start).map(Some),
            Some(_) => Ok(None),
            None => {
                self.baseline = Some(self.capture(self.first.handle)?);
                Ok(None)
            }
        }
    }

    fn validate_interval(interval: Duration) -> Result<(), Error> {
        if interval <= GPM_MIN_EXCLUSIVE {
            Err(Error::InvalidInterval)
        } else {
            Ok(())
        }
    }

    fn capture(&self, sample: SampleHandle) -> Result<Instant, Error> {
        let query = self.nvml.gpm_sample;
        // The device and sample buffer belong to the live NVML session.
        query.check(unsafe { query.get()?(self.device, sample) })?;
        Ok(Instant::now())
    }

    fn finish_read(&mut self, start: Instant) -> Result<GpmReading, Error> {
        self.baseline = None;
        let end = self.capture(self.second.handle)?;

        let mut request = MetricsRequest {
            version: GPM_VERSION,
            count: GPM_METRICS.len() as u32,
            sample1: self.first.handle,
            sample2: self.second.handle,
            metrics: [EMPTY_METRIC; GPM_METRIC_MAX],
        };
        for (output, metric) in request.metrics.iter_mut().zip(&GPM_METRICS) {
            output.id = metric.id;
        }
        let metrics_get = self.nvml.gpm_metrics;
        // The C layout includes the full metrics array, and both samples are live.
        metrics_get.check(unsafe { metrics_get.get()?(&mut request) })?;
        let metrics = std::array::from_fn(|index| {
            let output = &request.metrics[index];
            let result = metrics_get.check(output.status).and_then(|()| {
                if (0.0..=100.0).contains(&output.value) {
                    Ok(output.value)
                } else {
                    Err(Error::InvalidData(
                        "valor GPM inválido para uma porcentagem",
                    ))
                }
            });
            (&GPM_METRICS[index], result)
        });

        mem::swap(&mut self.first, &mut self.second);
        self.baseline = Some(end);
        Ok(GpmReading {
            elapsed: end - start,
            metrics,
        })
    }
}
