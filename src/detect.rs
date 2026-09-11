//! Finds the sheet of paper in a photo and returns its four corners (in the original,
//! full-resolution image's pixel coordinates).
//!
//! Approach (deliberately similar to the classic "OpenCV document scanner" recipe, just
//! implemented with `imageproc` primitives instead of OpenCV):
//!
//! 1. Downscale for speed/noise-robustness, blur slightly.
//! 2. Otsu-threshold into a binary mask, trying both polarities (paper lighter than its
//!    background, e.g. white paper on a desk - and paper darker than its background) since
//!    we can't assume which one applies to a given photo. Also try a *second, stricter*
//!    threshold restricted to the bright side only - on a light desk, plain paper and the
//!    desk itself can both land on the "bright" side of a single global split, merging into
//!    one blob; re-splitting just that blob often isolates the (brighter) paper from the
//!    (dimmer) desk.
//! 3. Morphologically close each mask (dilate then erode) to merge small gaps - e.g. from
//!    printed text - without moving the outer boundary much.
//! 4. Trace contours (Suzuki-Abe border following, via `imageproc::contours`), keep the
//!    largest plausible outer contour per candidate mask, take its convex hull.
//! 5. Reduce each hull to 4 corners via the standard min/max(x+y), min/max(x-y) heuristic.
//! 6. Sanity-check every candidate - big enough to be a document, not so big it's
//!    obviously the whole scene, roughly rectangular, and not touching all four sides of
//!    the frame (a real photo of a page almost always has visible margin somewhere; a
//!    detection that hugs every edge is usually background bleeding in, not a page) - and
//!    keep the largest one that passes. If nothing plausible was found, fall back to
//!    treating the entire photo as the "paper" so the program always produces output.

use crate::geometry::{Pt, clamp_to_bounds, convex_hull, expand_quad, extreme_corners, polygon_area};
use image::{GrayImage, Luma, RgbImage};
use imageproc::contours::{BorderType, find_contours};
use imageproc::distance_transform::Norm;
use imageproc::morphology::{dilate, erode};

/// Longest side (in pixels) that detection runs at. The final perspective warp always
/// samples the full-resolution original, so this only trades off detection speed vs.
/// sensitivity to small/noisy edges.
const DETECT_MAX_DIM: u32 = 1100;

/// How far outside the detected hull's own extremes we push the corners before warping,
/// so we don't clip the paper's own edge. 1.025 = 2.5% larger, measured from the quad
/// centroid. Leftover desk after this nudge is removed by the content-aware edge trim
/// in `main.rs`, which only peels dark background and leaves paper margins alone.
const EDGE_FILL_FACTOR: f64 = 1.025;

/// Smallest/largest fraction of the (downscaled) photo area a candidate blob is allowed
/// to cover and still be trusted as "the page". The upper bound is deliberately tighter
/// than "basically the whole frame": a merged paper+desk blob on a light-colored desk
/// tends to land in the low-90s-percent range, and we'd rather fall back honestly than
/// confidently crop to "the whole photo".
const MIN_AREA_FRACTION: f64 = 0.06;
const MAX_AREA_FRACTION: f64 = 0.92;

/// A candidate whose corners all lie within this fraction of every side of the frame is
/// rejected outright - a photographed page almost always has visible margin on at least
/// one side, so hugging all four edges is a strong tell that we grabbed the whole scene.
const BORDER_TOUCH_FRACTION: f64 = 0.015;

pub struct Detection {
    /// Corners in the ORIGINAL (full-resolution) image's pixel coordinates,
    /// ordered `[top_left, top_right, bottom_right, bottom_left]`.
    pub corners: [Pt; 4],
    /// True if we couldn't confidently find a paper edge and fell back to the whole photo.
    pub used_fallback: bool,
}

pub fn detect_paper(img: &RgbImage) -> Detection {
    let (full_w, full_h) = img.dimensions();
    let longest = full_w.max(full_h) as f64;
    let scale = if longest > DETECT_MAX_DIM as f64 {
        DETECT_MAX_DIM as f64 / longest
    } else {
        1.0
    };

    let (small_w, small_h) = (
        ((full_w as f64) * scale).round().max(1.0) as u32,
        ((full_h as f64) * scale).round().max(1.0) as u32,
    );

    let small = if scale < 1.0 {
        image::imageops::resize(
            img,
            small_w,
            small_h,
            image::imageops::FilterType::Triangle,
        )
    } else {
        img.clone()
    };

    let gray = image::imageops::grayscale(&small);
    let blurred = imageproc::filter::gaussian_blur_f32(&gray, 2.0);
    let small_area = (small_w as f64) * (small_h as f64);
    let (sw, sh) = (small_w as f64, small_h as f64);

    let mut best: Option<[Pt; 4]> = None;
    let mut best_area = 0.0;

    for (hull, area) in candidate_hulls(&blurred) {
        if area < small_area * MIN_AREA_FRACTION || area > small_area * MAX_AREA_FRACTION {
            continue; // too small to be confident, or looks like the whole scene
        }
        if hull.len() < 4 {
            continue;
        }
        let corners = extreme_corners(&hull);
        let quad_area = polygon_area(&corners);
        // The 4-corner approximation should still cover most of the actual blob;
        // otherwise the shape probably wasn't rectangular (bad detection).
        if quad_area < area * 0.5 {
            continue;
        }
        if touches_all_borders(&corners, sw, sh, BORDER_TOUCH_FRACTION) {
            continue;
        }
        if area > best_area {
            best_area = area;
            best = Some(corners);
        }
    }

    match best {
        Some(corners) => {
            let inv_scale = 1.0 / scale;
            let full_corners = corners.map(|p| Pt::new(p.x * inv_scale, p.y * inv_scale));
            // "Fill any edges/corners": nudge outward a little, then clamp back into the
            // photo, so a slightly-too-tight detection doesn't shave off the page edge.
            let expanded = expand_quad(full_corners, EDGE_FILL_FACTOR);
            let clamped = clamp_to_bounds(expanded, full_w as f64, full_h as f64);
            Detection {
                corners: clamped,
                used_fallback: false,
            }
        }
        None => Detection {
            corners: full_image_quad(full_w, full_h),
            used_fallback: true,
        },
    }
}

