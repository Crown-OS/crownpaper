//! CSS `cubic-bezier()` timing functions, evaluated on the CPU once per frame.

#[derive(Clone, Copy)]
pub struct CubicBezier {
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
}

pub const STANDARD: CubicBezier = CubicBezier::new(0.4, 0.0, 0.2, 1.0);
pub const CROSSFADE: CubicBezier = CubicBezier::new(0.3, 0.0, 0.7, 1.0);
pub const INTRO: CubicBezier = CubicBezier::new(0.62, 0.0, 0.49, 0.99);

impl CubicBezier {
    pub const fn new(x1: f32, y1: f32, x2: f32, y2: f32) -> Self {
        Self { x1, y1, x2, y2 }
    }

    /// Solves `B_x(t) = x` with Newton-Raphson and returns `B_y(t)`.
    pub fn ease(self, x: f32) -> f32 {
        let (ax, bx, cx) = coefficients(self.x1, self.x2);
        let (ay, by, cy) = coefficients(self.y1, self.y2);
        let x = x.clamp(0.0, 1.0);
        let mut t = x;
        for _ in 0..8 {
            let delta =
                (t * (ax + t * (bx + t * cx)) - x) / (ax + t * (2.0 * bx + 3.0 * cx * t)).max(1e-6);
            t -= delta;
            if delta.abs() < 1e-6 {
                break;
            }
        }
        let t = t.clamp(0.0, 1.0);
        t * (ay + t * (by + t * cy))
    }
}

fn coefficients(p1: f32, p2: f32) -> (f32, f32, f32) {
    (3.0 * p1, 3.0 * p2 - 6.0 * p1, 3.0 * p1 - 3.0 * p2 + 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_are_fixed() {
        for curve in [STANDARD, CROSSFADE, INTRO] {
            assert!(curve.ease(0.0).abs() < 1e-4);
            assert!((curve.ease(1.0) - 1.0).abs() < 2e-2);
        }
    }

    #[test]
    fn is_monotonic() {
        let samples: Vec<f32> = (0..=100).map(|i| STANDARD.ease(i as f32 / 100.0)).collect();
        assert!(samples.windows(2).all(|pair| pair[1] >= pair[0] - 1e-5));
    }

    #[test]
    fn linear_curve_is_identity() {
        let linear = CubicBezier::new(1.0 / 3.0, 1.0 / 3.0, 2.0 / 3.0, 2.0 / 3.0);
        assert!((linear.ease(0.37) - 0.37).abs() < 1e-4);
    }
}
