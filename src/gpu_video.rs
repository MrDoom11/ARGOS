use std::collections::{HashMap, HashSet};
use std::mem::{align_of, size_of};
use std::ptr::{null, null_mut};
use std::slice;

use windows_sys::Win32::Foundation::LUID;
use windows_sys::Win32::System::Performance::{
    PDH_CSTATUS_NEW_DATA, PDH_CSTATUS_VALID_DATA, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE,
    PDH_HCOUNTER, PDH_HQUERY, PDH_MORE_DATA, PdhAddEnglishCounterW, PdhCloseQuery,
    PdhCollectQueryData, PdhGetFormattedCounterArrayW, PdhOpenQueryW,
};
use windows_sys::core::w;

const MAX_COUNTER_BYTES: usize = 4 * 1024 * 1024;
const MAX_INSTANCE_NAME: usize = 128;
const _: () = assert!(align_of::<u64>() >= align_of::<PDH_FMT_COUNTERVALUE_ITEM_W>());

#[derive(Clone, Copy, Default)]
pub(crate) struct Sample {
    pub(crate) encode_percent: Option<f64>,
    pub(crate) decode_percent: Option<f64>,
    pub(crate) codec_percent: Option<f64>,
}

impl Sample {
    fn percent(&mut self, kind: Engine) -> &mut Option<f64> {
        match kind {
            Engine::Encode => &mut self.encode_percent,
            Engine::Decode => &mut self.decode_percent,
            Engine::Codec => &mut self.codec_percent,
        }
    }
}

#[derive(Clone, Copy, Default)]
pub(crate) struct Support {
    pub(crate) encode: bool,
    pub(crate) decode: bool,
    pub(crate) codec: bool,
}

impl Support {
    fn record(&mut self, kind: Engine) {
        match kind {
            Engine::Encode => self.encode = true,
            Engine::Decode => self.decode = true,
            Engine::Codec => self.codec = true,
        }
    }
}

pub(crate) struct Sampler {
    query: PDH_HQUERY,
    counter: PDH_HCOUNTER,
    luid: LUID,
    support: Support,
    buffer: Vec<u64>,
    engines: Engines,
}

impl Sampler {
    // The caller must resolve the vendor device to an unambiguous single-physical-adapter LUID.
    pub(crate) fn open(luid: LUID) -> Option<Self> {
        let mut query = null_mut();
        if unsafe { PdhOpenQueryW(null(), 0, &mut query) } != 0 || query.is_null() {
            return None;
        }
        let mut sampler = Self {
            query,
            counter: null_mut(),
            luid,
            support: Support::default(),
            buffer: Vec::new(),
            engines: Engines::default(),
        };
        let path = w!(r"\GPU Engine(*)\Utilization Percentage");
        if unsafe { PdhAddEnglishCounterW(query, path, 0, &mut sampler.counter) } != 0
            || sampler.counter.is_null()
        {
            return None;
        }
        // A rate counter needs two collections. A transient failure here can recover in read().
        unsafe { PdhCollectQueryData(query) };
        Some(sampler)
    }

    pub(crate) fn support(&self) -> Support {
        self.support
    }

    pub(crate) fn read(&mut self) -> Sample {
        if unsafe { PdhCollectQueryData(self.query) } != 0 {
            return Sample::default();
        }
        let Some((count, bytes)) = read_items(self.counter, &mut self.buffer) else {
            return Sample::default();
        };
        // A successful read writes count initialized items at the start of the buffer.
        let items = unsafe {
            slice::from_raw_parts(
                self.buffer.as_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>(),
                count,
            )
        };
        self.engines.clear();
        for item in items {
            let Some(instance) = instance_name(&self.buffer, bytes, item.szName)
                .and_then(|name| Instance::parse(name.as_str(), self.luid))
            else {
                continue;
            };
            let percent = matches!(
                item.FmtValue.CStatus,
                PDH_CSTATUS_VALID_DATA | PDH_CSTATUS_NEW_DATA
            )
            // The PDH_FMT_DOUBLE format selects this union member.
            .then(|| unsafe { item.FmtValue.Anonymous.doubleValue })
            .filter(|value| value.is_finite() && *value >= 0.0);
            self.engines.record(instance, percent);
        }
        self.engines.sample(&mut self.support)
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        // Closing the query also releases its counter.
        unsafe { PdhCloseQuery(self.query) };
    }
}

