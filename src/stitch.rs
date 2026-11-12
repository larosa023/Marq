//! Lining up frames for scrolling capture.
//!
//! Each row of a frame is summarized as a strip of BANDS brightness values (the row
//! split into BANDS equal parts, averaged). When the page scrolls by `dy` rows, row `r`
//! of the new frame looks like row `r + dy` of the previous one. We look for the `dy`
//! whose rows differ the least.
//!
//! Comparing averages instead of exact pixels is what makes this work on real pages:
//! browsers scroll by fractions of a pixel at 125%/150% scaling, re-rasterize text
//! slightly differently, and animate scrolling, so exact rows rarely match. Blank rows
//! carry no information and are ignored; rows that don't move at the top or bottom
//! (sticky headers, frozen panes, status bars) are left out of the comparison.

pub const BANDS: usize = 32;

/// Average difference per band (0–255) below which two rows count as the same. Generous:
/// browsers redraw pinned text slightly differently at 125%/150% scaling.
const SAME_ROW: u32 = 6;
/// The best offset must have an average difference below this to be trusted...
const MAX_COST: f32 = 10.0;
/// ...and be clearly better than any offset that isn't a near neighbour of it.
const CLEAR_WIN: f32 = 0.6;
/// Rows next to pinned areas are blurred together with them; skip a few more.
const EDGE_PAD: usize = 4;
/// When the hint has to break a tie, the pick must still be a good match on its own.
const HINT_COST: f32 = 4.0;

#[derive(Clone)]
pub struct Sig {
    pub rows: Vec<[u8; BANDS]>,
    pub detail: Vec<bool>,
}

impl Sig {
    pub fn h(&self) -> usize {
        self.rows.len()
    }
}

/// Summarize a frame (top-down BGRA pixels).
pub fn signature(px: &[u32], w: usize, h: usize) -> Sig {
    let mut rows = Vec::with_capacity(h);
    let mut detail = Vec::with_capacity(h);
    let w = w.max(1);
    for y in 0..h {
        let row = &px[y * w..(y + 1) * w];
        let mut bands = [0u8; BANDS];
        let mut lo = 255u8;
        let mut hi = 0u8;
        let mut min_px = 255u32;
        let mut max_px = 0u32;
        for (b, out) in bands.iter_mut().enumerate() {
            let x0 = b * w / BANDS;
            let x1 = ((b + 1) * w / BANDS).max(x0 + 1).min(w);
            let mut sum = 0u32;
            for &c in &row[x0..x1] {
                let l = (((c >> 16) & 0xFF) * 77 + ((c >> 8) & 0xFF) * 150 + (c & 0xFF) * 29) >> 8;
                sum += l;
                min_px = min_px.min(l);
                max_px = max_px.max(l);
            }
            let v = (sum / (x1 - x0) as u32) as u8;
            *out = v;
            lo = lo.min(v);
            hi = hi.max(v);
        }
        rows.push(bands);
        // A row is informative if it has visible detail anywhere (text, lines, images).
        detail.push(hi - lo > 6 || max_px - min_px > 40);
    }
    // Soften each band vertically ([1 2 1] twice): a page moved by a fraction of a
    // pixel then looks almost the same, while different content still looks different.
    for _ in 0..2 {
        let src = rows.clone();
        for r in 0..h {
            let up = &src[r.saturating_sub(1)];
            let mid = &src[r];
            let dn = &src[(r + 1).min(h - 1)];
            for b in 0..BANDS {
                rows[r][b] = ((up[b] as u32 + 2 * mid[b] as u32 + dn[b] as u32 + 2) / 4) as u8;
            }
        }
    }
    Sig { rows, detail }
}

fn row_cost(a: &[u8; BANDS], b: &[u8; BANDS]) -> u32 {
    a.iter().zip(b.iter()).map(|(x, y)| (*x as i32 - *y as i32).unsigned_abs()).sum()
}

/// Bands that count in `cost_at`: the one that differs most over the whole frame is
/// left out, so a page's own scroll bar (or an animation in one spot) can't spoil a match.
const USED_BANDS: f32 = (BANDS - 1) as f32;

