use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;

use super::appearance::{Appearance, Theme};
use super::ping_targets::normalize_target;
use crate::game_network::custom_ping::MAX_TARGETS;
use crate::watch::{
    CPU_POWER_INDEX, CPU_TEMPERATURE_INDEX, CPU_USAGE_INDEX, CUSTOM_PING_INDEX, CpuMetrics,
    DEFAULT_FPS_CEILING, FRAME_SENSOR_START, GUI_INTERVALS_MS, RAM_USED_INDEX, sensor_catalog,
};

pub(super) const MAX_COLUMNS: usize = 6;
pub(super) const MAX_ROWS: usize = 4;
pub(super) const MAX_SLOTS: usize = 64;
pub(super) const INITIAL_SENSORS: [usize; 4] = [
    CPU_USAGE_INDEX,
    CPU_TEMPERATURE_INDEX,
    RAM_USED_INDEX,
    FRAME_SENSOR_START,
];

const VERSION: &str = "version=1";
const DASHBOARD_CONFIG: &str = "dashboard-multi.cfg";
const LEGACY_DASHBOARD_CONFIG: &str = "dashboard.cfg";
const DROPDOWN_CONFIG: &str = "dropdown.conf";
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_DROPDOWN_CONFIG_BYTES: u64 = 8192;
const MAX_GPU_BINDINGS: usize = MAX_SLOTS * CPU_USAGE_INDEX;
const PERIOD_SECONDS: [u32; 4] = [30, 60, 120, 300];
const DEFAULT_REFRESH_MS: u32 = 1000;
const MAX_COLORREF: u32 = 0x00ff_ffff;
const MAX_MONITOR_ID_BYTES: usize = 512;
static NEXT_TEMP: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DropdownSettings {
    pub(super) enabled: bool,
    pub(super) monitor: Option<String>,
    pub(super) alignment: u8,
    pub(super) width: u8,
    pub(super) hotkey: Option<DropdownHotkey>,
}

impl Default for DropdownSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            monitor: None,
            alignment: 50,
            width: 30,
            hotkey: None,
        }
    }
}

impl DropdownSettings {
    fn encode(&self) -> String {
        let mut text = format!(
            "{VERSION}\nenabled={}\nalignment={}\nwidth={}\n",
            self.enabled, self.alignment, self.width,
        );
        if let Some(monitor) = &self.monitor {
            let _ = writeln!(text, "monitor={}", hex::encode(monitor));
        }
        if let Some(DropdownHotkey { modifiers, key }) = self.hotkey {
            let _ = writeln!(text, "hotkey={modifiers},{key}");
        }
        text
    }

