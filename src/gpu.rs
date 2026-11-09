use std::collections::HashSet;

use crate::adlx::{self, Adlx};
use crate::cli::GpuIndex;
use crate::nvml::{self, Nvml};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum Selection {
    #[default]
    Auto,
    Index(GpuIndex),
    Device(String),
}

#[derive(Clone)]
pub(crate) struct GpuInfo {
    pub(crate) index: GpuIndex,
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) short_name: String,
    pub(crate) persistent: bool,
    reported_id: Option<String>,
}

impl GpuInfo {
    fn new(index: GpuIndex, name: String, reported_id: Option<String>) -> Self {
        let reported_id = reported_id.filter(|id| !id.trim().is_empty());
        let persistent = reported_id.is_some();
        let id = match reported_id.as_deref() {
            Some(id) => match index {
                GpuIndex::Nvidia(_) => format!("nvidia:{id}"),
                GpuIndex::Radeon(_) => format!("radeon:{id}"),
            },
            None => session_id(index),
        };
        Self {
            index,
            id,
            name,
            short_name: index_label(index),
            persistent,
            reported_id,
        }
    }

    pub(crate) fn label(&self) -> String {
        format!("{} · {}", self.short_name, self.name)
    }

    pub(crate) fn selection(&self) -> Selection {
        if self.persistent {
            Selection::Device(self.id.clone())
        } else {
            Selection::Index(self.index)
        }
    }

    pub(crate) fn matches_selection(&self, selection: &Selection) -> bool {
        match selection {
            Selection::Auto => false,
            Selection::Index(index) => *index == self.index,
            Selection::Device(id) => id == &self.id,
        }
    }

    fn verify_identity(&self, current_id: Option<&str>) -> Result<(), String> {
        if self
            .reported_id
            .as_deref()
            .is_some_and(|expected_id| current_id != Some(expected_id))
        {
            return Err(format!(
                "a identidade da GPU {} mudou após a detecção",
                index_label(self.index)
            ));
        }
        Ok(())
    }
}

fn index_label(index: GpuIndex) -> String {
    match index {
        GpuIndex::Nvidia(index) => format!("NVIDIA {index}"),
        GpuIndex::Radeon(index) => format!("Radeon {index}"),
    }
}

fn session_id(index: GpuIndex) -> String {
    match index {
        GpuIndex::Nvidia(index) => format!("nvidia-session:{index}"),
        GpuIndex::Radeon(index) => format!("radeon-session:{index}"),
    }
}

fn disambiguate_ids(devices: &mut [GpuInfo]) {
    let mut used_ids = HashSet::new();
    let mut duplicated_ids = HashSet::new();
    for device in devices.iter() {
        if !used_ids.insert(&device.id) {
            duplicated_ids.insert(device.id.clone());
        }
    }
    for device in devices {
        if duplicated_ids.contains(&device.id) {
            device.id = session_id(device.index);
            device.persistent = false;
        }
    }
}

pub(crate) enum Device<'a> {
    Nvidia(nvml::Device<'a>),
    Radeon(adlx::Device<'a>),
}

pub(crate) struct Inventory {
    nvml: Result<Nvml, nvml::Error>,
    adlx: Result<Adlx, String>,
    devices: Vec<GpuInfo>,
}

impl Inventory {
    pub(crate) fn discover() -> Self {
        let mut devices = Vec::new();
        let nvml = Nvml::load().and_then(|nvml| {
            for index in 0..nvml.device_count()? {
                if let Ok(device) = nvml.device(index) {
                    let gpu_index = GpuIndex::Nvidia(index);
                    let name = device.name().unwrap_or_else(|_| index_label(gpu_index));
                    devices.push(GpuInfo::new(gpu_index, name, device.uuid()));
                }
            }
            Ok(nvml)
        });
        let adlx = Adlx::load();
        if let Ok(adlx) = &adlx {
            for index in 0..adlx.device_count() {
                if let Ok(device) = adlx.device(index) {
                    devices.push(GpuInfo::new(
                        GpuIndex::Radeon(index),
                        device.name().to_owned(),
                        device.persistent_id().map(str::to_owned),
                    ));
                }
            }
        }
        disambiguate_ids(&mut devices);
        Self {
            nvml,
            adlx,
            devices,
        }
    }

    pub(crate) fn devices(&self) -> &[GpuInfo] {
        &self.devices
    }

    pub(crate) fn nvml(&self) -> Result<&Nvml, &nvml::Error> {
        self.nvml.as_ref()
    }

    pub(crate) fn adlx_error(&self) -> Option<&str> {
        self.adlx.as_ref().err().map(String::as_str)
    }

    pub(crate) fn open(&self, index: GpuIndex) -> Result<Device<'_>, String> {
        let info = device_at_index(&self.devices, index)?;
        match index {
            GpuIndex::Nvidia(index) => {
                let device = self
                    .nvml()
                    .map_err(ToString::to_string)?
                    .device(index)
                    .map_err(|error| error.to_string())?;
                info.verify_identity(device.uuid().as_deref())?;
                Ok(Device::Nvidia(device))
            }
            GpuIndex::Radeon(index) => {
                let device = self.adlx.as_ref().map_err(Clone::clone)?.device(index)?;
                info.verify_identity(device.persistent_id())?;
                Ok(Device::Radeon(device))
            }
        }
    }

    pub(crate) fn resolve(&self, choice: &Selection) -> Result<Option<&GpuInfo>, String> {
        resolve_device(&self.devices, choice)
    }
}

