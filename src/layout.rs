//! The tray's tiling model and its pixel geometry.
//!
//! Two levels, deliberately: the tray is a row of columns, and each column is a
//! stack of slots. That covers what a tray is for while keeping splitter drags and
//! hit-testing trivial compared to a general split tree.
//!
//! Sizes are stored as weights, never pixels, so a Snap resize is a pure
//! recomputation and never accumulates rounding drift.
//!
//! Tiling is one of two arrangements. The default, [`Arrange::Free`], leaves every
//! pane at the size it had on the desktop and ignores the columns entirely.

use crate::window_manager::Snapshot;
use windows::Win32::Foundation::{HWND, POINT, RECT};

/// Smallest usable slot, in 96-dpi pixels; splitter drags clamp to this.
const MIN_SLOT_DIP: i32 = 90;

#[derive(Clone, Copy)]
pub struct Metrics {
    /// Height of the top strip. It doubles as the window's caption now that the
    /// system title bar is gone, so it also sizes the min/max/close buttons.
    pub toolbar: i32,
    pub header: i32,
    pub split: i32,
    pub pad: i32,
    pub close: i32,
    pub min_slot: i32,
    pub font_px: i32,
    /// Width of one caption button, matching the Windows 11 proportions.
    pub caption_w: i32,
}

impl Metrics {
    pub fn for_dpi(dpi: u32) -> Self {
        let s = |dip: i32| (dip * dpi as i32 + 48) / 96;
        Self {
            toolbar: s(36),
            header: s(24),
            split: s(6),
            pad: s(6),
            close: s(18),
            min_slot: s(MIN_SLOT_DIP),
            font_px: s(14),
            caption_w: s(46),
        }
    }
}

/// How the tray places its panes.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Arrange {
    /// Every pane keeps the size it had on the desktop and stays where it is put;
    /// resizing the tray only moves panes enough to keep them inside it.
    #[default]
    Free,
    /// Panes share the tray as a row of columns, resized along with it.
    Tile,
}

/// One adopted window plus what we need to give it back.
pub struct Pane {
    pub hwnd: HWND,
    pub title: String,
    pub snap: Snapshot,
    /// Header plus window, in tray client coordinates, for [`Arrange::Free`]. Kept
    /// while tiled too, so switching back puts every pane where it was.
    pub free: RECT,
    /// Stacking stamp for [`Arrange::Free`]; the highest is frontmost.
    pub z: u32,
}

pub struct Slot {
    pub weight: f32,
    pub pane: Pane,
}

pub struct Column {
    pub weight: f32,
    pub slots: Vec<Slot>,
}

/// The column structure is maintained in both arrangements — free placement simply
/// ignores it — so switching to tiling always has a sensible grid to fall back on.
#[derive(Default)]
pub struct Layout {
    pub cols: Vec<Column>,
    pub arrange: Arrange,
    next_z: u32,
}

/// Where a point landed. Toolbar buttons are hit-tested by the caller, which owns
/// their rects (they depend on measured text).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hit {
    None,
    /// Header strip of a slot — click focuses, drag reorders.
    Header(usize, usize),
    /// The × on a slot's header.
    Close(usize, usize),
    /// Vertical splitter to the right of column `n`.
    VSplit(usize),
    /// Horizontal splitter below slot `n` of column `c`.
    HSplit(usize, usize),
    /// The adopted window's own area; clicking it just focuses the pane.
    Client(usize, usize),
}

pub struct SlotGeom {
    pub full: RECT,
    pub header: RECT,
    pub close: RECT,
    pub client: RECT,
}

/// Cached pixel layout. Recomputed only when the size or the tiling changes,
/// then reused by painting, hit-testing and child placement.
#[derive(Default)]
pub struct Geom {
    pub slots: Vec<Vec<SlotGeom>>,
    pub vsplits: Vec<RECT>,
    pub hsplits: Vec<Vec<RECT>>,
    /// Every slot, back to front. Free panes overlap, so painting walks this
    /// forwards and hit-testing walks it backwards.
    pub order: Vec<(usize, usize)>,
}