fn same(a: &[u8; BANDS], b: &[u8; BANDS]) -> bool {
    row_cost(a, b) <= SAME_ROW * BANDS as u32
}

/// Rows at the top that are the same in both frames (sticky header, frozen panes).
pub fn still_top(prev: &Sig, next: &Sig) -> usize {
    let h = prev.h().min(next.h());
    (0..h / 2).take_while(|&r| same(&prev.rows[r], &next.rows[r])).count()
}

/// Rows at the bottom that are the same in both frames (footer, status bar, sheet tabs).
pub fn still_bottom(prev: &Sig, next: &Sig) -> usize {
    let h = prev.h().min(next.h());
    (0..h / 3).take_while(|&i| same(&prev.rows[h - 1 - i], &next.rows[h - 1 - i])).count()
}

/// Mean per-band difference between `next` rows and `prev` rows shifted by `dy`,
/// over informative rows in `top..bottom`, sampling every `step`-th row.
fn cost_at(prev: &Sig, next: &Sig, dy: usize, top: usize, bottom: usize, step: usize, skip: Skip) -> Option<f32> {
    cost_in(prev, next, dy, top, bottom, step, skip)
        // Leaving out the rows by the pointer can leave too little to go on.
        .or_else(|| skip.and_then(|_| cost_in(prev, next, dy, top, bottom, step, None)))
        .or_else(|| {
        // Rows that look pinned can leave too little to compare: on a spreadsheet, blank
        // grid rows look the same after scrolling by whole rows. Compare everything then.
        let (t, b) = (EDGE_PAD.min(top), next.h() - EDGE_PAD.min(next.h() - bottom));
        if (t, b) != (top, bottom) {
            cost_in(prev, next, dy, t, b, step, None)
        } else {
            None
        }
    })
}

fn cost_in(prev: &Sig, next: &Sig, dy: usize, top: usize, bottom: usize, step: usize, skip: Skip) -> Option<f32> {
    // Both rows compared must lie in the free (non-pinned) part: next row r, prev row r+dy.
    let end = bottom.saturating_sub(dy);
    if end <= top {
        return None;
    }
    let mut per_band = [0u32; BANDS];
    let mut n = 0u32;
    let mut r = top;
    while r < end {
        let hidden = skip.is_some_and(|(a, b)| (a..b).contains(&r) || (a..b).contains(&(r + dy)));
        if !hidden && (next.detail[r] || prev.detail[r + dy]) {
            let (a, b) = (&next.rows[r], &prev.rows[r + dy]);
            for k in 0..BANDS {
                per_band[k] += (a[k] as i32 - b[k] as i32).unsigned_abs();
            }
            n += 1;
        }
        r += step;
    }
    let sum = per_band.iter().sum::<u32>() - per_band.iter().max().copied().unwrap_or(0);
    // Enough overlap to be sure: a few lines lining up by chance prove nothing.
    let min_rows = if step == 1 { (next.h() / 12).max(6) } else { (next.h() / 36).max(2) } as u32;
    if n < min_rows {
        return None;
    }
    Some(sum as f32 / (n as f32 * USED_BANDS))
}

/// How many rows the content moved up between `prev` and `next` (0 = it didn't move).
/// None if they don't line up convincingly (scrolled too far, or the page changed).
pub fn find_offset(prev: &Sig, next: &Sig, last_dy: usize) -> Option<usize> {
    find_offset_near(prev, next, last_dy, None, None)
}

/// Rows `a..b` of the area to leave out of comparisons: where the pointer sits, the
/// page lights up whatever is under it (hover), in every frame, wherever it scrolled.
pub type Skip = Option<(usize, usize)>;

/// `find_offset`, with a rough idea of how far the page should have moved (`hint`).
/// On pages that repeat themselves (spreadsheet rows, lists of look-alike items),
/// several offsets can look equally good; the hint picks the right one.
pub fn find_offset_near(prev: &Sig, next: &Sig, last_dy: usize, hint: Option<usize>, skip: Skip) -> Option<usize> {
    match_offset(prev, next, last_dy, hint, skip)
}

