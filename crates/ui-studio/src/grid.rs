//! The arrangement grid (Ableton's model). The visible division either follows the zoom
//! (`Adaptive`: the finest of bar, 1/2, 1/4, 1/8 ... 1/128 note whose lines stay at least
//! [`MIN_SPACING_PX`] apart) or is pinned (`Fixed`). A triplet toggle scales either by 2/3, and
//! snapping can be switched off. This is view state, not saved in the session. Snapping
//! (`automation::snap`, `snap_delta`) and the lines drawn in the arrangement both ask this module,
//! so they always agree. The piano roll keeps its own [`GridView`] (its zoom differs) and asks the
//! same functions with its own points per tick.

use soundcraft_model::Session;
use soundcraft_time::TICKS_PER_QUARTER as PPQ;

/// The finest grid is the one whose lines are still at least this many points apart.
pub const MIN_SPACING_PX: f64 = 12.0;
/// Sub-beat lines closer than this many points are not drawn (snapping is unaffected).
pub const MIN_DRAW_PX: f64 = 4.0;

/// Where the division comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum GridMode {
    /// Follows the zoom.
    #[default]
    Adaptive,
    /// Pinned to this many ticks (before the triplet scaling), whatever the zoom.
    Fixed(i64),
}

/// The arrangement's grid settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct GridView {
    pub mode: GridMode,
    pub triplet: bool,
    pub snap_enabled: bool,
}

impl Default for GridView {
    fn default() -> Self {
        GridView { mode: GridMode::Adaptive, triplet: false, snap_enabled: true }
    }
}

/// The division ladder, coarsest first: a bar of `bar_ticks`, then 1/2, 1/4 (beat), 1/8, 1/16,
/// 1/32, 1/64 and 1/128 note, keeping only entries finer than the one before (a short bar drops
/// the coarse notes).
pub fn ladder(bar_ticks: i64) -> Vec<i64> {
    let mut out: Vec<i64> = Vec::with_capacity(9);
    for t in [bar_ticks.max(1), PPQ * 2, PPQ, PPQ / 2, PPQ / 4, PPQ / 8, PPQ / 16, PPQ / 32] {
        if out.last().is_none_or(|l| t < *l) {
            out.push(t);
        }
    }
    out
}

/// A division with the triplet scaling applied (a 1/8 triplet is a third of a beat).
pub fn scaled(base: i64, triplet: bool) -> i64 {
    if triplet { (base * 2 / 3).max(1) } else { base.max(1) }
}

/// Ticks of the first bar (the grid's "bar" division).
pub fn bar_ticks(s: &Session) -> i64 {
    s.tempo.meter_at_tick(0).ticks_per_bar().max(1)
}

/// Points per tick at `spp` samples per point.
pub fn px_per_tick(s: &Session, spp: f64) -> f64 {
    let q = (s.tempo.tick_to_samples(PPQ, s.sample_rate) - s.tempo.tick_to_samples(0, s.sample_rate)) as f64;
    q / PPQ as f64 / spp.max(1e-6)
}

/// The adaptive base division: the finest ladder entry whose (scaled) spacing is at least
/// [`MIN_SPACING_PX`]; a bar when none is.
pub fn adaptive_base(px_per_tick: f64, bar_ticks: i64, triplet: bool) -> i64 {
    let l = ladder(bar_ticks);
    l.iter().rev().copied().find(|b| scaled(*b, triplet) as f64 * px_per_tick >= MIN_SPACING_PX).unwrap_or(l[0])
}

/// The base division in force (before the triplet scaling).
pub fn base(px_per_tick: f64, bar_ticks: i64, g: &GridView) -> i64 {
    match g.mode {
        GridMode::Adaptive => adaptive_base(px_per_tick, bar_ticks, g.triplet),
        GridMode::Fixed(t) => t.max(1),
    }
}

/// The grid step in force, in ticks.
pub fn ticks(px_per_tick: f64, bar_ticks: i64, g: &GridView) -> i64 {
    scaled(base(px_per_tick, bar_ticks, g), g.triplet)
}

/// Pin the grid one rung finer (`finer`) or coarser than the division now in force.
pub fn step(g: &mut GridView, px_per_tick: f64, bar_ticks: i64, finer: bool) {
    let cur = base(px_per_tick, bar_ticks, g);
    let l = ladder(bar_ticks);
    let next = if finer { l.iter().copied().find(|t| *t < cur) } else { l.iter().rev().copied().find(|t| *t > cur) };
    g.mode = GridMode::Fixed(next.unwrap_or(cur));
}

/// The step at which sub-beat lines are drawn: the grid step when it is finer than a beat and its
/// lines are at least [`MIN_DRAW_PX`] apart, otherwise none.
pub fn sub_line_step(step_ticks: i64, px_per_tick: f64, beat_ticks: i64) -> Option<i64> {
    (step_ticks < beat_ticks && step_ticks as f64 * px_per_tick >= MIN_DRAW_PX).then_some(step_ticks)
}

/// A division's name: "Bar", "1/2", "1/16", "1/8T".
pub fn name(base: i64, bar_ticks: i64, triplet: bool) -> String {
    let t = if triplet { "T" } else { "" };
    if base >= bar_ticks {
        return format!("Bar{t}");
    }
    format!("1/{}{t}", (PPQ * 4 / base.max(1)).max(1))
}

/// The grid as the ruler corner shows it: "Grid: 1/16", "Grid: 1/8T", "Adaptive 1/32", "Snap off".
pub fn label(s: &Session, spp: f64, g: &GridView) -> String {
    label_at(px_per_tick(s, spp), bar_ticks(s), g)
}