impl Layout {
    pub fn pane_count(&self) -> usize {
        self.cols.iter().map(|c| c.slots.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.pane_count() == 0
    }

    pub fn panes(&self) -> impl Iterator<Item = &Pane> {
        self.cols.iter().flat_map(|c| c.slots.iter().map(|s| &s.pane))
    }

    pub fn contains(&self, hwnd: HWND) -> bool {
        self.panes().any(|p| p.hwnd == hwnd)
    }

    pub fn pane_at(&self, col: usize, slot: usize) -> Option<&Pane> {
        self.cols.get(col)?.slots.get(slot).map(|s| &s.pane)
    }

    /// Place a new pane so the grid stays roughly square: open a new column while
    /// there are fewer columns than the shortest column is deep, otherwise stack
    /// onto the shortest column.
    pub fn add(&mut self, mut pane: Pane) {
        // Newcomers land on top, as a freshly opened window would.
        self.next_z += 1;
        pane.z = self.next_z;

        let shortest = self
            .cols
            .iter()
            .enumerate()
            .min_by_key(|(_, c)| c.slots.len())
            .map(|(i, c)| (i, c.slots.len()));

        // New arrivals take the average weight of their peers rather than a flat
        // 1.0, so adding a pane never silently rescales splits the user set.
        match shortest {
            Some((idx, depth)) if self.cols.len() > depth => {
                let col = &mut self.cols[idx];
                let weight = mean(col.slots.iter().map(|s| s.weight));
                col.slots.push(Slot { weight, pane });
            }
            _ => {
                let weight = mean(self.cols.iter().map(|c| c.weight));
                self.cols.push(Column {
                    weight,
                    slots: vec![Slot { weight: 1.0, pane }],
                });
            }
        }
    }

    /// Detach a pane from the tiling and hand it back to the caller, collapsing
    /// the column if it just went empty.
    pub fn remove(&mut self, col: usize, slot: usize) -> Option<Pane> {
        let column = self.cols.get_mut(col)?;
        if slot >= column.slots.len() {
            return None;
        }
        let pane = column.slots.remove(slot).pane;
        if column.slots.is_empty() {
            self.cols.remove(col);
        }
        // Surviving weights are left alone; `distribute` renormalises by their sum,
        // so the remaining panes keep their relative proportions.
        Some(pane)
    }

    pub fn find(&self, hwnd: HWND) -> Option<(usize, usize)> {
        for (c, col) in self.cols.iter().enumerate() {
            for (s, slot) in col.slots.iter().enumerate() {
                if slot.pane.hwnd == hwnd {
                    return Some((c, s));
                }
            }
        }
        None
    }

    /// Exchange the *contents* of two slots. Weights stay with the slot, so the
    /// tiling keeps its shape and only the windows move.
    pub fn swap(&mut self, a: (usize, usize), b: (usize, usize)) {
        if a == b {
            return;
        }
        if a.0 == b.0 {
            let Some(col) = self.cols.get_mut(a.0) else {
                return;
            };
            let (lo, hi) = (a.1.min(b.1), a.1.max(b.1));
            if hi >= col.slots.len() {
                return;
            }
            let (head, tail) = col.slots.split_at_mut(hi);
            std::mem::swap(&mut head[lo].pane, &mut tail[0].pane);
        } else {
            let (first, second) = if a.0 < b.0 { (a, b) } else { (b, a) };
            if second.0 >= self.cols.len() {
                return;
            }
            let (head, tail) = self.cols.split_at_mut(second.0);
            let (Some(pa), Some(pb)) = (
                head[first.0].slots.get_mut(first.1),
                tail[0].slots.get_mut(second.1),
            ) else {
                return;
            };
            std::mem::swap(&mut pa.pane, &mut pb.pane);
        }
    }

    /// Reset every weight so all columns and slots share space equally.
    pub fn rebalance(&mut self) {
        for col in &mut self.cols {
            col.weight = 1.0;
            for slot in &mut col.slots {
                slot.weight = 1.0;
            }
        }
    }

    /// Bring a pane to the front of the free stacking order. Returns false when it
    /// was already there, so the caller can skip a repaint.
    pub fn raise(&mut self, hwnd: HWND) -> bool {
        let top = self.panes().map(|p| p.z).max().unwrap_or(0);
        let Some((c, s)) = self.find(hwnd) else {
            return false;
        };
        let pane = &mut self.cols[c].slots[s].pane;
        if pane.z == top {
            return false;
        }
        self.next_z += 1;
        pane.z = self.next_z;
        true
    }

    /// Move a free pane so its top-left corner sits at `(left, top)`, kept inside `area`.
    pub fn move_free(&mut self, hwnd: HWND, left: i32, top: i32, area: RECT) {
        let Some((c, s)) = self.find(hwnd) else {
            return;
        };
        let pane = &mut self.cols[c].slots[s].pane;
        let r = pane.free;
        pane.free = clamp_into(
            RECT {
                left,
                top,
                right: left + (r.right - r.left),
                bottom: top + (r.bottom - r.top),
            },
            area,
        );
    }

    /// Compute pixel rects for `area` (the client area below the toolbar).
    pub fn geometry(&self, area: RECT, m: &Metrics) -> Geom {
        match self.arrange {
            Arrange::Tile => self.tile_geometry(area, m),
            Arrange::Free => self.free_geometry(area, m),
        }
    }

    /// Free panes are clamped into `area` here rather than when the tray resizes, so
    /// shrinking the tray and growing it back returns every pane to where it was.
    fn free_geometry(&self, area: RECT, m: &Metrics) -> Geom {
        let mut geom = Geom::default();
        let mut order = Vec::new();
        for (ci, col) in self.cols.iter().enumerate() {
            let mut col_slots = Vec::with_capacity(col.slots.len());
            for (si, slot) in col.slots.iter().enumerate() {
                col_slots.push(slot_geom(clamp_into(slot.pane.free, area), m));
                order.push((slot.pane.z, (ci, si)));
            }
            geom.slots.push(col_slots);
            geom.hsplits.push(Vec::new());
        }
        order.sort_by_key(|(z, _)| *z);
        geom.order = order.into_iter().map(|(_, at)| at).collect();
        geom
    }

    fn tile_geometry(&self, area: RECT, m: &Metrics) -> Geom {
        let mut geom = Geom::default();
        if self.cols.is_empty() {
            return geom;
        }

        let widths: Vec<f32> = self.cols.iter().map(|c| c.weight).collect();
        let xs = distribute(area.left, area.right - area.left, &widths, m.split);

        for (ci, (x, w)) in xs.iter().copied().enumerate() {
            let col = &self.cols[ci];
            if ci + 1 < xs.len() {
                geom.vsplits.push(RECT {
                    left: x + w,
                    top: area.top,
                    right: x + w + m.split,
                    bottom: area.bottom,
                });
            }

            let heights: Vec<f32> = col.slots.iter().map(|s| s.weight).collect();
            let ys = distribute(area.top, area.bottom - area.top, &heights, m.split);

            let mut col_slots = Vec::with_capacity(ys.len());
            let mut col_splits = Vec::new();
            for (si, (y, h)) in ys.iter().copied().enumerate() {
                col_slots.push(slot_geom(
                    RECT {
                        left: x,
                        top: y,
                        right: x + w,
                        bottom: y + h,
                    },
                    m,
                ));
                geom.order.push((ci, si));
                if si + 1 < ys.len() {
                    col_splits.push(RECT {
                        left: x,
                        top: y + h,
                        right: x + w,
                        bottom: y + h + m.split,
                    });
                }
            }
            geom.slots.push(col_slots);
            geom.hsplits.push(col_splits);
        }
        geom
    }

    /// Move the splitter to the right of column `col` so it sits at `x`,
    /// shifting weight between that column and its neighbour only.
    pub fn drag_vsplit(&mut self, col: usize, x: i32, area: RECT, m: &Metrics) {
        if col + 1 >= self.cols.len() {
            return;
        }
        let widths: Vec<f32> = self.cols.iter().map(|c| c.weight).collect();
        let xs = distribute(area.left, area.right - area.left, &widths, m.split);
        let (x0, _) = xs[col];
        let (x1, w1) = xs[col + 1];
        let span = (x1 + w1) - x0;
        let left = (x - x0).clamp(m.min_slot, (span - m.min_slot - m.split).max(m.min_slot));

        let pair = self.cols[col].weight + self.cols[col + 1].weight;
        let frac = left as f32 / (span - m.split).max(1) as f32;
        self.cols[col].weight = pair * frac;
        self.cols[col + 1].weight = pair * (1.0 - frac);
    }

    /// Same as [`Layout::drag_vsplit`], for the splitter below slot `slot` of column `col`.
    pub fn drag_hsplit(&mut self, col: usize, slot: usize, y: i32, area: RECT, m: &Metrics) {
        let Some(column) = self.cols.get_mut(col) else {
            return;
        };
        if slot + 1 >= column.slots.len() {
            return;
        }
        let heights: Vec<f32> = column.slots.iter().map(|s| s.weight).collect();
        let ys = distribute(area.top, area.bottom - area.top, &heights, m.split);
        let (y0, _) = ys[slot];
        let (y1, h1) = ys[slot + 1];
        let span = (y1 + h1) - y0;
        let top = (y - y0).clamp(m.min_slot, (span - m.min_slot - m.split).max(m.min_slot));

        let pair = column.slots[slot].weight + column.slots[slot + 1].weight;
        let frac = top as f32 / (span - m.split).max(1) as f32;
        column.slots[slot].weight = pair * frac;
        column.slots[slot + 1].weight = pair * (1.0 - frac);
    }
}

impl Geom {
    pub fn hit_test(&self, pt: POINT) -> Hit {
        for (ci, split) in self.vsplits.iter().enumerate() {
            if contains(split, pt) {
                return Hit::VSplit(ci);
            }
        }
        for (ci, splits) in self.hsplits.iter().enumerate() {
            for (si, split) in splits.iter().enumerate() {
                if contains(split, pt) {
                    return Hit::HSplit(ci, si);
                }
            }
        }
        // Front to back, so the pane on top wins where free panes overlap.
        for &(ci, si) in self.order.iter().rev() {
            let Some(g) = self.slots.get(ci).and_then(|col| col.get(si)) else {
                continue;
            };
            if contains(&g.close, pt) {
                return Hit::Close(ci, si);
            }
            if contains(&g.header, pt) {
                return Hit::Header(ci, si);
            }
            if contains(&g.full, pt) {
                return Hit::Client(ci, si);
            }
        }
        Hit::None
    }
}

pub fn contains(r: &RECT, p: POINT) -> bool {
    p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom
}

pub fn intersects(a: &RECT, b: &RECT) -> bool {
    a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom
}

/// Header, close button and window area for a slot occupying `full`.
fn slot_geom(full: RECT, m: &Metrics) -> SlotGeom {
    let header = RECT {
        bottom: (full.top + m.header).min(full.bottom),
        ..full
    };
    let close = RECT {
        left: (header.right - m.close - m.pad).max(header.left),
        top: header.top + (m.header - m.close) / 2,
        right: (header.right - m.pad).max(header.left),
        bottom: header.top + (m.header - m.close) / 2 + m.close,
    };
    let client = RECT {
        top: header.bottom,
        ..full
    };
    SlotGeom {
        full,
        header,
        close,
        client,
    }
}

/// Slide `r` (without resizing it) until it lies inside `area`. When it is bigger
/// than `area`, the top-left corner wins, since that is where the header and its
/// handle live.
fn clamp_into(r: RECT, area: RECT) -> RECT {
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    let left = r.left.min(area.right - w).max(area.left);
    let top = r.top.min(area.bottom - h).max(area.top);
    RECT {
        left,
        top,
        right: left + w,
        bottom: top + h,
    }
}

/// Split `total` px across `weights`, leaving `gap` px between neighbours.
///
/// Positions are derived from a running cumulative fraction rather than by summing
/// rounded sizes, so the last item always lands exactly on the far edge.
fn distribute(start: i32, total: i32, weights: &[f32], gap: i32) -> Vec<(i32, i32)> {
    let n = weights.len();
    if n == 0 {
        return Vec::new();
    }
    let content = (total - gap * (n as i32 - 1)).max(0);
    let sum: f32 = weights.iter().sum::<f32>().max(f32::EPSILON);

    let mut out = Vec::with_capacity(n);
    let mut acc = 0.0f32;
    let mut prev = 0i32;
    for (i, w) in weights.iter().enumerate() {
        acc += w / sum;
        let end = if i + 1 == n {
            content
        } else {
            (acc * content as f32).round() as i32
        };
        out.push((start + prev + gap * i as i32, (end - prev).max(0)));
        prev = end;
    }
    out
}

fn mean(weights: impl Iterator<Item = f32>) -> f32 {
    let (sum, n) = weights.fold((0.0f32, 0usize), |(s, n), w| (s + w, n + 1));
    if n == 0 { 1.0 } else { sum / n as f32 }
}