fn match_offset(prev: &Sig, next: &Sig, last_dy: usize, hint: Option<usize>, skip: Skip) -> Option<usize> {
    let h = prev.h();
    if h < 8 || next.h() != h {
        return None;
    }
    // Leave out rows that are pinned in place, plus a small margin at each edge.
    let top = (still_top(prev, next) + EDGE_PAD).min(h / 3);
    let bottom = h - (still_bottom(prev, next) + EDGE_PAD).min(h / 4);
    if bottom <= top + 4 {
        return Some(0); // everything that can be compared is unchanged
    }
    let max_dy = h * 9 / 10;

    // Coarse pass on every 3rd row, then a precise look at the best few candidates.
    let mut coarse: Vec<(f32, usize)> = (0..=max_dy)
        .filter_map(|dy| cost_at(prev, next, dy, top, bottom, 3, skip).map(|c| (c, dy)))
        .collect();
    if coarse.is_empty() {
        // Nothing informative to compare (e.g. a blank area): treat as unchanged
        // if it really is the same, otherwise unknown.
        return cost_at(prev, next, 0, 0, h, 1, skip).filter(|c| *c <= 1.0).map(|_| 0).or(None);
    }
    coarse.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let lead = coarse[0].1;
    // The best coarse score among offsets that aren't neighbours of the leader.
    let rival = coarse.iter().find(|c| c.1.abs_diff(lead) > 3).map(|c| c.0);
    let mut cands: Vec<usize> = coarse.iter().take(6).map(|c| c.1).collect();
    if !cands.contains(&0) {
        cands.push(0);
    }
    if last_dy > 0 && last_dy <= max_dy && !cands.contains(&last_dy) {
        cands.push(last_dy);
    }
    let tol = hint.map_or(0, |h| (h / 5).max(10));
    if let Some(hint) = hint.filter(|&h| h <= max_dy) {
        // The best few coarse candidates around the expected offset, and their neighbours.
        let near: Vec<usize> = coarse.iter().filter(|c| c.1.abs_diff(hint) <= tol).take(4).map(|c| c.1).collect();
        for c in near.into_iter().chain([hint]) {
            for d in c.saturating_sub(1)..=(c + 1).min(max_dy) {
                if !cands.contains(&d) {
                    cands.push(d);
                }
            }
        }
    }
    // Coarse sampling can land a row off; look at the leaders' neighbours too.
    for c in coarse.iter().take(3).map(|c| c.1).collect::<Vec<_>>() {
        for d in c.saturating_sub(1)..=(c + 1).min(max_dy) {
            if !cands.contains(&d) {
                cands.push(d);
            }
        }
    }
    let mut fine: Vec<(f32, usize)> = cands
        .iter()
        .filter_map(|&dy| cost_at(prev, next, dy, top, bottom, 1, skip).map(|c| (c, dy)))
        .collect();
    fine.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let &(best_cost, best_dy) = fine.first()?;
    if best_cost > MAX_COST {
        return None;
    }
    // Rival among the precise scores too: the best one that isn't a near neighbour.
    let fine_rival = fine.iter().find(|f| f.1.abs_diff(best_dy) > 3).map(|f| f.0);
    let rival = match (rival, fine_rival) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    if let Some(r) = rival {
        if best_cost > r * CLEAR_WIN && best_cost > 0.3 {
            // Ambiguous: several offsets look about as good. With a hint, take the
            // good-enough one closest to where the page should be.
            let hint = hint?;
            let ok = |f: &&(f32, usize)| f.0 <= HINT_COST && f.0 <= best_cost * 2.0 + 0.5 && f.1.abs_diff(hint) <= tol;
            let pick = fine.iter().filter(ok).min_by_key(|f| f.1.abs_diff(hint))?;
            // Only if it's clearly the closest: another good candidate about as near
            // the hint means we can't tell which one it is.
            let d1 = pick.1.abs_diff(hint);
            let second = fine.iter().filter(ok).filter(|f| f.1.abs_diff(pick.1) > 3).map(|f| f.1.abs_diff(hint)).min();
            return match second {
                Some(d2) if d2 <= d1 * 2 + 4 => None,
                _ => Some(pick.1),
            };
        }
    }
    // Prefer "didn't move" when it's about as good (a blinking caret, a hover effect).
    if let Some(&(c0, _)) = fine.iter().find(|f| f.1 == 0) {
        if c0 <= best_cost + 0.6 {
            return Some(0);
        }
    }
    Some(best_dy)
}