fn full_image_quad(w: u32, h: u32) -> [Pt; 4] {
    [
        Pt::new(0.0, 0.0),
        Pt::new(w as f64 - 1.0, 0.0),
        Pt::new(w as f64 - 1.0, h as f64 - 1.0),
        Pt::new(0.0, h as f64 - 1.0),
    ]
}

/// True if every corner lies within `frac` of *some* side of the frame for all four
/// sides collectively - i.e. the shape reaches all the way to the left, right, top, and
/// bottom edges. Used to reject "the whole scene" masquerading as a confident detection.
fn touches_all_borders(corners: &[Pt; 4], w: f64, h: f64, frac: f64) -> bool {
    let margin_x = w * frac;
    let margin_y = h * frac;
    let touches_left = corners.iter().any(|p| p.x <= margin_x);
    let touches_right = corners.iter().any(|p| p.x >= w - margin_x);
    let touches_top = corners.iter().any(|p| p.y <= margin_y);
    let touches_bottom = corners.iter().any(|p| p.y >= h - margin_y);
    touches_left && touches_right && touches_top && touches_bottom
}

/// Builds every candidate mask worth trying and returns each one's largest plausible
/// outer contour as a (convex hull, area) pair.
fn candidate_hulls(gray: &GrayImage) -> Vec<(Vec<Pt>, f64)> {
    let otsu = imageproc::contrast::otsu_level(gray);
    let mut candidates = Vec::new();

    for bright_foreground in [true, false] {
        if let Some(c) = find_paper_hull(gray, |v| {
            if bright_foreground { v > otsu } else { v <= otsu }
        }) {
            candidates.push(c);
        }
    }

    // Hierarchical refinement: re-threshold just the bright side of the first split.
    // If paper and background were both "bright" (e.g. white paper on a light desk),
    // this often isolates the (brighter) paper from the (dimmer) desk within it.
    let bright_values: Vec<u8> = gray.pixels().map(|p| p[0]).filter(|&v| v > otsu).collect();
    if bright_values.len() > 256
        && let Some(otsu2) = otsu_threshold(&bright_values)
        && let Some(c) = find_paper_hull(gray, |v| v > otsu2)
    {
        candidates.push(c);
    }

    candidates
}

/// Otsu's method computed directly from a slice of intensity values, rather than a whole
/// image - lets us re-run it on just a subset of pixels (see `candidate_hulls`).
fn otsu_threshold(values: &[u8]) -> Option<u8> {
    if values.is_empty() {
        return None;
    }
    let mut hist = [0u32; 256];
    for &v in values {
        hist[v as usize] += 1;
    }
    let total = values.len() as u32;
    let total_sum: f64 = hist
        .iter()
        .enumerate()
        .map(|(i, &c)| i as f64 * c as f64)
        .sum();

    let mut bg_sum = 0f64;
    let mut bg_weight = 0u32;
    let mut best_variance = -1f64;
    let mut best_t = 0u8;

    for (t, &count) in hist.iter().enumerate() {
        bg_weight += count;
        if bg_weight == 0 {
            continue;
        }
        let fg_weight = total - bg_weight;
        if fg_weight == 0 {
            break;
        }
        bg_sum += t as f64 * count as f64;
        let fg_sum = total_sum - bg_sum;
        let bg_mean = bg_sum / bg_weight as f64;
        let fg_mean = fg_sum / fg_weight as f64;
        let variance = (bg_weight as f64) * (fg_weight as f64) * (bg_mean - fg_mean).powi(2);
        if variance > best_variance {
            best_variance = variance;
            best_t = t as u8;
        }
    }
    Some(best_t)
}

/// Builds a binary mask from `is_foreground`, cleans it up, and returns the convex hull +
/// area (in pixels) of its largest plausible outer contour, if any.
fn find_paper_hull(gray: &GrayImage, is_foreground: impl Fn(u8) -> bool) -> Option<(Vec<Pt>, f64)> {
    let (w, h) = gray.dimensions();

    let mut mask = GrayImage::new(w, h);
    for (x, y, p) in gray.enumerate_pixels() {
        mask.put_pixel(x, y, Luma([if is_foreground(p[0]) { 255 } else { 0 }]));
    }

    // Morphological closing merges small gaps/holes (e.g. printed text breaking up a
    // "blank paper" mask) without significantly moving the outer boundary.
    let closed = erode(&dilate(&mask, Norm::LInf, 3), Norm::LInf, 3);

    let contours = find_contours::<u32>(&closed);
    let mut best: Option<(Vec<Pt>, f64)> = None;

    for c in &contours {
        if c.border_type != BorderType::Outer {
            continue;
        }
        if c.points.len() < 4 {
            continue;
        }
        let pts: Vec<Pt> = c
            .points
            .iter()
            .map(|p| Pt::new(p.x as f64, p.y as f64))
            .collect();
        let area = polygon_area(&pts);
        let is_better = best.as_ref().map(|(_, a)| area > *a).unwrap_or(true);
        if is_better {
            best = Some((convex_hull(&pts), area));
        }
    }

    best
}
