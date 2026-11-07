use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{COLORREF, POINT, RECT, SIZE};
use windows_sys::Win32::Graphics::Gdi::{
    ANTIALIASED_QUALITY, CLIP_DEFAULT_PRECIS, CreateFontW, CreatePen, DC_BRUSH, DC_PEN,
    DEFAULT_CHARSET, DEFAULT_GUI_FONT, DEFAULT_PITCH, DT_CENTER, DT_END_ELLIPSIS, DT_LEFT,
    DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE, DT_VCENTER, DeleteObject, DrawTextW, Ellipse, FW_NORMAL,
    FW_SEMIBOLD, FillRect, GetStockObject, GetTextExtentPoint32W, HDC, HGDIOBJ, IntersectClipRect,
    OUT_DEFAULT_PRECIS, PS_SOLID, Polyline, RestoreDC, RoundRect, SaveDC, SelectObject, SetBkMode,
    SetDCBrushColor, SetDCPenColor, SetTextColor, TRANSPARENT,
};

use crate::watch::{MetricSnapshot, Reading};

pub(super) const CARD: COLORREF = rgb(19, 27, 41);
pub(super) const BORDER: COLORREF = rgb(42, 56, 78);
pub(super) const TEXT: COLORREF = rgb(230, 239, 250);
pub(super) const MUTED: COLORREF = rgb(151, 169, 191);
const PLOT: COLORREF = rgb(14, 21, 33);
const GRID: COLORREF = rgb(37, 49, 68);

pub(super) const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    r as u32 | ((g as u32) << 8) | ((b as u32) << 16)
}

pub(super) fn paint(
    dc: HDC,
    rect: RECT,
    dpi: u32,
    series: &[(&MetricSnapshot, COLORREF)],
    at: Instant,
    window: Duration,
) {
    let Some(&(first, _)) = series.first() else {
        return;
    };
    let unit = first.unit.trim();
    if series.iter().any(|(metric, _)| metric.unit.trim() != unit) {
        return;
    }
    let px = |dips: i32| (dips * dpi as i32 + 48) / 96;
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    if width < px(62) || height < px(43) {
        return;
    }

    let views: Vec<_> = series
        .iter()
        .map(|&(metric, color)| {
            let points = visible_points(&metric.points, at, window);
            SeriesView {
                metric,
                color,
                timeline: Timeline {
                    points,
                    at,
                    window,
                    interval: metric.interval,
                },
                stats: Statistics::from_points(points),
            }
        })
        .collect();
    let scale = common_scale(unit, views.iter().filter_map(|view| view.stats.as_ref()));
    let legend_font = Object::font(px(11), FW_SEMIBOLD);
    let detail_font = Object::font(px(10), FW_NORMAL);
    // Restore the device context before releasing the selected fonts.
    let Some(_state) = SavedDc::new(dc) else {
        return;
    };

    // The caller supplies a live drawing context and pixel bounds for this paint.
    unsafe {
        IntersectClipRect(dc, rect.left, rect.top, rect.right, rect.bottom);
        SetBkMode(dc, TRANSPARENT as i32);
        SelectObject(dc, GetStockObject(DC_BRUSH));
        SelectObject(dc, GetStockObject(DC_PEN));
        SetDCBrushColor(dc, CARD);
        SetDCPenColor(dc, BORDER);
        RoundRect(
            dc,
            rect.left,
            rect.top,
            rect.right,
            rect.bottom,
            px(6),
            px(6),
        );
    }

    let left = rect.left + px(6);
    let right = rect.right - px(6);
    let columns = ((right - left) / px(135).max(1)).clamp(1, views.len() as i32);
    let rows = legend_rows(views.len(), columns, height >= px(68));
    let legend_row = px(if height < px(100) { 12 } else { 15 });
    let legend_top = rect.top + px(3);
    let legend_bottom = legend_top + rows * legend_row;
    let stat_row = px(12);
    let stats_width = px(if views.len() == 1 { 185 } else { 240 });
    let show_stats = views.len() <= 4
        && width >= stats_width
        && height >= px(130) + views.len() as i32 * stat_row;
    let stats_height = if show_stats {
        views.len() as i32 * stat_row
    } else {
        0
    };
    let time_height = px(13);
    let bottom = rect.bottom - px(3);
    let plot = RECT {
        left: left + px(if width >= px(150) { 39 } else { 33 }),
        top: legend_bottom + px(3),
        right,
        bottom: bottom - time_height - stats_height,
    };

    legend_font.select(dc);
    paint_legend(
        dc,
        RECT {
            left,
            top: legend_top,
            right,
            bottom: legend_bottom,
        },
        &views,
        columns,
        rows,
        px(5),
        px(125),
    );
    if plot.right <= plot.left || plot.bottom - plot.top < px(5) {
        return;
    }

    detail_font.select(dc);
    draw_grid(dc, plot);
    if let Some(scale) = scale {
        draw_ticks(dc, plot, left, px(5), scale);
        let weight = px(if width < px(160) { 1 } else { 2 }).max(1);
        for view in &views {
            draw_trace(dc, plot, view.color, scale, view.timeline, weight);
        }
    }

    let time_top = plot.bottom;
    let time_bottom = time_top + time_height;
    let range = if window.as_secs() >= 60 && window.as_secs().is_multiple_of(60) {
        format!("−{} min", window.as_secs() / 60)
    } else {
        format!("−{} s", window.as_secs())
    };
    let time_axis = (plot.left, time_top, right, time_bottom);
    let plot_width = right - plot.left;
    if plot_width >= px(100) {
        text(dc, time_axis, &range, MUTED, DT_LEFT);
        text(dc, time_axis, unit, MUTED, DT_CENTER);
        text(dc, time_axis, "Agora", MUTED, DT_RIGHT);
    } else {
        if plot_width >= px(58) {
            text(dc, time_axis, &range, MUTED, DT_LEFT);
        }
        text(dc, time_axis, unit, MUTED, DT_RIGHT);
    }

    if show_stats {
        paint_statistics(
            dc,
            RECT {
                left,
                top: time_bottom,
                right,
                bottom,
            },
            &views,
            px(80),
        );
    }
}