// ---------------------------------------------------------------------------
// The scrolling-capture controller: frames in, wheel steps and a tall image out.
// Pure logic (no Windows calls) so it can be tested against recorded pages.
// ---------------------------------------------------------------------------

/// Wheel amount per step (120 = one notch). Adapted so each step moves ~45% of the area.
const STEP_START: i32 = 120;
const STEP_MIN: i32 = 20;
const STEP_MAX: i32 = 2400;
/// Judge a step no sooner than this many frames after it (smooth scrolling starts late)...
const SETTLE_FRAMES: u32 = 2;
/// ...and stop waiting for the page to sit still after this many (animated pages).
const MAX_SETTLE_FRAMES: u32 = 14;
/// Wheel sent with no movement before calling it the end of the page. Some apps (Excel)
/// only move once a whole notch has added up, so small steps alone don't count.
const END_UNITS: i32 = 240;
/// Times in a row we may lose track (and step back) before handing control to the user.
const MAX_LOSSES: u32 = 4;
/// Stop growing past this height (keeps memory and the final PNG reasonable)...
pub const MAX_HEIGHT: usize = 30_000;
/// ...and past this many pixels (~160 MB), whichever comes first.
pub const MAX_PIXELS: usize = 40_000_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Starting,
    Scrolling,
    /// The pointer is off the area: the user has control.
    Paused,
    /// The page is above where the image ends (the user scrolled up).
    WentUp,
    /// Couldn't line up the page with the image; waiting for the user to help.
    Lost,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Wait,
    /// Turn the wheel by this much (positive = down, 120 = one notch).
    Wheel(i32),
    /// Done: the end of the page, or the image is as big as it may get.
    Finish,
}

pub struct Scroller {
    w: usize,
    h: usize,
    stitched: Vec<u32>,
    rows: usize,
    /// The frame the image currently ends with.
    anchor: Option<Sig>,
    /// The previous frame, to tell when the page has stopped moving.
    prev: Option<Sig>,
    last_dy: usize,
    /// Rows pinned at the bottom (footer, status bar, sheet tabs), added once at the end.
    footer: Vec<u32>,
    footer_rows: usize,
    frames: u32,
    status: Status,
    // Auto-scroll.
    step: i32,
    waiting: bool,
    since_send: u32,
    /// Wheel sent since the anchor frame, and how many pixels one unit moves the page.
    units: i32,
    px_per_unit: Option<f32>,
    still_units: i32,
    /// Largest step that hasn't overshot (an app may jump further than expected).
    max_step: i32,
    stuck: u32,
    losses: u32,
}

impl Scroller {
    pub fn new(w: usize, h: usize) -> Scroller {
        Scroller {
            w,
            h,
            stitched: Vec::new(),
            rows: 0,
            anchor: None,
            prev: None,
            last_dy: 0,
            footer: Vec::new(),
            footer_rows: 0,
            frames: 0,
            status: Status::Starting,
            step: STEP_START,
            waiting: false,
            since_send: 0,
            units: 0,
            px_per_unit: None,
            still_units: 0,
            max_step: STEP_MAX,
            stuck: 0,
            losses: 0,
        }
    }

    pub fn status(&self) -> Status {
        self.status
    }

    pub fn w(&self) -> usize {
        self.w
    }

    /// Height of the image so far.
    pub fn rows(&self) -> usize {
        self.rows + self.footer_rows
    }

    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    /// The finished image (top-down BGRA) and its height.
    pub fn into_image(mut self) -> (Vec<u32>, usize) {
        let rows = self.rows + self.footer.len() / self.w.max(1);
        self.stitched.extend_from_slice(&self.footer);
        (self.stitched, rows)
    }

    /// Add the rows that scrolled into view: `dy` rows, above any pinned footer.
    fn append(&mut self, px: &[u32], sig: &Sig, dy: usize) {
        let (w, h) = (self.w, self.h);
        if let Some(anchor) = &self.anchor {
            let still = still_bottom(anchor, sig);
            if self.footer_rows == 0 && still >= 4 && still + dy < h && self.rows >= still {
                // First time we see a pinned footer: take it off the image for now.
                self.footer_rows = still;
                self.footer = self.stitched.split_off((self.rows - still) * w);
                self.rows -= still;
            }
        }
        let f = self.footer_rows.min(h - dy);
        self.stitched.extend_from_slice(&px[(h - f - dy) * w..(h - f) * w]);
        self.rows += dy;
        self.last_dy = dy;
    }

