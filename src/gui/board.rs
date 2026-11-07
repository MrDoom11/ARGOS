use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::watch::{GUI_INTERVALS_MS, sensor_catalog};

pub(super) const MAX_COLUMNS: usize = 6;
pub(super) const MAX_ROWS: usize = 4;
pub(super) const MAX_SLOTS: usize = 64;

const VERSION: &str = "version=1";
const MAX_CONFIG_BYTES: u64 = 8192;
const PERIOD_SECONDS: [u32; 4] = [30, 60, 120, 300];
const DEFAULT_REFRESH_MS: u32 = 1000;
const MAX_COLORREF: u32 = 0x00ff_ffff;
static NEXT_TEMP: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Board {
    pub(super) columns: usize,
    pub(super) rows: usize,
    pub(super) period: usize,
    pub(super) refresh_ms: u32,
    pub(super) panels: Vec<Vec<usize>>,
    colors: Vec<Option<u32>>,
}

impl Board {
    pub(super) fn new(active: impl IntoIterator<Item = usize>) -> Self {
        let mut board = Self {
            columns: MAX_COLUMNS,
            rows: 2,
            period: 1,
            refresh_ms: DEFAULT_REFRESH_MS,
            panels: Vec::new(),
            colors: vec![None; sensor_catalog().count()],
        };
        board.reset_selection(active);
        board
    }

    pub(super) fn load() -> Option<Self> {
        let path = config_path()?;
        let mut text = String::new();
        File::open(path)
            .ok()?
            .take(MAX_CONFIG_BYTES + 1)
            .read_to_string(&mut text)
            .ok()?;
        Self::decode(&text)
    }

    pub(super) fn save(&self) -> io::Result<()> {
        let path = config_path().ok_or_else(|| io::Error::other("LOCALAPPDATA indisponível"))?;
        replace_file(&path, self.encode().as_bytes())
    }

    pub(super) fn contains(&self, sensor: usize) -> bool {
        self.panels.iter().any(|panel| panel.contains(&sensor))
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

    pub(super) fn toggle(&mut self, sensor: usize) {
        if let Some(index) = self.panels.iter().position(|panel| panel.contains(&sensor)) {
            self.panels[index].retain(|&entry| entry != sensor);
            if self.panels[index].is_empty() {
                self.remove_empty_panel(index);
            }
        } else if sensor_catalog().nth(sensor).is_some() {
            if self.panels.len() < MAX_SLOTS {
                self.panels.push(vec![sensor]);
            } else if let Some(panel) = self.panels.iter_mut().find(|panel| panel.is_empty()) {
                panel.push(sensor);
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
                .panel_unit(source)
                .zip(self.panel_unit(target))
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
        let mut seen = vec![false; sensor_catalog().count()];
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

    fn panel_unit(&self, index: usize) -> Option<&'static str> {
        let sensor = *self.panels.get(index)?.first()?;
        sensor_catalog().nth(sensor).map(|(_, unit)| unit)
    }

    fn encode(&self) -> String {
        let catalog: Vec<_> = sensor_catalog().collect();
        let mut text = format!(
            "{VERSION}\ncolumns={}\nrows={}\nperiod={}\nrefresh_ms={}\n",
            self.columns, self.rows, PERIOD_SECONDS[self.period], self.refresh_ms,
        );
        for panel in &self.panels {
            let keys: Vec<_> = panel.iter().map(|&sensor| catalog[sensor].0).collect();
            let _ = writeln!(text, "panel={}", keys.join(","));
        }
        for ((key, _), &color) in catalog.iter().zip(&self.colors) {
            if let Some(color) = color {
                let rgb = swap_red_blue(color);
                let _ = writeln!(text, "color.{key}=#{rgb:06X}");
            }
        }
        text
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
        let mut seen = vec![false; catalog.len()];
        let mut board = Self::new([]);
        let mut has_panel_entries = false;
        for (key, value) in lines.filter_map(|line| line.split_once('=')) {
            let value = value.trim();
            match key.trim() {
                "columns" => board.columns = bounded(value, MAX_COLUMNS).unwrap_or(board.columns),
                "rows" => board.rows = bounded(value, MAX_ROWS).unwrap_or(board.rows),
                "period" => {
                    if let Ok(seconds) = value.parse()
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
                "panel" => {
                    if board.panels.len() >= MAX_SLOTS {
                        continue;
                    }
                    has_panel_entries |= !value.is_empty();
                    let first = board.panels.len();
                    for sensor in value
                        .split(',')
                        .filter_map(|key| catalog.iter().position(|(id, _)| *id == key.trim()))
                        .filter(|&sensor| mark_new(&mut seen, sensor))
                    {
                        let unit = catalog[sensor].1;
                        // Registros editados com unidades distintas viram painéis separados.
                        if let Some(panel) = board.panels[first..]
                            .iter_mut()
                            .find(|panel| catalog[panel[0]].1 == unit)
                        {
                            panel.push(sensor);
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
                        && let Some(sensor) = catalog.iter().position(|(id, _)| *id == key)
                        && let Some(color) = parse_color(value)
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

fn config_path() -> Option<PathBuf> {
    let root = PathBuf::from(std::env::var_os("LOCALAPPDATA")?);
    root.is_absolute()
        .then(|| root.join("ARGOS").join("dashboard.cfg"))
}

fn replace_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
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
    fn merges_compatible_panels_in_place_and_splits_the_result() {
        let mut board = Board::new([5, 0, 6, 11, 1]);
        assert!(!board.merge(0, 1));
        assert!(!board.merge(2, 2));
        assert!(board.merge(0, 2));
        assert!(board.merge(2, 1));
        assert_eq!(board.panels, [vec![0], vec![6, 5, 11], vec![1]]);
        assert!(board.split(1));
        assert_eq!(board.panels, [vec![0], vec![6], vec![5], vec![11], vec![1]]);
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
        let mut board = Board::new([0, 5, 6, 11]);
        assert!(board.move_panel(0, MAX_SLOTS - 1));
        assert!(board.merge(1, 3));
        assert!(board.panels[1].is_empty());
        assert_eq!(board.panels[3], [11, 5]);
        assert!(board.split(3));
        assert_eq!(board.panels[3], [11]);
        assert_eq!(board.panels[4], [5]);
        board.toggle(5);
        assert!(board.panels[4].is_empty());
        board.toggle(5);
        assert!(board.contains(5));
        assert_eq!(board.panels[MAX_SLOTS - 1], [0]);
        assert_eq!(board.panels.len(), MAX_SLOTS);
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
        board.toggle(5);
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
        assert_eq!((board.columns, board.rows, board.period), (6, 2, 1));
        assert_eq!(board.panels, [vec![0], vec![5, 6]]);
        assert!(Board::decode("version=2\n").is_none());
        assert!(Board::decode("version=1\npanel=unknown\n").is_none());
        assert!(Board::decode(&"x".repeat(MAX_CONFIG_BYTES as usize + 1)).is_none());
    }

    #[test]
    fn toggles_and_reset_keep_each_sensor_visible_at_most_once() {
        let mut board = Board::new([5, 6, 5, usize::MAX]);
        assert!(board.merge(1, 0));
        board.toggle(5);
        assert!(!board.contains(5));
        assert_eq!(board.panels, [vec![6]]);
        board.toggle(5);
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