    fn decode(text: &str) -> Option<Self> {
        if text.len() as u64 > MAX_DROPDOWN_CONFIG_BYTES {
            return None;
        }
        let mut lines = text.trim_start_matches('\u{feff}').lines();
        if lines.next()?.trim() != VERSION {
            return None;
        }
        let mut settings = Self::default();
        for (key, value) in lines.filter_map(|line| line.split_once('=')) {
            let value = value.trim();
            match key.trim() {
                "enabled" => {
                    if let Ok(enabled) = value.parse() {
                        settings.enabled = enabled;
                    }
                }
                "alignment" => {
                    if let Ok(alignment @ 0..=100) = value.parse() {
                        settings.alignment = alignment;
                    }
                }
                "width" => {
                    if let Ok(width @ 5..=100) = value.parse() {
                        settings.width = width;
                    }
                }
                "monitor" => {
                    if let Some(monitor) = decode_device_id(value) {
                        settings.monitor = Some(monitor);
                    }
                }
                "hotkey" => {
                    if let Some(hotkey) = DropdownHotkey::decode(value) {
                        settings.hotkey = Some(hotkey);
                    }
                }
                _ => {}
            }
        }
        Some(settings)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DropdownHotkey {
    pub(super) modifiers: u32,
    pub(super) key: u32,
}

impl DropdownHotkey {
    pub(super) fn new(modifiers: u32, key: u32) -> Option<Self> {
        const MODIFIERS: u32 = MOD_ALT | MOD_CONTROL | MOD_SHIFT | MOD_WIN;
        let vk = u16::try_from(key).ok()?;
        if modifiers == 0 || modifiers & !MODIFIERS != 0 {
            return None;
        }
        // A F12 é reservada pelo Windows para o depurador.
        let supported = matches!(
            vk,
            VK_0..=VK_9 | VK_A..=VK_Z | VK_F1..=VK_F11 | VK_F13..=VK_F24
                | VK_NUMPAD0..=VK_DIVIDE | VK_OEM_1..=VK_OEM_3 | VK_OEM_4..=VK_OEM_8
                | VK_OEM_102 | VK_BACK | VK_TAB | VK_RETURN | VK_ESCAPE | VK_SPACE
                | VK_PRIOR..=VK_DOWN | VK_INSERT | VK_DELETE
        );
        let reserved = match vk {
            VK_TAB => modifiers & (MOD_ALT | MOD_WIN) != 0,
            VK_ESCAPE => modifiers & (MOD_ALT | MOD_CONTROL | MOD_WIN) != 0,
            VK_DELETE => modifiers & (MOD_CONTROL | MOD_ALT) == (MOD_CONTROL | MOD_ALT),
            VK_F4 => modifiers & MOD_ALT != 0 || modifiers == (MOD_WIN | MOD_CONTROL),
            VK_LEFT | VK_RIGHT | VK_UP | VK_DOWN | VK_0..=VK_9 => modifiers & MOD_WIN != 0,
            VK_SPACE => modifiers & (MOD_ALT | MOD_WIN) != 0,
            VK_D => modifiers == MOD_WIN || modifiers == (MOD_WIN | MOD_CONTROL),
            VK_E | VK_I | VK_L | VK_M | VK_P | VK_R | VK_S | VK_X => modifiers == MOD_WIN,
            _ => false,
        };
        (supported && !reserved).then_some(Self { modifiers, key })
    }

    fn decode(text: &str) -> Option<Self> {
        let (modifiers, key) = text.split_once(',')?;
        Self::new(modifiers.trim().parse().ok()?, key.trim().parse().ok()?)
    }
}

#[derive(Debug, PartialEq, Eq)]
struct GpuSensor {
    gpu_id: String,
    base: usize,
    persistent: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Board {
    pub(super) columns: usize,
    pub(super) rows: usize,
    pub(super) period: usize,
    pub(super) refresh_ms: u32,
    pub(super) fps_ceiling: NonZeroU16,
    pub(super) panels: Vec<Vec<usize>>,
    pub(super) window_bounds: Option<[i32; 4]>,
    pub(super) gpu_ids: Vec<String>,
    pub(super) ping_targets: Vec<String>,
    pub(super) appearance: Appearance,
    pub(super) dropdown: DropdownSettings,
    colors: Vec<Option<u32>>,
    gpu_sensors: Vec<GpuSensor>,
}

impl Board {
    pub(super) fn new(active: impl IntoIterator<Item = usize>) -> Self {
        let mut board = Self {
            columns: INITIAL_SENSORS.len(),
            rows: 1,
            period: 1,
            refresh_ms: DEFAULT_REFRESH_MS,
            fps_ceiling: const { NonZeroU16::new(DEFAULT_FPS_CEILING).unwrap() },
            panels: Vec::new(),
            window_bounds: None,
            gpu_ids: Vec::new(),
            ping_targets: Vec::new(),
            appearance: Appearance::default(),
            dropdown: DropdownSettings::default(),
            colors: vec![None; sensor_catalog().count()],
            gpu_sensors: Vec::new(),
        };
        board.reset_selection(active);
        board
    }

    pub(super) fn load() -> Option<Self> {
        Self::load_from(&config_path()?)
    }

    fn load_from(path: &Path) -> Option<Self> {
        let dashboard = match File::open(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                File::open(path.with_file_name(LEGACY_DASHBOARD_CONFIG))
            }
            result => result,
        };
        let mut board = dashboard
            .ok()
            .and_then(|file| read_config(file, MAX_CONFIG_BYTES))
            .and_then(|text| Self::decode(&text));
        if let Some(dropdown) = File::open(path.with_file_name(DROPDOWN_CONFIG))
            .ok()
            .and_then(|file| read_config(file, MAX_DROPDOWN_CONFIG_BYTES))
            .and_then(|text| DropdownSettings::decode(&text))
        {
            board
                .get_or_insert_with(|| Self::new(INITIAL_SENSORS))
                .dropdown = dropdown;
        }
        board
    }

    pub(super) fn save(&self) -> io::Result<()> {
        let path = config_path().ok_or_else(|| io::Error::other("LOCALAPPDATA indisponível"))?;
        let text = self.encode();
        if text.len() as u64 > MAX_CONFIG_BYTES {
            return Err(io::Error::other(
                "configuração do painel excede o limite de tamanho",
            ));
        }
        replace_file(&path, text.as_bytes())
    }

    pub(super) fn save_dropdown(&self) -> io::Result<()> {
        let path = config_path().ok_or_else(|| io::Error::other("LOCALAPPDATA indisponível"))?;
        replace_file(
            &path.with_file_name(DROPDOWN_CONFIG),
            self.dropdown.encode().as_bytes(),
        )
    }

    pub(super) fn contains(&self, sensor: usize) -> bool {
        self.panels.iter().any(|panel| panel.contains(&sensor))
    }

    pub(super) fn gpu_sensor(&self, slot: usize) -> Option<(&str, usize)> {
        let binding = self
            .gpu_sensors
            .get(slot.checked_sub(sensor_catalog().count())?)?;
        Some((&binding.gpu_id, binding.base))
    }

    pub(super) fn gpu_sensor_slot(&self, gpu_id: &str, base: usize) -> Option<usize> {
        self.gpu_sensor_index(gpu_id, base)
            .map(|index| sensor_catalog().count() + index)
    }

    pub(super) fn register_gpu_sensor(
        &mut self,
        gpu_id: &str,
        persistent: bool,
        base: usize,
    ) -> usize {
        assert!(base < CPU_USAGE_INDEX);
        let persistent = persistent && is_persistent_gpu_id(gpu_id);
        if let Some(index) = self.gpu_sensor_index(gpu_id, base) {
            self.gpu_sensors[index].persistent |= persistent;
            return sensor_catalog().count() + index;
        }
        let slot = self.colors.len();
        self.gpu_sensors.push(GpuSensor {
            gpu_id: gpu_id.to_owned(),
            base,
            persistent,
        });
        self.colors.push(None);
        slot
    }

    fn gpu_sensor_index(&self, gpu_id: &str, base: usize) -> Option<usize> {
        self.gpu_sensors
            .iter()
            .position(|binding| binding.base == base && binding.gpu_id == gpu_id)
    }

    pub(super) fn bind_legacy_gpu(&mut self, gpu_id: &str) -> bool {
        if !is_persistent_gpu_id(gpu_id) {
            return false;
        }
        let mut changed = false;
        for base in 0..CPU_USAGE_INDEX {
            if !self.contains(base) && self.color(base).is_none() {
                continue;
            }
            let slot = self.register_gpu_sensor(gpu_id, true, base);
            let already_bound = self.contains(slot);
            for panel in &mut self.panels {
                if already_bound {
                    panel.retain(|&sensor| sensor != base);
                } else if let Some(sensor) = panel.iter_mut().find(|sensor| **sensor == base) {
                    *sensor = slot;
                }
            }
            if let Some(color) = self.colors[base].take() {
                self.colors[slot].get_or_insert(color);
            }
            changed = true;
        }
        self.trim_empty_tail();
        changed
    }

    pub(super) fn sensor_unit(&self, slot: usize) -> Option<&'static str> {
        let base = self.gpu_sensor(slot).map_or(slot, |(_, base)| base);
        sensor_catalog().nth(base).map(|(_, unit)| unit)
    }

    pub(super) fn cpu_metrics(&self) -> CpuMetrics {
        CpuMetrics {
            temperature: self.contains(CPU_TEMPERATURE_INDEX),
            power: self.contains(CPU_POWER_INDEX),
        }
    }

    pub(super) fn active_ping_targets(&self) -> &[String] {
        if self.contains(CUSTOM_PING_INDEX) {
            &self.ping_targets
        } else {
            &[]
        }
    }

    pub(super) fn color(&self, sensor: usize) -> Option<u32> {
        self.colors.get(sensor).copied().flatten()
    }

    pub(super) fn set_color(&mut self, sensor: usize, color: Option<u32>) -> bool {
        let Some(current) = self.colors.get_mut(sensor) else {
            return false;
        };
        if *current == color || color.is_some_and(|color| color > MAX_COLORREF) {
            return false;
        }
        *current = color;
        true
    }

    pub(super) fn toggle(&mut self, sensor: usize, first_slot: usize) {
        if let Some(index) = self.panels.iter().position(|panel| panel.contains(&sensor)) {
            self.panels[index].retain(|&entry| entry != sensor);
            if self.panels[index].is_empty() {
                self.remove_empty_panel(index);
            }
        } else if self.sensor_unit(sensor).is_some() {
            let first_slot = first_slot.min(self.panels.len());
            if let Some(slot) = (first_slot..MAX_SLOTS)
                .chain(0..first_slot)
                .find(|&slot| self.panels.get(slot).is_none_or(Vec::is_empty))
            {
                if let Some(panel) = self.panels.get_mut(slot) {
                    panel.push(sensor);
                } else {
                    self.panels.push(vec![sensor]);
                }
            }
        }
    }

    pub(super) fn move_panel(&mut self, from: usize, to: usize) -> bool {
        if from == to || to >= MAX_SLOTS || self.panels.get(from).is_none_or(Vec::is_empty) {
            return false;
        }
        if to >= self.panels.len() {
            self.panels.resize_with(to + 1, Vec::new);
        }
        self.panels.swap(from, to);
        self.trim_empty_tail();
        true
    }

    pub(super) fn can_merge(&self, source: usize, target: usize) -> bool {
        source != target
            && self
                .mergeable_panel_unit(source)
                .zip(self.mergeable_panel_unit(target))
                .is_some_and(|(left, right)| left == right)
    }

    pub(super) fn merge(&mut self, source: usize, target: usize) -> bool {
        if !self.can_merge(source, target) {
            return false;
        }
        let mut series = std::mem::take(&mut self.panels[source]);
        self.panels[target].append(&mut series);
        self.remove_empty_panel(source);
        true
    }

    pub(super) fn split(&mut self, panel: usize) -> bool {
        let Some(series) = self.panels.get(panel).filter(|series| series.len() > 1) else {
            return false;
        };
        let additional = series.len() - 1;
        if self.panels.iter().all(|panel| !panel.is_empty()) {
            if self.panels.len() + additional > MAX_SLOTS {
                return false;
            }
            let separated = self.panels[panel]
                .split_off(1)
                .into_iter()
                .map(|sensor| vec![sensor]);
            self.panels.splice(panel + 1..panel + 1, separated);
        } else {
            let slots: Vec<_> = (panel + 1..self.panels.len())
                .chain(0..panel)
                .filter(|&index| self.panels[index].is_empty())
                .chain(self.panels.len()..MAX_SLOTS)
                .take(additional)
                .collect();
            if slots.len() != additional {
                return false;
            }
            for (sensor, slot) in self.panels[panel].split_off(1).into_iter().zip(slots) {
                if slot == self.panels.len() {
                    self.panels.push(vec![sensor]);
                } else {
                    self.panels[slot].push(sensor);
                }
            }
        }
        true
    }

    pub(super) fn reset_selection(&mut self, active: impl IntoIterator<Item = usize>) {
        let mut seen = vec![false; self.colors.len()];
        self.panels = active
            .into_iter()
            .filter(|&sensor| mark_new(&mut seen, sensor))
            .take(MAX_SLOTS)
            .map(|sensor| vec![sensor])
            .collect();
    }

    fn remove_empty_panel(&mut self, index: usize) {
        let has_other_empty = self
            .panels
            .iter()
            .enumerate()
            .any(|(slot, panel)| slot != index && panel.is_empty());
        if has_other_empty {
            self.trim_empty_tail();
        } else {
            self.panels.remove(index);
        }
    }

    fn trim_empty_tail(&mut self) {
        while self.panels.last().is_some_and(Vec::is_empty) {
            self.panels.pop();
        }
    }

    fn mergeable_panel_unit(&self, index: usize) -> Option<&'static str> {
        let sensor = *self.panels.get(index)?.first()?;
        if sensor == CUSTOM_PING_INDEX {
            return None;
        }
        self.sensor_unit(sensor)
    }