    fn full(&self) -> bool {
        self.rows >= MAX_HEIGHT || self.rows * self.w >= MAX_PIXELS
    }

    fn send(&mut self, amount: i32) -> Action {
        self.waiting = true;
        self.since_send = 0;
        self.units += amount;
        Action::Wheel(amount)
    }

    /// Feed the next grab of the area (about every 90 ms), with where the pointer is
    /// inside the area (None when it's off the area, which pauses auto-scroll).
    pub fn feed(&mut self, px: Vec<u32>, pointer: Option<(usize, usize)>) -> Action {
        let (w, h) = (self.w, self.h);
        let pointer_inside = pointer.is_some();
        // Rows around the pointer: hover highlights and tooltips live there.
        // Small areas need every row they have, and are less likely to be hovered.
        let skip = pointer.filter(|_| h >= 240).map(|(_, y)| {
            let r = h / 10;
            (y.saturating_sub(r), (y + r).min(h))
        });
        if h < 8 || px.len() < w * h {
            return Action::Wait;
        }
        self.frames += 1;
        let sig = signature(&px, w, h);
        let still = self.prev.as_ref().is_some_and(|p| find_offset(p, &sig, 0) == Some(0));
        self.prev = Some(sig.clone());

        let Some(anchor) = self.anchor.as_ref() else {
            self.stitched = px;
            self.rows = h;
            self.anchor = Some(sig);
            return Action::Wait;
        };

        // Very start: the first grab may have caught a hover effect or a late repaint
        // (the pointer was just moved onto the area). Start over from a settled frame.
        if self.frames <= 4 && self.units == 0 && self.rows == h {
            if still && find_offset(anchor, &sig, 0) != Some(0) {
                self.stitched = px;
                self.anchor = Some(sig);
                return Action::Wait;
            }
            if !still {
                return Action::Wait;
            }
        }

        if !pointer_inside || self.status == Status::Lost {
            // The user is in control: follow along frame by frame, however they scroll.
            let off = find_offset_near(anchor, &sig, self.last_dy, None, skip);
            let went_up = off.is_none() && find_offset_near(&sig, anchor, 0, None, skip).is_some();
            match off {
                Some(0) => {
                    self.anchor = Some(sig);
                    if self.status != Status::Lost {
                        self.status = Status::Paused;
                    }
                }
                Some(dy) => {
                    self.append(&px, &sig, dy);
                    self.anchor = Some(sig);
                    self.status = if pointer_inside { Status::Scrolling } else { Status::Paused };
                    self.losses = 0;
                    self.waiting = false;
                    self.units = 0;
                }
                None => {
                    if self.status != Status::Lost {
                        self.status = if went_up { Status::WentUp } else { Status::Paused };
                    }
                }
            }
            if self.full() {
                return Action::Finish;
            }
            // Back on track (or still paused): auto-scroll picks up from the next frame.
            self.waiting = false;
            self.units = 0;
            return Action::Wait;
        }

        if self.status == Status::Paused || self.status == Status::Starting || self.status == Status::WentUp {
            self.status = Status::Scrolling;
        }

        if self.waiting {
            self.since_send += 1;
            let settled = (self.since_send >= SETTLE_FRAMES && still) || self.since_send >= MAX_SETTLE_FRAMES;
            if !settled {
                return Action::Wait;
            }
            self.waiting = false;
            let anchor = self.anchor.as_ref().unwrap();
            let hint = self.px_per_unit.filter(|_| self.units > 0).map(|r| (r * self.units as f32).round() as usize);
            match find_offset_near(anchor, &sig, self.last_dy, hint, skip) {
                Some(0) => {
                    self.anchor = Some(sig);
                    if self.units > 0 {
                        self.still_units = self.units; // all sent since it last moved
                        self.stuck += 1;
                        if self.stuck >= 2 && self.still_units >= END_UNITS {
                            return Action::Finish; // the end of the page
                        }
                    } else {
                        self.still_units = 0;
                        self.stuck = 0;
                    }
                    // Keep counting the wheel sent: an app that scrolls by whole notches
                    // (Excel) moves once enough has added up, and that move is for all of it.
                }
                Some(dy) => {
                    if self.units > 0 {
                        let r = dy as f32 / self.units as f32;
                        self.px_per_unit = Some(match self.px_per_unit {
                            Some(old) => old * 0.3 + r * 0.7,
                            None => r,
                        });
                        // Aim for ~45% of the area per step: plenty of overlap to line up.
                        let target = (h as f32 * 0.45).max(8.0);
                        self.step = ((target / r.max(1e-3)).round() as i32).clamp(STEP_MIN, self.max_step.max(STEP_MIN));
                    }
                    self.append(&px, &sig, dy);
                    self.anchor = Some(sig);
                    self.units = 0;
                    self.stuck = 0;
                    self.still_units = 0;
                    self.losses = 0;
                    if self.full() {
                        return Action::Finish;
                    }
                }
                None => {
                    let anchor = self.anchor.as_ref().unwrap();
                    if find_offset_near(&sig, anchor, 0, None, skip).is_some() {
                        // Above the end of the image (overshot a step back): carry on down.
                    } else {
                        // Scrolled too far, or the page changed: step back up into the
                        // overlap and continue with smaller steps.
                        self.losses += 1;
                        if self.losses > MAX_LOSSES {
                            self.status = Status::Lost;
                            self.units = 0;
                            return Action::Wait;
                        }
                        if self.units > 0 {
                            self.max_step = self.max_step.min(self.units * 3 / 5).max(STEP_MIN);
                        }
                        self.step = (self.step / 2).clamp(STEP_MIN, self.max_step);
                        // At least a whole notch: some apps (Excel) ignore anything less.
                        let back = -self.step.max(self.units / 2).max(120);
                        return self.send(back);
                    }
                }
            }
        }
        let step = self.step;
        self.send(step)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic "document": lines of dark text blocks of varying length on white,
    /// a few images and rules, with blank gaps between lines — like a real page.
    fn document(w: usize, h: usize, seed: u64) -> Vec<f32> {
        let mut s = seed;
        let mut rnd = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        let mut img = vec![250.0f32; w * h];
        let mut y = 4;
        while y + 20 < h {
            let kind = rnd() % 10;
            if kind == 0 {
                // An image block.
                let bh = 30 + (rnd() % 60) as usize;
                let x0 = (rnd() % (w as u64 / 2)) as usize;
                let x1 = (x0 + w / 3).min(w);
                for yy in y..(y + bh).min(h) {
                    for xx in x0..x1 {
                        img[yy * w + xx] = ((xx * 7 + yy * 13) % 200) as f32 + 20.0;
                    }
                }
                y += bh + 8;
            } else {
                // A line of words.
                let lh = 10 + (rnd() % 6) as usize;
                let mut x = 6;
                let line_end = w - (rnd() % (w as u64 / 3)) as usize;
                while x + 10 < line_end {
                    let word = 8 + (rnd() % 50) as usize;
                    for yy in y..(y + lh).min(h) {
                        for xx in x..(x + word).min(w) {
                            if (xx + yy * 3) % 5 != 0 {
                                img[yy * w + xx] = 40.0 + ((xx ^ yy) % 30) as f32;
                            }
                        }
                    }
                    x += word + 6;
                }
                y += lh + 6 + (rnd() % 10) as usize;
            }
        }
        img
    }

    /// The view of the document at a (possibly fractional) scroll position, with
    /// optional noise, as BGRA pixels.
    fn view(doc: &[f32], w: usize, top: f32, h: usize, noise: u32) -> Vec<u32> {
        let mut out = Vec::with_capacity(w * h);
        let rows = doc.len() / w;
        let mut s: u32 = (top * 1000.0) as u32 | 1;
        for y in 0..h {
            let fy = top + y as f32;
            let y0 = (fy.floor() as usize).min(rows - 1);
            let y1 = (y0 + 1).min(rows - 1);
            let t = fy - fy.floor();
            for x in 0..w {
                let mut v = doc[y0 * w + x] * (1.0 - t) + doc[y1 * w + x] * t;
                if noise > 0 {
                    s ^= s << 13;
                    s ^= s >> 17;
                    s ^= s << 5;
                    v += (s % (2 * noise + 1)) as f32 - noise as f32;
                }
                let g = v.clamp(0.0, 255.0) as u32;
                out.push(0xFF00_0000 | (g << 16) | (g << 8) | g);
            }
        }
        out
    }

    #[test]
    fn exact_scrolls() {
        let (w, h) = (300, 240);
        let doc = document(w, 3000, 7);
        let a = signature(&view(&doc, w, 0.0, h, 0), w, h);
        for dy in [1usize, 9, 40, 120, 200] {
            let b = signature(&view(&doc, w, dy as f32, h, 0), w, h);
            assert_eq!(find_offset(&a, &b, 0), Some(dy), "dy={dy}");
        }
        assert_eq!(find_offset(&a, &a, 0), Some(0));
    }

    #[test]
    fn fractional_scrolls_and_noise_like_a_browser() {
        let (w, h) = (360, 300);
        let doc = document(w, 4000, 11);
        let mut top = 0.0f32;
        let mut prev = signature(&view(&doc, w, top, h, 2), w, h);
        let mut total = 0usize;
        for step in [33.4f32, 47.7, 12.5, 90.25, 60.6, 100.33, 5.5] {
            top += step;
            let next = signature(&view(&doc, w, top, h, 2), w, h);
            let dy = find_offset(&prev, &next, 0).expect("should line up");
            total += dy;
            assert!((total as f32 - top).abs() <= 1.5, "drift: total {total} vs {top}");
            prev = next;
        }
    }

    #[test]
    fn no_movement_with_blinking_caret() {
        let (w, h) = (300, 240);
        let doc = document(w, 3000, 3);
        let a_px = view(&doc, w, 100.0, h, 0);
        let mut b_px = a_px.clone();
        for y in 60..76 {
            b_px[y * w + 150] = 0xFF00_0000; // caret
        }
        let (a, b) = (signature(&a_px, w, h), signature(&b_px, w, h));
        assert_eq!(find_offset(&a, &b, 40), Some(0));
    }

    #[test]
    fn sticky_header_and_footer() {
        let (w, h) = (300, 300);
        let doc = document(w, 3000, 5);
        let frame = |top: f32| {
            let mut px = view(&doc, w, top, h, 0);
            for y in 0..60 {
                for x in 0..w {
                    px[y * w + x] = 0xFF20_3040 + ((x / 10) as u32 % 5) * 0x0010_1010; // header bar
                }
            }
            for y in h - 30..h {
                for x in 0..w {
                    px[y * w + x] = 0xFF60_6060 + ((x / 25) as u32 % 3) * 0x0020_2020; // status bar
                }
            }
            px
        };
        let a = signature(&frame(0.0), w, h);
        let b = signature(&frame(70.0), w, h);
        assert_eq!(find_offset(&a, &b, 0), Some(70));
        // The blur softens the edge by a row or two.
        assert!(still_top(&a, &b) >= 55);
        assert!(still_bottom(&a, &b) >= 26);
    }

    #[test]
    fn too_far_is_rejected() {
        let (w, h) = (300, 200);
        let doc = document(w, 3000, 9);
        let a = signature(&view(&doc, w, 0.0, h, 0), w, h);
        let b = signature(&view(&doc, w, 900.0, h, 0), w, h);
        assert_eq!(find_offset(&a, &b, 0), None);
    }

    #[test]
    fn stress_many_pages_random_steps() {
        // What matters for the stitched image is that every seam lines up: each step's
        // offset must be within a pixel of how far the page really moved since the
        // frame it's compared with.
        let (w, h) = (420, 260);
        let (mut failures, mut steps) = (0, 0);
        let mut worst = 0.0f32;
        for seed in 1..=40u64 {
            let doc = document(w, 9000, seed * 7919);
            let mut s = seed;
            let mut prev_top = 0.0f32;
            let mut prev = signature(&view(&doc, w, prev_top, h, 2), w, h);
            let mut last = 0;
            for _ in 0..25 {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                // 0 to 85% of the view, fractional.
                let step = ((s >> 33) % 1000) as f32 / 1000.0 * h as f32 * 0.85;
                let top = prev_top + step;
                let next = signature(&view(&doc, w, top, h, 2), w, h);
                steps += 1;
                match find_offset(&prev, &next, last) {
                    Some(dy) => {
                        worst = worst.max((dy as f32 - step).abs());
                        last = dy;
                        prev = next;
                        prev_top = top;
                    }
                    None => failures += 1, // the user would scroll back a little
                }
            }
        }
        assert!(worst <= 1.0, "a seam was off by {worst} px");
        assert!(failures * 50 <= steps, "{failures} of {steps} steps failed to line up");
    }

    #[test]
    fn speed_on_a_big_area() {
        let (w, h) = (1800, 1300);
        let doc = document(w, 4000, 42);
        let a = signature(&view(&doc, w, 0.0, h, 1), w, h);
        let b = signature(&view(&doc, w, 333.0, h, 1), w, h);
        let t = std::time::Instant::now();
        assert_eq!(find_offset(&a, &b, 0), Some(333));
        let ms = t.elapsed().as_millis();
        println!("find_offset on {w}x{h}: {ms} ms");
        assert!(ms < 60, "too slow: {ms} ms");
    }

    #[test]
    fn mostly_blank_page_with_sparse_text() {
        let (w, h) = (400, 300);
        let mut doc = vec![250.0f32; w * 3000];
        for line in 0..60 {
            let y = 40 + line * 47;
            for yy in y..y + 12 {
                for xx in 20..(60 + (line * 37) % 300) {
                    doc[yy * w + xx] = 30.0;
                }
            }
        }
        let a = signature(&view(&doc, w, 0.0, h, 1), w, h);
        let b = signature(&view(&doc, w, 85.0, h, 1), w, h);
        assert_eq!(find_offset(&a, &b, 0), Some(85));
    }

    /// Drive a Scroller against a simulated app: `per_unit` pixels per wheel unit, or
    /// whole notches of `notch_px` (like Excel). Returns the image height and whether
    /// every stitched row equals the document.
    fn auto_run(per_unit: f32, notch_px: Option<usize>, h: usize, doc_rows: usize) -> (usize, usize, bool) {
        let w = 300;
        let doc = document(w, doc_rows, 99);
        let max_top = (doc_rows - h) as f32;
        let mut top = 0.0f32;
        let mut acc = 0i32;
        let mut sc = Scroller::new(w, h);
        for _ in 0..3000 {
            let px = view(&doc, w, top, h, 0);
            match sc.feed(px, Some((w / 2, h / 4))) {
                Action::Wait => {}
                Action::Finish => break,
                Action::Wheel(n) => match notch_px {
                    Some(np) => {
                        acc += n;
                        let k = acc / 120;
                        acc -= k * 120;
                        top = (top + (k * np as i32) as f32).clamp(0.0, max_top);
                    }
                    None => top = (top + n as f32 * per_unit).round().clamp(0.0, max_top),
                },
            }
        }
        let (img, rows) = sc.into_image();
        let exact = (0..rows).all(|y| (0..w).all(|x| (img[y * w + x] & 0xFF) as f32 == doc[y * w + x].clamp(0.0, 255.0).floor()));
        (rows, doc_rows, exact)
    }

    #[test]
    fn auto_scroll_captures_the_whole_page() {
        for (per_unit, h) in [(0.83, 300), (2.5, 400), (0.3, 260), (1.0, 120)] {
            let (rows, total, exact) = auto_run(per_unit, None, h, 5000);
            assert_eq!(rows, total, "per_unit={per_unit} h={h}");
            assert!(exact, "per_unit={per_unit} h={h}: rows don't match the page");
        }
    }

    #[test]
    fn auto_scroll_in_whole_notches_like_excel() {
        for (np, h) in [(60, 400), (60, 130), (150, 300)] {
            let (rows, total, exact) = auto_run(0.0, Some(np), h, 4000);
            assert_eq!(rows, total, "notch={np} h={h}");
            assert!(exact, "notch={np} h={h}: rows don't match the page");
        }
    }
}