fn device_at_index(devices: &[GpuInfo], index: GpuIndex) -> Result<&GpuInfo, String> {
    devices
        .iter()
        .find(|device| device.index == index)
        .ok_or_else(|| format!("GPU {} indisponível", index_label(index)))
}

fn resolve_device<'a>(
    devices: &'a [GpuInfo],
    choice: &Selection,
) -> Result<Option<&'a GpuInfo>, String> {
    match choice {
        Selection::Auto => Ok(devices
            .iter()
            .find(|device| device.index == GpuIndex::Nvidia(0))
            .or_else(|| {
                devices
                    .iter()
                    .find(|device| matches!(device.index, GpuIndex::Nvidia(_)))
            })
            .or_else(|| devices.first())),
        Selection::Index(index) => device_at_index(devices, *index).map(Some),
        Selection::Device(id) => devices
            .iter()
            .find(|device| device.persistent && device.id == *id)
            .map(Some)
            .ok_or_else(|| format!("GPU selecionada indisponível ({id})")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpu(index: GpuIndex, id: &str) -> GpuInfo {
        GpuInfo::new(index, "Modelo de GPU".to_owned(), Some(id.to_owned()))
    }

    #[test]
    fn auto_prefers_nvidia_zero_and_falls_back_to_available_devices() {
        let devices = [
            gpu(GpuIndex::Radeon(0), "rd"),
            gpu(GpuIndex::Nvidia(1), "nv1"),
            gpu(GpuIndex::Nvidia(0), "nv0"),
        ];
        for (device_count, index) in [
            (3, Some(GpuIndex::Nvidia(0))),
            (2, Some(GpuIndex::Nvidia(1))),
            (1, Some(GpuIndex::Radeon(0))),
            (0, None),
        ] {
            let device = resolve_device(&devices[..device_count], &Selection::Auto).unwrap();
            assert_eq!(device.map(|device| device.index), index);
        }
    }

    #[test]
    fn explicit_choices_match_the_selected_vendor_or_id() {
        let devices = [
            gpu(GpuIndex::Nvidia(0), "nv"),
            gpu(GpuIndex::Radeon(0), "rd"),
        ];
        for choice in [
            Selection::Index(GpuIndex::Radeon(0)),
            Selection::Device(devices[1].id.clone()),
        ] {
            assert_eq!(
                resolve_device(&devices, &choice).unwrap().unwrap().index,
                GpuIndex::Radeon(0),
            );
        }
        for choice in [
            Selection::Index(GpuIndex::Nvidia(1)),
            Selection::Device("radeon:removed".to_owned()),
        ] {
            assert!(resolve_device(&devices, &choice).is_err());
        }
    }

    #[test]
    fn identical_models_have_distinct_labels_and_ids() {
        let mut devices = [
            gpu(GpuIndex::Radeon(0), "rd0"),
            gpu(GpuIndex::Radeon(1), "rd1"),
        ];
        assert_eq!(devices[0].name, devices[1].name);
        assert_ne!(devices[0].label(), devices[1].label());
        assert_ne!(devices[0].id, devices[1].id);

        devices[1] = gpu(GpuIndex::Radeon(1), "rd0");
        disambiguate_ids(&mut devices);
        assert_ne!(devices[0].id, devices[1].id);
        for device in &devices {
            assert!(!device.persistent);
            assert!(resolve_device(&devices, &Selection::Device(device.id.clone())).is_err());
            assert!(device.verify_identity(Some("rd0")).is_ok());
            assert!(device.verify_identity(Some("changed")).is_err());
        }
    }

    #[test]
    fn verifies_reported_identity_and_allows_unidentified_devices_for_the_session() {
        let identified = gpu(GpuIndex::Nvidia(0), "expected");
        assert!(identified.verify_identity(Some("expected")).is_ok());
        assert!(identified.verify_identity(Some("changed")).is_err());
        assert!(identified.verify_identity(None).is_err());

        let unidentified = GpuInfo::new(GpuIndex::Nvidia(0), "Modelo de GPU".to_owned(), None);
        assert!(unidentified.verify_identity(None).is_ok());
    }
}