    fn encode(&self) -> String {
        let catalog: Vec<_> = sensor_catalog().collect();
        let theme = match self.appearance.theme {
            Theme::Dark => "dark",
            Theme::Light => "light",
        };
        let mut text = format!(
            "{VERSION}\ncolumns={}\nrows={}\nperiod={}\nrefresh_ms={}\nfps_ceiling={}\ntheme={theme}\nopacity={}\n",
            self.columns,
            self.rows,
            PERIOD_SECONDS[self.period],
            self.refresh_ms,
            self.fps_ceiling,
            self.appearance.opacity,
        );
        if let Some([left, top, right, bottom]) = self.window_bounds {
            let _ = writeln!(text, "window={left},{top},{right},{bottom}");
        }
        for gpu in self.gpu_ids.iter().filter(|id| is_persistent_gpu_id(id)) {
            let _ = writeln!(text, "gpu={}", hex::encode(gpu));
        }
        for target in &self.ping_targets {
            let _ = writeln!(text, "ping_target={target}");
        }
        for panel in &self.panels {
            let keys: Vec<_> = panel
                .iter()
                .filter_map(|&sensor| self.encode_sensor_key(sensor, &catalog))
                .collect();
            let _ = writeln!(text, "panel={}", keys.join(","));
        }
        for (sensor, &color) in self.colors.iter().enumerate() {
            if let Some(color) = color
                && let Some(key) = self.encode_sensor_key(sensor, &catalog)
            {
                let rgb = swap_red_blue(color);
                let _ = writeln!(text, "color.{key}=#{rgb:06X}");
            }
        }
        text
    }