struct SeriesView<'a> {
    metric: &'a MetricSnapshot,
    color: COLORREF,
    timeline: Timeline<'a>,
    stats: Option<Statistics>,
}

fn legend_rows(series: usize, columns: i32, room_for_second_row: bool) -> i32 {
    let row_limit = if room_for_second_row { 2 } else { 1 };
    series.div_ceil(columns as usize).min(row_limit) as i32
}

fn paint_legend(
    dc: HDC,
    bounds: RECT,
    views: &[SeriesView<'_>],
    columns: i32,
    rows: i32,
    marker: i32,
    compact_width: i32,
) {
    let column_width = (bounds.right - bounds.left) / columns;
    let row_height = (bounds.bottom - bounds.top) / rows;
    let slots = (rows * columns) as usize;
    let visible = if views.len() > slots {
        slots - 1
    } else {
        views.len()
    };
    let cell = |index: usize| {
        let index = index as i32;
        let left = bounds.left + (index % columns) * column_width;
        let top = bounds.top + (index / columns) * row_height;
        (left, top, left + column_width - marker, top + row_height)
    };
    for (index, view) in views.iter().take(visible).enumerate() {
        let (left, top, right, bottom) = cell(index);
        let mark_top = top + (row_height - marker) / 2;
        fill(
            dc,
            RECT {
                left,
                top: mark_top,
                right: left + marker,
                bottom: mark_top + marker,
            },
            view.color,
        );

        let compact = column_width < compact_width;
        let current = current_label(view, compact, views.len() == 1);
        let value_width = (text_width(dc, &current) + 1).min(column_width / 2);
        let label_left = left + marker + marker / 2;
        let label_right = right - value_width - marker / 2;
        let name = if compact || text_width(dc, view.metric.name) > label_right - label_left {
            compact_name(view.metric.name)
        } else {
            view.metric.name
        };
        text(
            dc,
            (label_left, top, label_right, bottom),
            name,
            TEXT,
            DT_LEFT,
        );
        text(
            dc,
            (right - value_width, top, right, bottom),
            &current,
            view.color,
            DT_RIGHT,
        );
    }
    if visible < views.len() {
        let hidden = views.len() - visible;
        let label = if visible == 0 {
            format!("{hidden} curvas")
        } else {
            format!("+{hidden} curvas")
        };
        text(dc, cell(visible), &label, MUTED, DT_LEFT);
    }
}

fn current_label(view: &SeriesView<'_>, compact: bool, single: bool) -> String {
    let Timeline {
        points,
        at,
        interval,
        ..
    } = view.timeline;
    match current_reading(points, at, interval) {
        Some(Reading::Value(value)) if value.is_finite() => {
            let mut display = value_text(value);
            if (single && !compact) || view.metric.unit.trim() == "%" {
                display.push_str(view.metric.unit);
            }
            display
        }
        Some(Reading::Pending) => if compact { "…" } else { "Aguardando" }.to_owned(),
        Some(Reading::Unavailable) => if compact { "N/D" } else { "Indisponível" }.to_owned(),
        Some(Reading::Failed | Reading::Value(_)) => "Falha".to_owned(),
        None => "—".to_owned(),
    }
}

fn compact_name(name: &str) -> &str {
    match name {
        "Temperatura" => "Temp.",
        "Atividade de memória" => "Ativ. memória",
        "Atividade dos SMs" => "SMs ativos",
        "Ocupação dos SMs" => "Ocup. SMs",
        "Largura de banda da DRAM" => "Banda DRAM",
        "Clock de memória" => "Clock mem.",
        "Uso de CPU" => "CPU",
        _ => name.strip_prefix("Atividade ").unwrap_or(name),
    }
}

fn paint_statistics(dc: HDC, bounds: RECT, views: &[SeriesView<'_>], label_width: i32) {
    let row_height = (bounds.bottom - bounds.top) / views.len() as i32;
    let multiple = views.len() > 1;
    for (index, view) in views.iter().enumerate() {
        let top = bounds.top + index as i32 * row_height;
        let bottom = top + row_height;
        let mut left = bounds.left;
        if multiple {
            text(
                dc,
                (left, top, left + label_width, bottom),
                compact_name(view.metric.name),
                view.color,
                DT_LEFT,
            );
            left += label_width;
        }
        let line = view.stats.as_ref().map_or_else(
            || "Mín —   Máx —   Méd —".to_owned(),
            |stats| {
                format!(
                    "Mín {}   Máx {}   Méd {}",
                    value_text(stats.min),
                    value_text(stats.max),
                    value_text(stats.mean)
                )
            },
        );
        text(dc, (left, top, bounds.right, bottom), &line, MUTED, DT_LEFT);
    }
}

fn visible_points(
    points: &[(Instant, Reading)],
    at: Instant,
    window: Duration,
) -> &[(Instant, Reading)] {
    let start = points.partition_point(|(time, _)| at.saturating_duration_since(*time) > window);
    let end = points.partition_point(|(time, _)| *time <= at);
    &points[start..end]
}

fn current_reading(
    points: &[(Instant, Reading)],
    at: Instant,
    interval: Duration,
) -> Option<Reading> {
    points
        .last()
        .and_then(|&(time, reading)| close_in_time(time, at, interval).then_some(reading))
}

fn close_in_time(previous: Instant, next: Instant, interval: Duration) -> bool {
    next.saturating_duration_since(previous) <= interval.saturating_add(interval / 2)
}

fn measured(reading: Reading) -> Option<f64> {
    match reading {
        Reading::Value(value) if value.is_finite() => Some(value),
        _ => None,
    }
}

struct Statistics {
    min: f64,
    max: f64,
    mean: f64,
}

impl Statistics {
    fn from_points(points: &[(Instant, Reading)]) -> Option<Self> {
        let mut values = points.iter().filter_map(|&(_, reading)| measured(reading));
        let first = values.next()?;
        let (min, max, sum, count) =
            values.fold((first, first, first, 1), |(min, max, sum, count), value| {
                (min.min(value), max.max(value), sum + value, count + 1)
            });
        Some(Self {
            min,
            max,
            mean: sum / f64::from(count),
        })
    }
}

#[derive(Clone, Copy)]
struct Scale {
    low: f64,
    high: f64,
}

fn common_scale<'a>(unit: &str, stats: impl Iterator<Item = &'a Statistics>) -> Option<Scale> {
    let limits = stats
        .map(|stats| (stats.min, stats.max))
        .reduce(|(low, high), (min, max)| (low.min(min), high.max(max)));
    Scale::for_metric(unit, limits)
}

impl Scale {
    fn for_metric(unit: &str, limits: Option<(f64, f64)>) -> Option<Self> {
        let minimum_span = match unit.trim() {
            "%" => {
                return Some(Self {
                    low: 0.0,
                    high: 100.0,
                });
            }
            "°C" | "W" => 8.0,
            "MHz" => 200.0,
            "MiB" => 128.0,
            _ => 1.0,
        };
        let (min, max) = limits?;
        let spread = max - min;
        let padding = (spread * 0.12).max((minimum_span - spread) / 2.0);
        let mut low = min - padding;
        if min >= 0.0 {
            low = low.max(0.0);
        }
        let high = max + padding;
        let step = nice_step((high - low) / 2.0);
        Some(Self {
            low: (low / step).floor() * step,
            high: (high / step).ceil() * step,
        })
    }

    fn ordinate(self, rect: RECT, value: f64) -> i32 {
        let fraction = ((value - self.low) / (self.high - self.low)).clamp(0.0, 1.0);
        rect.bottom - 1 - (f64::from(rect.bottom - rect.top - 1) * fraction).round() as i32
    }
}

fn nice_step(step: f64) -> f64 {
    let power = 10.0_f64.powf(step.log10().floor());
    let scaled = step / power;
    let rounded = if scaled <= 1.0 {
        1.0
    } else if scaled <= 2.0 {
        2.0
    } else if scaled <= 5.0 {
        5.0
    } else {
        10.0
    };
    power * rounded
}

fn value_text(value: f64) -> String {
    let magnitude = value.abs();
    let digits = if magnitude >= 10.0 { 1 } else { 2 };
    if magnitude > 0.0 && magnitude < 0.005 {
        return if value > 0.0 {
            "<0,01".to_owned()
        } else {
            ">−0,01".to_owned()
        };
    }
    format!("{value:.digits$}").replace('.', ",")
}

fn axis_text(value: f64) -> String {
    if value.abs() < 0.005 {
        return "0".to_owned();
    }
    format!("{value:.2}")
        .trim_end_matches('0')
        .trim_end_matches('.')
        .replace('.', ",")
}

fn draw_grid(dc: HDC, plot: RECT) {
    fill(dc, plot, PLOT);
    unsafe {
        SelectObject(dc, GetStockObject(DC_PEN));
        SetDCPenColor(dc, GRID);
    }
    let width = plot.right - plot.left - 1;
    let height = plot.bottom - plot.top - 1;
    let rows = if height >= 100 { 4 } else { 2 };
    let columns = if width >= 180 { 8 } else { 4 };
    for division in 0..=rows {
        let y = plot.top + height * division / rows;
        line(dc, (plot.left, y), (plot.right - 1, y));
    }
    for division in 0..=columns {
        let x = plot.left + width * division / columns;
        line(dc, (x, plot.top), (x, plot.bottom - 1));
    }
}

fn draw_ticks(dc: HDC, plot: RECT, left: i32, margin: i32, scale: Scale) {
    let step = (scale.high - scale.low) / 2.0;
    let ticks = [scale.low, scale.high, scale.low + step];
    let count = if plot.bottom - plot.top >= 4 * margin + 2 {
        3
    } else {
        2
    };
    for value in ticks.into_iter().take(count) {
        let y = scale.ordinate(plot, value);
        text(
            dc,
            (left, y - margin, plot.left - margin, y + margin),
            &axis_text(value),
            MUTED,
            DT_RIGHT,
        );
    }
}

#[derive(Clone, Copy)]
struct Timeline<'a> {
    points: &'a [(Instant, Reading)],
    at: Instant,
    window: Duration,
    interval: Duration,
}

