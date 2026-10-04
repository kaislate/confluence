//! A small time-series plot over telemetry history; `None` samples (disconnects)
//! are drawn as breaks, never as a false line.

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Shape, Stroke, Vec2, WidgetInfo, WidgetType};

/// Runs of consecutive finite samples, as (index, value).
pub fn segments(values: &[Option<f64>]) -> Vec<Vec<(usize, f64)>> {
    let mut out = Vec::new();
    let mut run = Vec::new();
    for (i, v) in values.iter().enumerate() {
        match v {
            Some(x) if x.is_finite() => run.push((i, *x)),
            _ => {
                if !run.is_empty() {
                    out.push(std::mem::take(&mut run));
                }
            }
        }
    }
    if !run.is_empty() {
        out.push(run);
    }
    out
}

/// The y range of all finite samples, with 10 % margin (a flat line gets ±1).
pub fn y_range(series: &[&[Option<f64>]]) -> Option<(f64, f64)> {
    let mut it = series.iter().flat_map(|s| s.iter()).filter_map(|v| v.filter(|x| x.is_finite()));
    let first = it.next()?;
    let (lo, hi) = it.fold((first, first), |(lo, hi), x| (lo.min(x), hi.max(x)));
    if hi - lo < 1e-9 {
        return Some((lo - 1.0, hi + 1.0));
    }
    let m = (hi - lo) * 0.1;
    Some((lo - m, hi + m))
}

pub struct Series<'a> {
    pub name: &'a str,
    pub color: Color32,
    pub values: Vec<Option<f64>>,
}

/// Draws `series` right-aligned over `len` sample slots (the newest at the right edge).
pub fn plot(ui: &mut egui::Ui, label: &str, height: f32, len: usize, series: &[Series]) {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover());
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Other, true, label));
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 3.0, ui.visuals().extreme_bg_color);
    let all: Vec<&[Option<f64>]> = series.iter().map(|s| s.values.as_slice()).collect();
    let Some((lo, hi)) = y_range(&all) else {
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            "no data yet",
            FontId::proportional(11.0),
            ui.visuals().weak_text_color(),
        );
        return;
    };
    let len = len.max(2);
    let to_pos = |offset: usize, i: usize, v: f64| {
        let x = rect.left() + ((offset + i) as f32 / (len - 1) as f32) * rect.width();
        let y = rect.bottom() - ((v - lo) / (hi - lo)) as f32 * rect.height();
        Pos2::new(x, y)
    };
    for s in series {
        let offset = len.saturating_sub(s.values.len());
        for seg in segments(&s.values) {
            let points: Vec<Pos2> = seg.iter().map(|&(i, v)| to_pos(offset, i, v)).collect();
            if points.len() == 1 {
                painter.circle_filled(points[0], 1.5, s.color);
            } else {
                painter.add(Shape::line(points, Stroke::new(1.5, s.color)));
            }
        }
    }
    let font = FontId::proportional(10.0);
    let weak = ui.visuals().weak_text_color();
    painter.text(rect.left_top() + Vec2::new(3.0, 2.0), Align2::LEFT_TOP, format!("{hi:.1}"), font.clone(), weak);
    painter.text(
        rect.left_bottom() + Vec2::new(3.0, -2.0),
        Align2::LEFT_BOTTOM,
        format!("{lo:.1}"),
        font.clone(),
        weak,
    );
    let mut x = rect.right() - 4.0;
    for s in series.iter().rev() {
        let r: Rect = painter.text(Pos2::new(x, rect.top() + 2.0), Align2::RIGHT_TOP, s.name, font.clone(), s.color);
        x = r.left() - 8.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gaps_split_the_line() {
        let v = [Some(1.0), Some(2.0), None, Some(3.0), Some(f64::NAN), Some(4.0)];
        assert_eq!(segments(&v), vec![vec![(0, 1.0), (1, 2.0)], vec![(3, 3.0)], vec![(5, 4.0)]]);
        assert!(segments(&[None, None]).is_empty());
    }

    #[test]
    fn the_y_range_covers_every_series_with_a_margin() {
        let a = [Some(10.0), Some(20.0)];
        let b = [Some(0.0), None];
        let (lo, hi) = y_range(&[&a, &b]).unwrap();
        assert!(lo < 0.0 && hi > 20.0, "{lo}..{hi}");
        let flat = [Some(5.0), Some(5.0)];
        let (lo, hi) = y_range(&[&flat]).unwrap();
        assert!(lo < 5.0 && hi > 5.0, "a flat line still gets a range");
        assert_eq!(y_range(&[&[None]]), None);
    }
}