    fn encode_sensor_key(&self, slot: usize, catalog: &[(&str, &str)]) -> Option<String> {
        if let Some(&(key, _)) = catalog.get(slot) {
            return Some(key.to_owned());
        }
        let binding = self.gpu_sensors.get(slot.checked_sub(catalog.len())?)?;
        binding.persistent.then(|| {
            format!(
                "{}@{}",
                catalog[binding.base].0,
                hex::encode(&binding.gpu_id)
            )
        })
    }

    fn decode_sensor_key(&mut self, key: &str, catalog: &[(&str, &str)]) -> Option<usize> {
        let (key, gpu) = key
            .split_once('@')
            .map_or((key, None), |(key, gpu)| (key, Some(gpu)));
        let base = catalog.iter().position(|(id, _)| *id == key)?;
        let Some(gpu) = gpu else { return Some(base) };
        if base >= CPU_USAGE_INDEX {
            return None;
        }
        let gpu = decode_persistent_gpu_id(gpu)?;
        if self.gpu_sensors.len() >= MAX_GPU_BINDINGS {
            return self.gpu_sensor_slot(&gpu, base);
        }
        Some(self.register_gpu_sensor(&gpu, true, base))
    }

    fn decode(text: &str) -> Option<Self> {
        if text.len() as u64 > MAX_CONFIG_BYTES {
            return None;
        }
        let mut lines = text.trim_start_matches('\u{feff}').lines();
        if lines.next()?.trim() != VERSION {
            return None;
        }
        let catalog: Vec<_> = sensor_catalog().collect();
        let mut seen = HashSet::new();
        let mut board = Self::new([]);
        let mut has_panel_entries = false;
        for (key, value) in lines.filter_map(|line| line.split_once('=')) {
            let value = value.trim();
            match key.trim() {
                "columns" => board.columns = bounded(value, MAX_COLUMNS).unwrap_or(board.columns),
                "rows" => board.rows = bounded(value, MAX_ROWS).unwrap_or(board.rows),
                "period" => {
                    if let Ok(seconds) = value.parse::<u32>()
                        && let Some(index) =
                            PERIOD_SECONDS.iter().position(|&period| period == seconds)
                    {
                        board.period = index;
                    }
                }
                "refresh_ms" => {
                    if let Ok(refresh_ms) = value.parse()
                        && GUI_INTERVALS_MS.contains(&refresh_ms)
                    {
                        board.refresh_ms = refresh_ms;
                    }
                }
                "fps_ceiling" => {
                    if let Ok(ceiling) = value.parse() {
                        board.fps_ceiling = ceiling;
                    }
                }
                "theme" => match value {
                    "dark" => board.appearance.theme = Theme::Dark,
                    "light" => board.appearance.theme = Theme::Light,
                    _ => {}
                },
                "opacity" => {
                    if let Ok(opacity @ 0..=100) = value.parse() {
                        board.appearance.opacity = opacity;
                    }
                }
                "window" => {
                    if let Some(bounds) = parse_window_bounds(value) {
                        board.window_bounds = Some(bounds);
                    }
                }
                "gpu" => {
                    if board.gpu_ids.len() < MAX_SLOTS
                        && let Some(gpu) = decode_persistent_gpu_id(value)
                        && !board.gpu_ids.contains(&gpu)
                    {
                        board.gpu_ids.push(gpu);
                    }
                }
                "ping_target" => {
                    if board.ping_targets.len() < MAX_TARGETS
                        && let Some(target) = normalize_target(value)
                        && !board.ping_targets.contains(&target)
                    {
                        board.ping_targets.push(target);
                    }
                }
                "panel" => {
                    if board.panels.len() >= MAX_SLOTS {
                        continue;
                    }
                    has_panel_entries |= !value.is_empty();
                    let first = board.panels.len();
                    for key in value.split(',').map(str::trim) {
                        let Some(sensor) = board.decode_sensor_key(key, &catalog) else {
                            continue;
                        };
                        if !seen.insert(sensor) {
                            continue;
                        }
                        let unit = board.sensor_unit(sensor);
                        // Registros editados com unidades distintas viram painéis separados.
                        if let Some(offset) = board.panels[first..].iter().position(|panel| {
                            sensor != CUSTOM_PING_INDEX
                                && panel[0] != CUSTOM_PING_INDEX
                                && board.sensor_unit(panel[0]) == unit
                        }) {
                            board.panels[first + offset].push(sensor);
                        } else if board.panels.len() < MAX_SLOTS {
                            board.panels.push(vec![sensor]);
                        }
                    }
                    if board.panels.len() == first {
                        board.panels.push(Vec::new());
                    }
                }
                key => {
                    if let Some(key) = key.strip_prefix("color.")
                        && let Some(color) = parse_color(value)
                        && let Some(sensor) = board.decode_sensor_key(key, &catalog)
                    {
                        board.set_color(sensor, Some(color));
                    }
                }
            }
        }
        board.trim_empty_tail();
        (!has_panel_entries || !board.panels.is_empty()).then_some(board)
    }
}

fn is_persistent_gpu_id(id: &str) -> bool {
    id.len() <= MAX_MONITOR_ID_BYTES
        && !id.chars().any(char::is_control)
        && id
            .strip_prefix("nvidia:")
            .or_else(|| id.strip_prefix("radeon:"))
            .is_some_and(|value| !value.is_empty())
}

fn decode_persistent_gpu_id(value: &str) -> Option<String> {
    decode_device_id(value).filter(|id| is_persistent_gpu_id(id))
}