fn draw_trace(
    dc: HDC,
    plot: RECT,
    color: COLORREF,
    scale: Scale,
    timeline: Timeline<'_>,
    weight: i32,
) {
    let Timeline {
        points,
        at,
        window,
        interval,
    } = timeline;
    let pen = Object::pen(weight, color);
    let Some(_state) = SavedDc::new(dc) else {
        return;
    };
    unsafe {
        IntersectClipRect(dc, plot.left, plot.top, plot.right, plot.bottom);
        SelectObject(dc, GetStockObject(DC_BRUSH));
        SetDCBrushColor(dc, color);
        if pen.0.is_null() {
            SetDCPenColor(dc, color);
        } else {
            SelectObject(dc, pen.0);
        }
    }

    let mut segment = Vec::with_capacity(points.len());
    let mut previous = None;
    for &(time, reading) in points {
        let value = measured(reading);
        if value.is_none()
            || previous.is_some_and(|previous| !close_in_time(previous, time, interval))
        {
            draw_segment(dc, &segment, weight);
            segment.clear();
        }
        previous = value.map(|value| {
            let fraction = if window.is_zero() {
                1.0
            } else {
                1.0 - at.saturating_duration_since(time).as_secs_f64() / window.as_secs_f64()
            };
            let x = plot.left
                + (f64::from(plot.right - plot.left - 1) * fraction.clamp(0.0, 1.0)).round() as i32;
            segment.push(POINT {
                x,
                y: scale.ordinate(plot, value),
            });
            time
        });
    }
    draw_segment(dc, &segment, weight);
}