fn read_items(counter: PDH_HCOUNTER, buffer: &mut Vec<u64>) -> Option<(usize, usize)> {
    // The instance list can grow between calls. Retry boundedly, reusing aligned storage.
    for _ in 0..3 {
        if !buffer.is_empty() {
            let capacity = size_of_val(buffer.as_slice());
            let mut bytes = capacity as u32;
            let mut count = 0;
            let status = unsafe {
                PdhGetFormattedCounterArrayW(
                    counter,
                    PDH_FMT_DOUBLE,
                    &mut bytes,
                    &mut count,
                    buffer.as_mut_ptr().cast(),
                )
            };
            if status == 0 {
                let (count, bytes) = (count as usize, bytes as usize);
                return (bytes <= capacity
                    && count <= bytes / size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>())
                .then_some((count, bytes));
            }
            if status != PDH_MORE_DATA {
                return None;
            }
        }

        // PDH reports a reliable required size only when asked with a null buffer.
        let mut bytes = 0;
        let mut count = 0;
        let status = unsafe {
            PdhGetFormattedCounterArrayW(
                counter,
                PDH_FMT_DOUBLE,
                &mut bytes,
                &mut count,
                null_mut(),
            )
        };
        let required = bytes as usize;
        if status != PDH_MORE_DATA || required == 0 || required > MAX_COUNTER_BYTES {
            return None;
        }
        if required > size_of_val(buffer.as_slice()) {
            buffer.resize(required.div_ceil(size_of::<u64>()), 0);
        }
    }
    None
}

struct InstanceName {
    bytes: [u8; MAX_INSTANCE_NAME],
    len: usize,
}

impl InstanceName {
    fn as_str(&self) -> &str {
        // The name was checked to be ASCII while reading it from the counter buffer.
        str::from_utf8(&self.bytes[..self.len]).unwrap_or_default()
    }
}

