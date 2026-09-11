//! Small 2D geometry helpers used by paper detection and perspective correction.
//! Deliberately dependency-free (just f64 math) so it's easy to reason about and test.

/// A point in image space. We use `f64` throughout so intermediate math (hull, area,
/// homography control points) never loses precision before the final cast to pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pt {
    pub x: f64,
    pub y: f64,
}

impl Pt {
    pub fn new(x: f64, y: f64) -> Self {
        Pt { x, y }
    }
}

/// Andrew's monotone chain convex hull. Order of input points doesn't matter.
/// Returns the hull in counter-clockwise order with no duplicate closing point.
pub fn convex_hull(points: &[Pt]) -> Vec<Pt> {
    let mut pts = points.to_vec();
    pts.sort_by(|a, b| {
        a.x.partial_cmp(&b.x)
            .unwrap()
            .then(a.y.partial_cmp(&b.y).unwrap())
    });
    pts.dedup_by(|a, b| (a.x - b.x).abs() < 1e-9 && (a.y - b.y).abs() < 1e-9);

    let n = pts.len();
    if n < 3 {
        return pts;
    }

    fn cross(o: Pt, a: Pt, b: Pt) -> f64 {
        (a.x - o.x) * (b.y - o.y) - (a.y - o.y) * (b.x - o.x)
    }

    let mut lower: Vec<Pt> = Vec::new();
    for &p in &pts {
        while lower.len() >= 2 && cross(lower[lower.len() - 2], lower[lower.len() - 1], p) <= 0.0 {
            lower.pop();
        }
        lower.push(p);
    }

    let mut upper: Vec<Pt> = Vec::new();
    for &p in pts.iter().rev() {
        while upper.len() >= 2 && cross(upper[upper.len() - 2], upper[upper.len() - 1], p) <= 0.0 {
            upper.pop();
        }
        upper.push(p);
    }

    lower.pop();
    upper.pop();
    lower.extend(upper);
    lower
}

/// Picks the 4 "extreme" points of a convex polygon using the classic
/// min/max(x+y) and min/max(x-y) heuristic. This is a standard, robust way to turn a
/// (possibly many-sided) convex hull of a photographed sheet of paper into its four
/// corners, since a rectangle under moderate perspective distortion still has its
/// corners at the extremes of `x+y` and `x-y`.
///
/// Returns `[top_left, top_right, bottom_right, bottom_left]`.
pub fn extreme_corners(hull: &[Pt]) -> [Pt; 4] {
    let tl = *hull
        .iter()
        .min_by(|a, b| (a.x + a.y).partial_cmp(&(b.x + b.y)).unwrap())
        .unwrap();
    let br = *hull
        .iter()
        .max_by(|a, b| (a.x + a.y).partial_cmp(&(b.x + b.y)).unwrap())
        .unwrap();
    let tr = *hull
        .iter()
        .max_by(|a, b| (a.x - a.y).partial_cmp(&(b.x - b.y)).unwrap())
        .unwrap();
    let bl = *hull
        .iter()
        .min_by(|a, b| (a.x - a.y).partial_cmp(&(b.x - b.y)).unwrap())
        .unwrap();
    [tl, tr, br, bl]
}

/// Shoelace formula. Works for any simple polygon, convex or not.
pub fn polygon_area(points: &[Pt]) -> f64 {
    if points.len() < 3 {
        return 0.0;
    }
    let mut sum = 0.0;
    for i in 0..points.len() {
        let j = (i + 1) % points.len();
        sum += points[i].x * points[j].y - points[j].x * points[i].y;
    }
    (sum * 0.5).abs()
}

/// Expands a quad outward from its own centroid by `factor` (e.g. `1.03` = 3% larger).
/// Detected edges tend to sit exactly on, or a hair inside, the true paper boundary, so
/// nudging the corners outward before warping avoids clipping a thin sliver of the page.
pub fn expand_quad(quad: [Pt; 4], factor: f64) -> [Pt; 4] {
    let cx = quad.iter().map(|p| p.x).sum::<f64>() / 4.0;
    let cy = quad.iter().map(|p| p.y).sum::<f64>() / 4.0;
    quad.map(|p| Pt::new(cx + (p.x - cx) * factor, cy + (p.y - cy) * factor))
}

/// Clamps every corner back into the photo's pixel bounds (can't sample outside it).
pub fn clamp_to_bounds(quad: [Pt; 4], w: f64, h: f64) -> [Pt; 4] {
    quad.map(|p| Pt::new(p.x.clamp(0.0, w - 1.0), p.y.clamp(0.0, h - 1.0)))
}

pub fn side_length(a: Pt, b: Pt) -> f64 {
    ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt()
}

/// Given the 4 corners `[top_left, top_right, bottom_right, bottom_left]` of the detected
/// (possibly trapezoidal) paper region, picks an output rectangle size that best preserves
/// the paper's real aspect ratio: the wider of the top/bottom edges, and the taller of the
/// left/right edges.
pub fn quad_output_size(c: [Pt; 4]) -> (u32, u32) {
    let width_top = side_length(c[0], c[1]);
    let width_bottom = side_length(c[3], c[2]);
    let height_left = side_length(c[0], c[3]);
    let height_right = side_length(c[1], c[2]);

    let w = width_top.max(width_bottom).round().max(1.0) as u32;
    let h = height_left.max(height_right).round().max(1.0) as u32;
    (w, h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hull_of_square_is_its_corners() {
        let pts = vec![
            Pt::new(0.0, 0.0),
            Pt::new(10.0, 0.0),
            Pt::new(10.0, 10.0),
            Pt::new(0.0, 10.0),
            Pt::new(5.0, 5.0), // interior point, should be dropped
        ];
        let hull = convex_hull(&pts);
        assert_eq!(hull.len(), 4);
        let area = polygon_area(&hull);
        assert!((area - 100.0).abs() < 1e-6);
    }

    #[test]
    fn extreme_corners_recovers_trapezoid() {
        // A trapezoid leaning right, as if photographed at an angle.
        let quad = [
            Pt::new(10.0, 0.0),
            Pt::new(110.0, 0.0),
            Pt::new(130.0, 100.0),
            Pt::new(-10.0, 100.0),
        ];
        let hull = convex_hull(&quad);
        let corners = extreme_corners(&hull);
        // top_left should be the point with smallest x+y => (10,0)
        assert_eq!(corners[0], Pt::new(10.0, 0.0));
    }
}