fn draw_segment(dc: HDC, segment: &[POINT], radius: i32) {
    unsafe {
        match segment {
            [] => (),
            [point] => {
                Ellipse(
                    dc,
                    point.x - radius,
                    point.y - radius,
                    point.x + radius + 1,
                    point.y + radius + 1,
                );
            }
            _ => {
                Polyline(dc, segment.as_ptr(), segment.len() as i32);
            }
        }
    }
}

fn line(dc: HDC, start: (i32, i32), end: (i32, i32)) {
    let points = [
        POINT {
            x: start.0,
            y: start.1,
        },
        POINT { x: end.0, y: end.1 },
    ];
    unsafe { Polyline(dc, points.as_ptr(), 2) };
}

fn fill(dc: HDC, bounds: RECT, color: COLORREF) {
    // The stock DC brush is retained by Windows.
    unsafe {
        let brush = GetStockObject(DC_BRUSH);
        SelectObject(dc, brush);
        SetDCBrushColor(dc, color);
        FillRect(dc, &bounds, brush);
    }
}

fn text_width(dc: HDC, value: &str) -> i32 {
    let mut units = [0; 128];
    let count = text_units(value, &mut units);
    let mut size = SIZE::default();
    unsafe { GetTextExtentPoint32W(dc, units.as_ptr(), count, &mut size) };
    size.cx
}

