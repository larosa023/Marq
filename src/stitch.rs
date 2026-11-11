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

/// Average difference per band (0–255) below which two rows count as the same.
const SAME_ROW: u32 = 2;
/// The best offset must have an average difference below this to be trusted...
const MAX_COST: f32 = 10.0;
/// ...and be clearly better than any offset that isn't a near neighbour of it.
const CLEAR_WIN: f32 = 0.6;
/// Rows next to pinned areas are blurred together with them; skip a few more.
const EDGE_PAD: usize = 4;

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
fn cost_at(prev: &Sig, next: &Sig, dy: usize, top: usize, bottom: usize, step: usize) -> Option<f32> {
    // Both rows compared must lie in the free (non-pinned) part: next row r, prev row r+dy.
    let end = bottom.saturating_sub(dy);
    if end <= top {
        return None;
    }
    let (mut sum, mut n) = (0u64, 0u32);
    let mut r = top;
    while r < end {
        if next.detail[r] || prev.detail[r + dy] {
            sum += row_cost(&next.rows[r], &prev.rows[r + dy]) as u64;
            n += 1;
        }
        r += step;
    }
    let min_rows = if step == 1 { 6 } else { 2 };
    if n < min_rows {
        return None;
    }
    Some(sum as f32 / (n as f32 * BANDS as f32))
}

/// How many rows the content moved up between `prev` and `next` (0 = it didn't move).
/// None if they don't line up convincingly (scrolled too far, or the page changed).
pub fn find_offset(prev: &Sig, next: &Sig, last_dy: usize) -> Option<usize> {
    let h = prev.h();
    if h < 8 || next.h() != h {
        return None;
    }
    // Leave out rows that are pinned in place, plus a small margin at each edge.
    let top = (still_top(prev, next) + EDGE_PAD).min(h / 2);
    let bottom = h - (still_bottom(prev, next) + EDGE_PAD).min(h / 3);
    if bottom <= top + 4 {
        return Some(0); // everything that can be compared is unchanged
    }
    let max_dy = h * 9 / 10;

    // Coarse pass on every 3rd row, then a precise look at the best few candidates.
    let mut coarse: Vec<(f32, usize)> = (0..=max_dy)
        .filter_map(|dy| cost_at(prev, next, dy, top, bottom, 3).map(|c| (c, dy)))
        .collect();
    if coarse.is_empty() {
        // Nothing informative to compare (e.g. a blank area): treat as unchanged
        // if it really is the same, otherwise unknown.
        return cost_at(prev, next, 0, 0, h, 1).filter(|c| *c <= 1.0).map(|_| 0).or(None);
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
    let mut fine: Vec<(f32, usize)> = cands
        .iter()
        .filter_map(|&dy| cost_at(prev, next, dy, top, bottom, 1).map(|c| (c, dy)))
        .collect();
    fine.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let &(best_cost, best_dy) = fine.first()?;
    if best_cost > MAX_COST {
        return None;
    }
    if let Some(r) = rival {
        if best_cost > r * CLEAR_WIN && best_cost > 0.3 {
            return None; // ambiguous: several offsets look about as good
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
}
