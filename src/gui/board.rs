use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::watch::sensor_catalog;

pub(super) const MAX_COLUMNS: usize = 6;
pub(super) const MAX_ROWS: usize = 4;

const VERSION: &str = "version=1";
const MAX_CONFIG_BYTES: u64 = 8192;
const PERIOD_SECONDS: [u32; 4] = [30, 60, 120, 300];
static NEXT_TEMP: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Board {
    pub(super) columns: usize,
    pub(super) rows: usize,
    pub(super) period: usize,
    pub(super) panels: Vec<Vec<usize>>,
}

impl Board {
    pub(super) fn new(active: impl IntoIterator<Item = usize>) -> Self {
        let mut board = Self {
            columns: MAX_COLUMNS,
            rows: 2,
            period: 1,
            panels: Vec::new(),
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

    pub(super) fn toggle(&mut self, sensor: usize) {
        if let Some(index) = self.panels.iter().position(|panel| panel.contains(&sensor)) {
            self.panels[index].retain(|&entry| entry != sensor);
            if self.panels[index].is_empty() {
                self.panels.remove(index);
            }
        } else if sensor_catalog().nth(sensor).is_some() {
            self.panels.push(vec![sensor]);
        }
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
        self.panels.remove(source);
        true
    }

    pub(super) fn split(&mut self, panel: usize) -> bool {
        let Some(series) = self.panels.get_mut(panel).filter(|series| series.len() > 1) else {
            return false;
        };
        let separated = series.split_off(1).into_iter().map(|sensor| vec![sensor]);
        self.panels.splice(panel + 1..panel + 1, separated);
        true
    }

    pub(super) fn reset_selection(&mut self, active: impl IntoIterator<Item = usize>) {
        let mut seen = vec![false; sensor_catalog().count()];
        self.panels = active
            .into_iter()
            .filter(|&sensor| mark_new(&mut seen, sensor))
            .map(|sensor| vec![sensor])
            .collect();
    }

    fn panel_unit(&self, index: usize) -> Option<&'static str> {
        let sensor = *self.panels.get(index)?.first()?;
        sensor_catalog().nth(sensor).map(|(_, unit)| unit)
    }

    fn encode(&self) -> String {
        let catalog: Vec<_> = sensor_catalog().collect();
        let mut text = format!(
            "{VERSION}\ncolumns={}\nrows={}\nperiod={}\n",
            self.columns, self.rows, PERIOD_SECONDS[self.period],
        );
        for panel in &self.panels {
            let keys: Vec<_> = panel.iter().map(|&sensor| catalog[sensor].0).collect();
            let _ = writeln!(text, "panel={}", keys.join(","));
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
                "panel" => {
                    has_panel_entries = true;
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
                        } else {
                            board.panels.push(vec![sensor]);
                        }
                    }
                }
                _ => {}
            }
        }
        (!has_panel_entries || !board.panels.is_empty()).then_some(board)
    }
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