fn text_units(value: &str, units: &mut [u16]) -> i32 {
    let mut count = 0;
    for (slot, unit) in units.iter_mut().zip(value.encode_utf16()) {
        *slot = unit;
        count += 1;
    }
    count
}

fn text(dc: HDC, bounds: (i32, i32, i32, i32), value: &str, color: COLORREF, align: u32) {
    let mut rect = RECT {
        left: bounds.0,
        top: bounds.1,
        right: bounds.2,
        bottom: bounds.3,
    };
    if rect.left >= rect.right {
        return;
    }
    // The dashboard strings are short. Keep their UTF-16 representation on the stack.
    let mut units = [0; 128];
    let count = text_units(value, &mut units);
    unsafe {
        SetTextColor(dc, color);
        DrawTextW(
            dc,
            units.as_ptr(),
            count,
            &mut rect,
            align | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
        );
    }
}

struct SavedDc {
    dc: HDC,
    state: i32,
}

impl SavedDc {
    fn new(dc: HDC) -> Option<Self> {
        let state = unsafe { SaveDC(dc) };
        if state == 0 {
            None
        } else {
            Some(Self { dc, state })
        }
    }
}

impl Drop for SavedDc {
    fn drop(&mut self) {
        unsafe { RestoreDC(self.dc, self.state) };
    }
}

struct Object(HGDIOBJ);

