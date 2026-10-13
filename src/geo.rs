//! Geometry on the 1e7-scaled integers: great-circle distance, and Douglas-Peucker to draw a
//! day with fewer points than the phone kept.

/// Degrees times this are the stored integers.
pub const SCALE: f64 = 10_000_000.0;
const EARTH_RADIUS_M: f64 = 6_371_008.8;

/// A position in degrees.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geo {
    pub lat: f64,
    pub lon: f64,
}

impl Geo {
    pub fn from_scaled(lat: i32, lon: i32) -> Geo {
        Geo {
            lat: f64::from(lat) / SCALE,
            lon: f64::from(lon) / SCALE,
        }
    }

    /// The great-circle distance in metres. The read API only simplifies; the server's
    /// segmentation measures with this.
    #[allow(dead_code)]
    pub fn distance_m(self, other: Geo) -> f64 {
        let (lat1, lat2) = (self.lat.to_radians(), other.lat.to_radians());
        let d_lat = lat2 - lat1;
        let d_lon = (other.lon - self.lon).to_radians();
        let a = (d_lat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (d_lon / 2.0).sin().powi(2);
        2.0 * EARTH_RADIUS_M * a.sqrt().min(1.0).asin()
    }
}

/// Which of the points survive Douglas-Peucker at `tolerance_m`, as indexes in order; the
/// first and the last always do. Distances are measured on an equirectangular projection
/// scaled by the cosine of the points' mean latitude, which is exact enough over a day's
/// walk and cheap enough for a hundred thousand points. Iterative, with its own stack, so no
/// group of points can overflow the call stack.
pub fn simplify(points: &[Geo], tolerance_m: f64) -> Vec<usize> {
    if points.len() <= 2 {
        return (0..points.len()).collect();
    }
    let mean_lat = points.iter().map(|p| p.lat).sum::<f64>() / points.len() as f64;
    let k = mean_lat.to_radians().cos();
    let xy: Vec<(f64, f64)> = points
        .iter()
        .map(|p| {
            (
                p.lon.to_radians() * k * EARTH_RADIUS_M,
                p.lat.to_radians() * EARTH_RADIUS_M,
            )
        })
        .collect();
    let mut keep = vec![false; points.len()];
    keep[0] = true;
    keep[points.len() - 1] = true;
    let mut stack = vec![(0usize, points.len() - 1)];
    while let Some((first, last)) = stack.pop() {
        if last <= first + 1 {
            continue;
        }
        let (mut farthest, mut best) = (first, 0.0);
        for i in first + 1..last {
            let d = segment_distance(xy[i], xy[first], xy[last]);
            if d > best {
                best = d;
                farthest = i;
            }
        }
        if best > tolerance_m {
            keep[farthest] = true;
            stack.push((first, farthest));
            stack.push((farthest, last));
        }
    }
    keep.iter()
        .enumerate()
        .filter(|(_, kept)| **kept)
        .map(|(i, _)| i)
        .collect()
}

/// The distance from `p` to the segment `a`-`b` in the projected plane.
fn segment_distance(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let length2 = dx * dx + dy * dy;
    let t = if length2 == 0.0 {
        0.0
    } else {
        (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / length2).clamp(0.0, 1.0)
    };
    let (cx, cy) = (a.0 + t * dx, a.1 + t * dy);
    ((p.0 - cx).powi(2) + (p.1 - cy).powi(2)).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One degree of latitude on the sphere `distance_m` uses, so a test measures the
    /// function and not the difference between two radii.
    const DEGREE_M: f64 = EARTH_RADIUS_M * std::f64::consts::PI / 180.0;

    fn north_of(origin: Geo, metres: f64) -> Geo {
        Geo {
            lat: origin.lat + metres / DEGREE_M,
            lon: origin.lon,
        }
    }

    fn east_of(origin: Geo, metres: f64) -> Geo {
        Geo {
            lat: origin.lat,
            lon: origin.lon + metres / (DEGREE_M * origin.lat.to_radians().cos()),
        }
    }

    const HOME: Geo = Geo {
        lat: 52.2297,
        lon: 21.0122,
    };

    #[test]
    fn a_hundred_collinear_points_reduce_to_their_ends() {
        let points: Vec<Geo> = (0..100).map(|i| north_of(HOME, 10.0 * i as f64)).collect();
        for tolerance in [1.0, 10.0, 100.0] {
            assert_eq!(simplify(&points, tolerance), vec![0, 99]);
        }
    }

    #[test]
    fn the_corners_of_a_square_survive() {
        let a = HOME;
        let b = north_of(a, 100.0);
        let c = east_of(b, 100.0);
        let d = east_of(a, 100.0);
        let mut points = vec![];
        for (from, to) in [(a, b), (b, c), (c, d)] {
            for i in 0..10 {
                let t = i as f64 / 10.0;
                points.push(Geo {
                    lat: from.lat + (to.lat - from.lat) * t,
                    lon: from.lon + (to.lon - from.lon) * t,
                });
            }
        }
        points.push(d);
        let kept = simplify(&points, 10.0);
        assert_eq!(kept, vec![0, 10, 20, 30]);
    }

    #[test]
    fn the_walk_loses_most_points_and_none_strays_from_the_line() {
        let raw: Vec<[f64; 2]> =
            serde_json::from_str(include_str!("../tests/fixtures/walk.json")).unwrap();
        let points: Vec<Geo> = raw
            .iter()
            .map(|p| Geo {
                lat: p[0],
                lon: p[1],
            })
            .collect();
        assert_eq!(points.len(), 1000);
        let kept = simplify(&points, 10.0);
        assert!(kept.len() <= 200, "{} points kept", kept.len());
        assert_eq!(kept[0], 0);
        assert_eq!(*kept.last().unwrap(), 999);
        let k = points[0].lat.to_radians().cos();
        let project = |p: Geo| {
            (
                p.lon.to_radians() * k * EARTH_RADIUS_M,
                p.lat.to_radians() * EARTH_RADIUS_M,
            )
        };
        for (segment_start, segment_end) in kept.iter().zip(kept.iter().skip(1)) {
            for i in *segment_start + 1..*segment_end {
                let d = segment_distance(
                    project(points[i]),
                    project(points[*segment_start]),
                    project(points[*segment_end]),
                );
                assert!(d <= 10.0 + 1e-6, "point {i} is {d:.2} m from the line");
            }
        }
    }

    #[test]
    fn the_great_circle_distance_is_right_over_warsaw() {
        let d = HOME.distance_m(north_of(HOME, 1_000.0));
        assert!((d - 1_000.0).abs() < 1.0, "{d}");
        assert_eq!(Geo::from_scaled(522_297_000, 210_122_000), HOME);
    }
}
