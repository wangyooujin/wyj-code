use std::f64::consts::PI;

/// Area of a circle with the given radius. Returns 0.0 for negative radius.
pub fn circle_area(radius: f64) -> f64 {
    if radius < 0.0 {
        return 0.0;
    }
    PI * radius * radius
}

/// Area of a rectangle. Returns 0.0 if either side is negative.
pub fn rect_area(width: f64, height: f64) -> f64 {
    if width < 0.0 || height < 0.0 {
        return 0.0;
    }
    width * height
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn circle_basic() {
        assert!((circle_area(1.0) - std::f64::consts::PI).abs() < 1e-9);
        assert_eq!(circle_area(-1.0), 0.0);
    }

    #[test]
    fn rect_basic() {
        assert_eq!(rect_area(3.0, 4.0), 12.0);
        assert_eq!(rect_area(-1.0, 4.0), 0.0);
    }
}