impl Object {
    fn font(height: i32, weight: u32) -> Self {
        let name: [u16; 9] = [83, 101, 103, 111, 101, 32, 85, 73, 0]; // Segoe UI
        Self(unsafe {
            CreateFontW(
                -height,
                0,
                0,
                0,
                weight as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET as u32,
                OUT_DEFAULT_PRECIS as u32,
                CLIP_DEFAULT_PRECIS as u32,
                ANTIALIASED_QUALITY as u32,
                DEFAULT_PITCH as u32,
                name.as_ptr(),
            )
        })
    }

    fn pen(width: i32, color: COLORREF) -> Self {
        Self(unsafe { CreatePen(PS_SOLID, width, color) })
    }

    fn select(&self, dc: HDC) {
        unsafe {
            let font = if self.0.is_null() {
                GetStockObject(DEFAULT_GUI_FONT)
            } else {
                self.0
            };
            SelectObject(dc, font);
        }
    }
}

impl Drop for Object {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { DeleteObject(self.0) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statistics_only_include_measured_values_inside_the_window() {
        let start = Instant::now();
        let at = start + Duration::from_secs(5);
        let points = [
            (start, Reading::Value(900.0)),
            (start + Duration::from_secs(1), Reading::Value(0.0)),
            (start + Duration::from_secs(2), Reading::Pending),
            (start + Duration::from_secs(3), Reading::Value(12.0)),
            (start + Duration::from_secs(4), Reading::Failed),
            (at, Reading::Unavailable),
            (at + Duration::from_secs(1), Reading::Value(900.0)),
        ];
        let visible = visible_points(&points, at, Duration::from_secs(4));
        let stats = Statistics::from_points(visible).unwrap();
        assert_eq!((stats.min, stats.max, stats.mean), (0.0, 12.0, 6.0));
        assert!(matches!(
            current_reading(visible, at, Duration::from_secs(1)),
            Some(Reading::Unavailable)
        ));
    }

    #[test]
    fn traces_break_on_missing_intervals_and_do_not_report_stale_values() {
        let at = Instant::now();
        let interval = Duration::from_secs(1);
        assert!(close_in_time(
            at,
            at + Duration::from_millis(1350),
            interval
        ));
        assert!(!close_in_time(at, at + 2 * interval, interval));
        assert!(
            current_reading(&[(at, Reading::Value(25.0))], at + 2 * interval, interval).is_none()
        );
    }

    #[test]
    fn percentage_scale_is_fixed_and_flat_values_keep_a_visible_axis_range() {
        let stats = Statistics {
            min: 62.0,
            max: 62.0,
            mean: 62.0,
        };
        let percentage = common_scale("%", [&stats].into_iter()).unwrap();
        assert_eq!((percentage.low, percentage.high), (0.0, 100.0));
        let adaptive = common_scale(" °C", [&stats].into_iter()).unwrap();
        assert!(adaptive.low < 62.0 && adaptive.high > 62.0);
        assert_eq!(axis_text(2.5), "2,5");
        assert_eq!(axis_text(16_311.0), "16311");
    }

    #[test]
    fn merged_adaptive_axis_covers_both_series_including_a_measured_zero() {
        let at = Instant::now();
        let first =
            Statistics::from_points(&[(at, Reading::Value(0.0)), (at, Reading::Pending)]).unwrap();
        let second =
            Statistics::from_points(&[(at, Reading::Failed), (at, Reading::Value(8.25))]).unwrap();
        let scale = common_scale(" W", [&first, &second].into_iter()).unwrap();
        assert_eq!(scale.low, 0.0);
        assert!(scale.high >= 8.25);
        assert!(common_scale(" W", std::iter::empty()).is_none());
        let percent = common_scale(" %", std::iter::empty()).unwrap();
        assert_eq!((percent.low, percent.high), (0.0, 100.0));
    }

    #[test]
    fn a_large_merge_keeps_its_legend_within_the_panel_budget() {
        assert_eq!(legend_rows(10, 1, true), 2);
        assert_eq!(legend_rows(10, 1, false), 1);
        assert_eq!(legend_rows(3, 2, true), 2);
        assert_eq!(legend_rows(1, 2, true), 1);
    }
}