fn instance_name(buffer: &[u64], bytes: usize, name: *const u16) -> Option<InstanceName> {
    let byte_offset = name.addr().checked_sub(buffer.as_ptr().addr())?;
    let remaining = bytes.checked_sub(byte_offset)?;
    if !byte_offset.is_multiple_of(align_of::<u16>()) {
        return None;
    }
    let units = (remaining / size_of::<u16>()).min(MAX_INSTANCE_NAME);
    // PDH's name pointers refer to NUL-terminated strings stored in the same output buffer.
    let start = unsafe { buffer.as_ptr().cast::<u8>().add(byte_offset).cast::<u16>() };
    let mut bytes = [0; MAX_INSTANCE_NAME];
    for (index, &unit) in unsafe { slice::from_raw_parts(start, units) }
        .iter()
        .enumerate()
    {
        if unit == 0 {
            return Some(InstanceName { bytes, len: index });
        }
        if unit > 127 {
            return None;
        }
        bytes[index] = (unit as u8).to_ascii_lowercase();
    }
    None
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Engine {
    Encode,
    Decode,
    Codec,
}

impl Engine {
    fn parse(label: &str) -> Option<Self> {
        // Friendly names can include an ordinal, as in the Radeon 760M's "Video Codec 0".
        let label = label
            .rsplit_once(' ')
            .filter(|(_, ordinal)| {
                !ordinal.is_empty() && ordinal.bytes().all(|byte| byte.is_ascii_digit())
            })
            .map_or(label, |(name, _)| name);
        match label {
            "videoencode" | "video encode" => Some(Self::Encode),
            "videodecode" | "video decode" => Some(Self::Decode),
            "videocodec" | "video codec" => Some(Self::Codec),
            _ => None,
        }
    }
}

struct Instance {
    process: u32,
    node: u16,
    kind: Engine,
}

impl Instance {
    fn parse(name: &str, luid: LUID) -> Option<Self> {
        let (process, tail) = name.strip_prefix("pid_")?.split_once("_luid_")?;
        let (adapter, tail) = tail.split_once("_phys_")?;
        let (high, low) = adapter.split_once('_')?;
        if u32::from_str_radix(high.strip_prefix("0x")?, 16).ok()? != luid.HighPart as u32
            || u32::from_str_radix(low.strip_prefix("0x")?, 16).ok()? != luid.LowPart
        {
            return None;
        }
        let (physical, tail) = tail.split_once("_eng_")?;
        if physical != "0" {
            return None;
        }
        let (node, kind) = tail.split_once("_engtype_")?;
        Some(Self {
            process: process.parse().ok()?,
            node: node.parse().ok()?,
            kind: Engine::parse(kind)?,
        })
    }
}

#[derive(Default)]
struct Engines {
    nodes: HashMap<u16, Node>,
    instances: HashSet<(u16, u32)>,
}

struct Node {
    kind: Option<Engine>,
    percent: Option<f64>,
}

impl Engines {
    fn clear(&mut self) {
        self.nodes.clear();
        self.instances.clear();
    }

    fn record(&mut self, instance: Instance, percent: Option<f64>) {
        let node = self.nodes.entry(instance.node).or_insert(Node {
            kind: Some(instance.kind),
            percent: Some(0.0),
        });
        if node.kind != Some(instance.kind) {
            node.kind = None;
            return;
        }
        if !self.instances.insert((instance.node, instance.process)) {
            node.percent = None;
            return;
        }
        node.percent = node
            .percent
            .zip(percent)
            .map(|(total, percent)| (total + percent).min(100.0));
    }

    fn sample(&self, support: &mut Support) -> Sample {
        let mut sample = Sample::default();
        let mut incomplete = Support::default();
        for node in self.nodes.values() {
            let Some(kind) = node.kind else { continue };
            support.record(kind);
            if let Some(percent) = node.percent {
                // Sum processes on one engine, then show the busiest engine in each category.
                let value = sample.percent(kind);
                *value = Some(value.map_or(percent, |value| value.max(percent)));
            } else {
                incomplete.record(kind);
            }
        }
        if incomplete.encode {
            sample.encode_percent = None;
        }
        if incomplete.decode {
            sample.decode_percent = None;
        }
        if incomplete.codec {
            sample.codec_percent = None;
        }
        sample
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LUID: LUID = LUID {
        HighPart: -1,
        LowPart: 0x158d7,
    };

    #[test]
    fn attributes_only_known_video_engines_of_the_selected_physical_adapter() {
        let name =
            |luid, phys, kind| format!("pid_42_luid_{luid}_phys_{phys}_eng_3_engtype_{kind}");
        for (kind, expected) in [
            ("videoencode", Some(Engine::Encode)),
            ("video encode", Some(Engine::Encode)),
            ("video encode 0", Some(Engine::Encode)),
            ("videodecode", Some(Engine::Decode)),
            ("video decode", Some(Engine::Decode)),
            ("video decode 2", Some(Engine::Decode)),
            ("videocodec", Some(Engine::Codec)),
            ("video codec", Some(Engine::Codec)),
            ("video codec 0", Some(Engine::Codec)),
            ("video codec 12", Some(Engine::Codec)),
            ("video codec -1", None),
            ("video codec 0#1", None),
            ("video jpeg 0", None),
            ("video", None),
            ("videoprocessing", None),
        ] {
            assert_eq!(
                Instance::parse(&name("0xffffffff_0x000158d7", 0, kind), LUID)
                    .map(|instance| instance.kind),
                expected
            );
        }
        assert!(
            Instance::parse(&name("0x00000000_0x000158d7", 0, "video codec 0"), LUID).is_none()
        );
        assert!(
            Instance::parse(&name("0xffffffff_0x00011cad", 0, "video codec 0"), LUID).is_none()
        );
        assert!(
            Instance::parse(&name("0xffffffff_0x000158d7", 1, "video codec 0"), LUID).is_none()
        );
    }

    #[test]
    fn sums_processes_per_node_then_reports_the_busiest_node() {
        let mut engines = Engines::default();
        let mut support = Support::default();
        for (process, node, kind, percent) in [
            (1, 1, Engine::Encode, 10.0),
            (2, 1, Engine::Encode, 20.0),
            (1, 2, Engine::Encode, 25.0),
            (2, 3, Engine::Decode, 0.0),
        ] {
            engines.record(
                Instance {
                    process,
                    node,
                    kind,
                },
                Some(percent),
            );
        }
        let sample = engines.sample(&mut support);
        assert_eq!(sample.encode_percent, Some(30.0));
        assert_eq!(sample.decode_percent, Some(0.0));
        assert_eq!(sample.codec_percent, None);
        assert!(support.encode && support.decode && !support.codec);
    }

    #[test]
    fn warming_or_missing_instances_do_not_become_zero() {
        let mut engines = Engines::default();
        let mut support = Support::default();
        engines.record(
            Instance {
                process: 1,
                node: 1,
                kind: Engine::Codec,
            },
            Some(20.0),
        );
        engines.record(
            Instance {
                process: 2,
                node: 1,
                kind: Engine::Codec,
            },
            None,
        );
        assert_eq!(engines.sample(&mut support).codec_percent, None);
        assert!(support.codec);
        engines.clear();
        assert_eq!(engines.sample(&mut support).codec_percent, None);
        assert!(support.codec);
    }
}