fn decode_device_id(value: &str) -> Option<String> {
    if value.is_empty() || value.len() > 2 * MAX_MONITOR_ID_BYTES {
        return None;
    }
    let monitor = String::from_utf8(hex::decode(value).ok()?).ok()?;
    (!monitor.chars().any(char::is_control)).then_some(monitor)
}

fn parse_window_bounds(value: &str) -> Option<[i32; 4]> {
    let mut parts = value.split(',');
    let mut bounds = [0_i32; 4];
    for edge in &mut bounds {
        *edge = parts.next()?.trim().parse().ok()?;
    }
    let [left, top, right, bottom] = bounds;
    (parts.next().is_none() && right.checked_sub(left)? > 0 && bottom.checked_sub(top)? > 0)
        .then_some(bounds)
}

fn parse_color(value: &str) -> Option<u32> {
    let hex = value.strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(hex, 16).ok().map(swap_red_blue)
}

// COLORREF usa 0x00BBGGRR. O arquivo de configuração usa #RRGGBB.
fn swap_red_blue(color: u32) -> u32 {
    let [first, green, third, _] = color.to_le_bytes();
    u32::from_le_bytes([third, green, first, 0])
}

fn mark_new(seen: &mut [bool], sensor: usize) -> bool {
    seen.get_mut(sensor)
        .is_some_and(|selected| !std::mem::replace(selected, true))
}

fn bounded(text: &str, max: usize) -> Option<usize> {
    text.parse().ok().filter(|value| (1..=max).contains(value))
}

pub(super) fn config_path() -> Option<PathBuf> {
    let root = PathBuf::from(std::env::var_os("LOCALAPPDATA")?);
    root.is_absolute()
        .then(|| root.join("ARGOS").join(DASHBOARD_CONFIG))
}

fn read_config(file: File, max_bytes: u64) -> Option<String> {
    let mut text = String::new();
    file.take(max_bytes + 1).read_to_string(&mut text).ok()?;
    Some(text)
}