/// [`label`] for a view that already knows its points per tick (the piano roll).
pub fn label_at(ppt: f64, bar: i64, g: &GridView) -> String {
    if !g.snap_enabled {
        return "Snap off".into();
    }
    let n = name(base(ppt, bar, g), bar, g.triplet);
    match g.mode {
        GridMode::Adaptive => format!("Adaptive {n}"),
        GridMode::Fixed(_) => format!("Grid: {n}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BAR: i64 = PPQ * 4;
    fn adaptive() -> GridView {
        GridView::default()
    }

    #[test]
    fn the_ladder_runs_from_a_bar_down_to_a_128th() {
        assert_eq!(ladder(BAR), vec![3840, 1920, 960, 480, 240, 120, 60, 30]);
        // A 2/4 bar is a half note: no duplicate; 3/8 (1.5 beats) drops the half note too.
        assert_eq!(ladder(PPQ * 2), vec![1920, 960, 480, 240, 120, 60, 30]);
        assert_eq!(ladder(PPQ * 3 / 2)[..2], [1440, 960]);
    }

    #[test]
    fn the_division_follows_the_zoom_down_to_a_128th() {
        // Points per tick; a beat is 960 ticks.
        let at = |beat_px: f64| ticks(beat_px / PPQ as f64, BAR, &adaptive());
        assert_eq!(at(1.0), BAR, "zoomed far out: a bar");
        assert_eq!(at(12.0), PPQ, "a beat is 12 px: the beat");
        assert_eq!(at(11.9), PPQ * 2, "just under: the half note");
        assert_eq!(at(48.0), PPQ / 4, "1/16 at 12 px");
        assert_eq!(at(96.0), PPQ / 8, "1/32");
        assert_eq!(at(192.0), PPQ / 16, "1/64");
        assert_eq!(at(384.0), PPQ / 32, "1/128");
        assert_eq!(at(10_000.0), PPQ / 32, "zooming further in keeps 1/128");
        assert_eq!(at(1e9), PPQ / 32);
    }

    #[test]
    fn the_spacing_of_the_chosen_division_is_at_least_the_minimum() {
        for beat_px in [20.0, 50.0, 90.0, 140.0, 300.0, 700.0] {
            let ppt = beat_px / PPQ as f64;
            let t = ticks(ppt, BAR, &adaptive());
            assert!(t as f64 * ppt >= MIN_SPACING_PX, "{beat_px}: {t}");
        }
    }

    #[test]
    fn a_triplet_is_two_thirds_of_the_division() {
        assert_eq!(scaled(PPQ / 2, true), PPQ / 3, "a 1/8 triplet is a third of a beat");
        assert_eq!(scaled(PPQ, true), 640);
        assert_eq!(scaled(PPQ / 32, true), 20);
        let g = GridView { mode: GridMode::Fixed(PPQ / 2), triplet: true, ..adaptive() };
        assert_eq!(ticks(1.0, BAR, &g), 320);
        // Adaptive with triplets still subdivides to the 128th triplet and keeps 12 px.
        let t = GridView { triplet: true, ..adaptive() };
        assert_eq!(ticks(1000.0 / PPQ as f64, BAR, &t), 20);
        let ppt = 60.0 / PPQ as f64;
        let d = ticks(ppt, BAR, &t);
        assert!(d as f64 * ppt >= MIN_SPACING_PX);
    }

    #[test]
    fn a_fixed_grid_ignores_the_zoom() {
        let g = GridView { mode: GridMode::Fixed(PPQ / 4), ..adaptive() };
        for ppt in [1e-4, 0.01, 1.0, 100.0] {
            assert_eq!(ticks(ppt, BAR, &g), PPQ / 4);
        }
    }

    #[test]
    fn narrowing_and_widening_walk_the_ladder_from_the_current_division() {
        let mut g = adaptive();
        let ppt = 48.0 / PPQ as f64; // adaptive: 1/16
        step(&mut g, ppt, BAR, true);
        assert_eq!(g.mode, GridMode::Fixed(PPQ / 8), "one finer than 1/16");
        step(&mut g, ppt, BAR, false);
        step(&mut g, ppt, BAR, false);
        assert_eq!(g.mode, GridMode::Fixed(PPQ / 2), "two coarser than 1/32");
        for _ in 0..20 {
            step(&mut g, ppt, BAR, false);
        }
        assert_eq!(g.mode, GridMode::Fixed(BAR), "stops at a bar");
        for _ in 0..20 {
            step(&mut g, ppt, BAR, true);
        }
        assert_eq!(g.mode, GridMode::Fixed(PPQ / 32), "stops at a 128th");
    }

    #[test]
    fn sub_beat_lines_are_culled_below_four_points() {
        let ppt = |px_per_step: f64, step: i64| px_per_step / step as f64;
        assert_eq!(sub_line_step(PPQ / 4, ppt(4.0, PPQ / 4), PPQ), Some(PPQ / 4));
        assert_eq!(sub_line_step(PPQ / 4, ppt(3.9, PPQ / 4), PPQ), None);
        assert_eq!(sub_line_step(PPQ, ppt(100.0, PPQ), PPQ), None, "a beat is not a sub-beat line");
        assert_eq!(sub_line_step(BAR, 1.0, PPQ), None);
    }

    #[test]
    fn names_read_like_ableton() {
        assert_eq!(name(PPQ / 4, BAR, false), "1/16");
        assert_eq!(name(PPQ / 2, BAR, true), "1/8T");
        assert_eq!(name(PPQ, BAR, false), "1/4");
        assert_eq!(name(PPQ * 2, BAR, false), "1/2");
        assert_eq!(name(BAR, BAR, false), "Bar");
        assert_eq!(name(PPQ / 32, BAR, false), "1/128");
    }
}