pub(super) fn replace_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let directory = path
        .parent()
        .ok_or_else(|| io::Error::other("diretório de configuração indisponível"))?;
    fs::create_dir_all(directory)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let serial = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    let temporary = path.with_extension(format!("{}.{now}.{serial}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let written = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    let result = written.and_then(|()| fs::rename(&temporary, path));
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_dashboard_has_four_requested_sensors_on_one_row() {
        let board = Board::new(INITIAL_SENSORS);
        let keys: Vec<_> = board
            .panels
            .iter()
            .map(|panel| sensor_catalog().nth(panel[0]).unwrap().0)
            .collect();
        assert_eq!((board.columns, board.rows), (4, 1));
        assert_eq!(
            keys,
            ["cpu-usage", "cpu-temperature", "ram-used", "present-fps"]
        );
    }

    #[test]
    fn merges_compatible_panels_in_place_and_splits_the_result() {
        let fp32 = sensor_catalog()
            .position(|(key, _)| key == "gpm-fp32")
            .unwrap();
        let mut board = Board::new([5, 0, 6, fp32, 1]);
        assert!(!board.merge(0, 1));
        assert!(!board.merge(2, 2));
        assert!(board.merge(0, 2));
        assert!(board.merge(2, 1));
        assert_eq!(board.panels, [vec![0], vec![6, 5, fp32], vec![1]]);
        assert!(board.split(1));
        assert_eq!(
            board.panels,
            [vec![0], vec![6], vec![5], vec![fp32], vec![1]]
        );
        assert!(!board.split(1));
    }

    #[test]
    fn configuration_round_trip_preserves_dimensions_period_and_series() {
        let mut board = Board::new([0, 5, 6, 11, 16]);
        board.columns = 4;
        board.rows = 3;
        board.period = 3;
        assert!(board.merge(2, 1));
        assert_eq!(Board::decode(&board.encode()), Some(board));
        let empty = Board::new([]);
        assert_eq!(Board::decode(&empty.encode()), Some(empty));
    }

    #[test]
    fn custom_ping_destinations_round_trip_in_their_own_panel() {
        let legacy = Board::new(INITIAL_SENSORS).encode();
        assert!(Board::decode(&legacy).unwrap().ping_targets.is_empty());

        let catalog: Vec<_> = sensor_catalog().collect();
        let ping = crate::watch::GAME_LATENCY_INDEX;
        let ping_key = catalog[ping].0;
        let custom_key = catalog[CUSTOM_PING_INDEX].0;
        let text = format!("{VERSION}\npanel={ping_key},{custom_key}\n")
            + concat!(
                "ping_target=EXAMPLE.COM\nping_target=example.com\n",
                "ping_target=https://example.com\nping_target=FE80::1%12\n",
                "ping_target=2001:DB8::0001\nping_target=192.0.2.1\n",
                "ping_target=192.0.2.2\n"
            );
        let mut board = Board::decode(&text).unwrap();
        assert_eq!(board.panels, [vec![ping], vec![CUSTOM_PING_INDEX]]);
        assert!(!board.can_merge(0, 1));
        assert!(!board.can_merge(1, 0));
        assert_eq!(
            board.active_ping_targets(),
            ["example.com", "fe80::1%12", "2001:db8::1", "192.0.2.1"]
        );
        assert_eq!(Board::decode(&board.encode()).as_ref(), Some(&board));
        board.toggle(CUSTOM_PING_INDEX, 0);
        assert!(board.active_ping_targets().is_empty());
        assert_eq!(board.ping_targets.len(), MAX_TARGETS);
    }

    #[test]
    fn window_bounds_are_optional_and_support_negative_monitor_coordinates() {
        let mut board = Board::new([0]);
        let legacy = board.encode();
        assert_eq!(Board::decode(&legacy).unwrap().window_bounds, None);

        board.window_bounds = Some([-1366, -60, -566, 260]);
        assert_eq!(Board::decode(&board.encode()), Some(board));
        for invalid in ["1,2,3", "1,2,3,4,5", "1,2,0,4", "-2147483648,1,1,2"] {
            let text = format!("{legacy}window={invalid}\n");
            assert_eq!(Board::decode(&text).unwrap().window_bounds, None);
        }
    }

    #[test]
    fn saves_gpu_identity_without_changing_the_sensor_selection() {
        let mut board = Board::new(INITIAL_SENSORS);
        assert!(Board::decode(&board.encode()).unwrap().gpu_ids.is_empty());
        let radeon = r"radeon:pnp:PCI\VEN_1002&DEV_15BF\3&ABC&0&01";
        let legacy = format!("{}gpu={}\n", board.encode(), hex::encode(radeon));
        assert_eq!(Board::decode(&legacy).unwrap().gpu_ids, [radeon]);
        board.gpu_ids = vec![radeon.into(), "nvidia:GPU-123".into()];
        assert_eq!(Board::decode(&board.encode()).unwrap(), board);
        let invalid = format!(
            "{}gpu=666f6f\ngpu={}\n",
            board.encode(),
            hex::encode(radeon)
        );
        assert_eq!(Board::decode(&invalid).unwrap(), board);
    }

    #[test]
    fn same_sensor_on_two_gpus_keeps_independent_series_and_colors() {
        let mut board = Board::new([]);
        let nvidia = board.register_gpu_sensor("nvidia:GPU-123", true, 0);
        let radeon = board.register_gpu_sensor("radeon:pci:01:00.0", true, 0);
        assert_ne!(nvidia, radeon);
        assert_eq!(board.register_gpu_sensor("nvidia:GPU-123", true, 0), nvidia);
        board.toggle(nvidia, 0);
        board.toggle(radeon, 0);
        assert!(board.merge(1, 0));
        assert!(board.set_color(nvidia, Some(0x0012_3456)));
        assert!(board.set_color(radeon, Some(0x00ab_cdef)));

        let reopened = Board::decode(&board.encode()).unwrap();
        let [nvidia, radeon] = reopened.panels[0].as_slice() else {
            panic!("as duas GPUs devem permanecer no painel");
        };
        assert_eq!(reopened.gpu_sensor(*nvidia), Some(("nvidia:GPU-123", 0)));
        assert_eq!(
            reopened.gpu_sensor(*radeon),
            Some(("radeon:pci:01:00.0", 0))
        );
        assert_eq!(reopened.color(*nvidia), Some(0x0012_3456));
        assert_eq!(reopened.color(*radeon), Some(0x00ab_cdef));
    }

    #[test]
    fn binds_legacy_gpu_charts_and_colors_to_the_persistent_device() {
        let mut board = Board::new([0, CPU_USAGE_INDEX]);
        assert!(board.set_color(0, Some(0x0012_3456)));
        assert!(board.set_color(1, Some(0x0065_4321)));
        assert!(!board.bind_legacy_gpu("nvidia-session:0"));
        assert!(board.bind_legacy_gpu("nvidia:GPU-123"));
        assert!(!board.bind_legacy_gpu("radeon:pci:01:00.0"));
        assert_eq!(board.panels[1], [CPU_USAGE_INDEX]);
        assert_eq!(
            board.gpu_sensor(board.panels[0][0]),
            Some(("nvidia:GPU-123", 0))
        );
        assert_eq!(board.color(board.panels[0][0]), Some(0x0012_3456));
        assert_eq!(board.color(0), None);
        let other = board.register_gpu_sensor("nvidia:GPU-123", true, 1);
        assert_eq!(board.color(other), Some(0x0065_4321));
        assert_eq!(Board::decode(&board.encode()), Some(board));
    }

    #[test]
    fn session_gpu_bindings_are_never_loaded_or_saved() {
        let mut board = Board::new([CPU_USAGE_INDEX]);
        let session_id = "radeon-session:0";
        board.gpu_ids.push(session_id.to_owned());
        let sensor = board.register_gpu_sensor(session_id, false, 0);
        board.toggle(sensor, 0);
        assert!(board.set_color(sensor, Some(0)));
        assert!(board.move_panel(1, 0));

        let encoded = board.encode();
        assert!(!encoded.contains('@'));
        assert!(!encoded.contains("gpu="));
        let session = hex::encode(session_id);
        let persistent = hex::encode("nvidia:GPU-123");
        let temperature = sensor_catalog().next().unwrap().0;
        let malformed = format!(
            "{encoded}gpu={session}\npanel={temperature}@{session},cpu-usage@{persistent}\n"
        );
        let reopened = Board::decode(&malformed).unwrap();
        assert!(reopened.gpu_ids.is_empty());
        assert_eq!(reopened.panels, [vec![], vec![CPU_USAGE_INDEX]]);
        assert!(reopened.gpu_sensors.is_empty());
    }

    #[test]
    fn dropdown_settings_preserve_monitor_position_width_and_hotkey() {
        let mut board = Board::new(INITIAL_SENSORS);
        let dashboard = board.encode();
        board.dropdown = DropdownSettings {
            enabled: true,
            monitor: Some(r"\\?\DISPLAY#ARGOS#5&abc&0&UID17#{e6f07b5f}".into()),
            alignment: 0,
            width: 100,
            hotkey: DropdownHotkey::new(MOD_CONTROL | MOD_SHIFT, u32::from(VK_F8)),
        };
        assert!(board.dropdown.hotkey.is_some());
        assert_eq!(
            DropdownSettings::decode(&board.dropdown.encode()),
            Some(board.dropdown.clone())
        );
        assert_eq!(board.encode(), dashboard);
    }

    #[test]
    fn legacy_configuration_leaves_dropdown_disabled() {
        let legacy = "version=1\ncolumns=3\nrows=2\npanel=cpu-usage\n";
        let board = Board::decode(legacy).unwrap();
        assert_eq!(board.dropdown, DropdownSettings::default());
        assert_eq!((board.columns, board.rows), (3, 2));
        assert_eq!(board.panels, [vec![CPU_USAGE_INDEX]]);
    }

    #[test]
    fn uses_legacy_dashboard_only_until_the_new_file_exists() {
        let dir = std::env::temp_dir().join(format!("argos-board-test-{}", std::process::id()));
        let path = dir.join(DASHBOARD_CONFIG);
        let legacy_path = dir.join(LEGACY_DASHBOARD_CONFIG);
        let legacy = Board::new([CPU_USAGE_INDEX]);
        let current = Board::new([CPU_TEMPERATURE_INDEX]);
        replace_file(&legacy_path, legacy.encode().as_bytes()).unwrap();
        assert_eq!(Board::load_from(&path), Some(legacy));

        replace_file(&path, current.encode().as_bytes()).unwrap();
        assert_eq!(Board::load_from(&path), Some(current));
        replace_file(&path, b"version=invalid\n").unwrap();
        assert_eq!(Board::load_from(&path), None);
        assert!(legacy_path.is_file());

        fs::remove_file(path).unwrap();
        fs::remove_file(legacy_path).unwrap();
        fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn invalid_dropdown_settings_keep_defaults() {
        let monitor = hex::encode("display\r\nrows=4");
        let text = format!(
            "version=1\nenabled=1\nalignment=101\nwidth=4\n\
             monitor={monitor}\nhotkey={MOD_CONTROL},{VK_F12}\n"
        );
        assert_eq!(
            DropdownSettings::decode(&text),
            Some(DropdownSettings::default())
        );
        assert_eq!(DropdownSettings::decode("version=2\nenabled=true\n"), None);
        for invalid in ["", "f", "gg", "ff", "00"] {
            assert_eq!(decode_device_id(invalid), None);
        }
    }

    #[test]
    fn dropdown_hotkeys_require_supported_keys_and_modifiers() {
        for (modifiers, key) in [
            (0, VK_A),
            (MOD_CONTROL | MOD_NOREPEAT, VK_A),
            (MOD_CONTROL, VK_CONTROL),
            (MOD_CONTROL, VK_F12),
            (MOD_CONTROL | MOD_ALT, VK_DELETE),
            (MOD_CONTROL | MOD_SHIFT, VK_ESCAPE),
            (MOD_ALT | MOD_SHIFT, VK_TAB),
            (MOD_ALT, VK_F4),
            (MOD_WIN, VK_L),
        ] {
            assert_eq!(DropdownHotkey::new(modifiers, u32::from(key)), None);
        }
        assert_eq!(DropdownHotkey::new(MOD_ALT, u32::MAX), None);
        for (modifiers, key) in [
            (MOD_ALT, VK_F8),
            (MOD_CONTROL | MOD_SHIFT, VK_DOWN),
            (MOD_WIN | MOD_SHIFT, VK_OEM_102),
        ] {
            let key = u32::from(key);
            assert_eq!(
                DropdownHotkey::new(modifiers, key),
                Some(DropdownHotkey { modifiers, key })
            );
        }
    }

    #[test]
    fn frame_charts_keep_independent_visibility_after_reopening() {
        let fps = FRAME_SENSOR_START;
        let frametime = fps + 1;
        let cpu_wait = fps + 2;
        let mut board = Board::new([0]);

        for sensor in [fps, frametime, cpu_wait] {
            board.toggle(sensor, 0);
        }
        assert_eq!(
            board.panels,
            [vec![0], vec![fps], vec![frametime], vec![cpu_wait]]
        );

        board.toggle(fps, 0);
        board.toggle(cpu_wait, 0);
        let config = board.encode();
        assert!(config.lines().any(|line| line == "panel=present-frametime"));
        let mut reopened = Board::decode(&config).unwrap();
        assert_eq!(reopened.panels, [vec![0], vec![frametime]]);

        reopened.toggle(frametime, 0);
        assert_eq!(Board::decode(&reopened.encode()), Some(Board::new([0])));
    }

    #[test]
    fn moving_to_an_occupied_slot_swaps_complete_panels() {
        let mut board = Board::new([0, 5, 6, 1]);
        assert!(board.merge(2, 1));
        assert!(board.move_panel(1, 0));
        assert_eq!(board.panels, [vec![5, 6], vec![0], vec![1]]);
        assert!(!board.move_panel(0, 0));
        assert!(!board.move_panel(10, 0));
        assert!(!board.move_panel(1, MAX_SLOTS));
    }

    #[test]
    fn moving_across_pages_keeps_exact_vacant_slots_after_reopening() {
        let mut board = Board::new([0, 5, 6]);
        board.columns = 2;
        board.rows = 2;
        assert!(board.move_panel(0, 8));
        assert!(board.panels[0].is_empty());
        assert_eq!(board.panels[1..3], [vec![5], vec![6]]);
        assert!(board.panels[3..8].iter().all(Vec::is_empty));
        assert_eq!(board.panels[8], [0]);
        assert_eq!(
            board
                .encode()
                .lines()
                .filter(|&line| line == "panel=")
                .count(),
            6
        );
        let mut reopened = Board::decode(&board.encode()).unwrap();
        assert_eq!(reopened, board);
        assert!(!reopened.move_panel(3, 1));
        assert!(reopened.move_panel(8, 0));
        assert_eq!(reopened.panels, [vec![0], vec![5], vec![6]]);
    }

    #[test]
    fn sparse_merge_split_and_toggle_leave_other_occupied_slots_in_place() {
        let fp32 = sensor_catalog()
            .position(|(key, _)| key == "gpm-fp32")
            .unwrap();
        let mut board = Board::new([0, 5, 6, fp32]);
        assert!(board.move_panel(0, MAX_SLOTS - 1));
        assert!(board.merge(1, 3));
        assert!(board.panels[1].is_empty());
        assert_eq!(board.panels[3], [fp32, 5]);
        assert!(board.split(3));
        assert_eq!(board.panels[3], [fp32]);
        assert_eq!(board.panels[4], [5]);
        board.toggle(5, 0);
        assert!(board.panels[4].is_empty());
        board.toggle(5, 0);
        assert!(board.contains(5));
        assert_eq!(board.panels[MAX_SLOTS - 1], [0]);
        assert_eq!(board.panels.len(), MAX_SLOTS);
    }

    #[test]
    fn adding_sensors_fills_vacant_slots_from_the_visible_row() {
        let mut board = Board::new([0, 1, 2, 3]);
        assert!(board.move_panel(1, 8));

        board.toggle(5, 4);
        assert_eq!(board.panels[4], [5]);
        assert!(board.panels[1].is_empty());

        board.toggle(6, 0);
        assert_eq!(board.panels[1], [6]);
        assert_eq!(board.panels[8], [1]);
        assert_eq!(board.panels.len(), 9);
    }

    #[test]
    fn configuration_ignores_records_beyond_the_slot_limit() {
        let mut keys = sensor_catalog().map(|(key, _)| key);
        let first = keys.next().unwrap();
        let overflow = keys.next().unwrap();
        let mut text = format!("{VERSION}\npanel={first}\n");
        text.push_str(&"panel=\n".repeat(MAX_SLOTS - 1));
        let _ = writeln!(text, "panel={overflow}");
        let board = Board::decode(&text).unwrap();
        assert_eq!(board.panels, [vec![0]]);
    }

    #[test]
    fn old_configuration_defaults_to_one_second_and_standard_colors() {
        let key = sensor_catalog().next().unwrap().0;
        let text = format!("{VERSION}\ncolumns=2\nrows=3\nperiod=120\npanel={key}\n");
        let board = Board::decode(&text).unwrap();
        assert_eq!((board.columns, board.rows, board.period), (2, 3, 2));
        assert_eq!(board.panels, [vec![0]]);
        assert_eq!(board.refresh_ms, 1000);
        assert_eq!(board.color(0), None);
    }

    #[test]
    fn fps_ceiling_round_trips_and_defaults_for_legacy_configuration() {
        let mut board = Board::decode("version=1\npanel=present-fps\n").unwrap();
        assert_eq!(board.fps_ceiling.get(), DEFAULT_FPS_CEILING);

        board.fps_ceiling = NonZeroU16::new(144).unwrap();
        let text = board.encode();
        assert!(text.contains("fps_ceiling=144\n"));
        let invalid = format!("{text}fps_ceiling=0\nfps_ceiling=65536\nfps_ceiling=-1\n");
        assert_eq!(Board::decode(&invalid), Some(board));
    }

    #[test]
    fn appearance_round_trips_and_rejects_unknown_values() {
        let legacy = "version=1\npanel=cpu-usage\n";
        let mut board = Board::decode(legacy).unwrap();
        assert_eq!(board.appearance.theme, Theme::Dark);
        assert_eq!(board.appearance.opacity, 100);

        board.appearance.theme = Theme::Light;
        board.appearance.opacity = 75;
        let text = board.encode();
        assert!(text.contains("theme=light\nopacity=75\n"));
        assert_eq!(Board::decode(&text).as_ref(), Some(&board));
        let malformed = format!("{text}theme=unknown\nopacity=101\nopacity=256\n");
        assert_eq!(Board::decode(&malformed), Some(board));
    }

    #[test]
    fn refresh_and_colors_round_trip_and_reject_invalid_values() {
        let mut board = Board::new([0]);
        board.refresh_ms = 200;
        assert!(board.set_color(0, Some(0x0056_3412)));
        assert!(board.set_color(1, Some(0)));
        assert!(!board.set_color(0, Some(0x0056_3412)));
        assert!(!board.set_color(0, Some(0x0156_3412)));
        assert!(!board.set_color(usize::MAX, Some(0)));

        let key = sensor_catalog().next().unwrap().0;
        let text = board.encode();
        assert!(text.contains(&format!("color.{key}=#123456\n")));
        assert!(text.contains("refresh_ms=200\n"));
        let malformed = format!(
            "{text}refresh_ms=123\ncolor.{key}=#+ABCD1\ncolor.{key}=#12345678\ncolor.unknown=#FFFFFF\n"
        );
        assert_eq!(Board::decode(&malformed), Some(board));
    }

    #[test]
    fn custom_color_survives_visibility_changes_and_group_edits() {
        let mut board = Board::new([5, 6]);
        assert!(board.set_color(5, Some(0x00ff_0000)));
        assert!(board.merge(1, 0));
        assert!(board.split(0));
        board.toggle(5, 0);
        board.reset_selection([6, 5]);
        assert_eq!(board.color(5), Some(0x00ff_0000));
        assert!(board.set_color(5, None));
        assert_eq!(board.color(5), None);
    }

    #[test]
    fn invalid_values_and_unknown_sensors_cannot_break_the_layout() {
        let catalog: Vec<_> = sensor_catalog().collect();
        let temperature = catalog[0].0;
        let activity = catalog[5].0;
        let memory_activity = catalog[6].0;
        let text = format!(
            "{VERSION}\ncolumns=0\nrows=999\nperiod=13\npanel={temperature},{activity},unknown,{memory_activity},{activity}\npanel={temperature}\n"
        );
        let board = Board::decode(&text).unwrap();
        assert_eq!((board.columns, board.rows, board.period), (4, 1, 1));
        assert_eq!(board.panels, [vec![0], vec![5, 6]]);
        assert!(Board::decode("version=2\n").is_none());
        assert!(Board::decode("version=1\npanel=unknown\n").is_none());
        assert!(Board::decode(&"x".repeat(MAX_CONFIG_BYTES as usize + 1)).is_none());
    }

    #[test]
    fn toggles_and_reset_keep_each_sensor_visible_at_most_once() {
        let mut board = Board::new([5, 6, 5, usize::MAX]);
        assert!(board.merge(1, 0));
        board.toggle(5, 0);
        assert!(!board.contains(5));
        assert_eq!(board.panels, [vec![6]]);
        board.toggle(5, 0);
        assert!(board.contains(5));
        assert_eq!(board.panels, [vec![6], vec![5]]);
        board.columns = 3;
        board.rows = 4;
        board.period = 0;
        board.reset_selection([11, 11, usize::MAX]);
        assert_eq!((board.columns, board.rows, board.period), (3, 4, 0));
        assert_eq!(board.panels, [vec![11]]);
    }
}
