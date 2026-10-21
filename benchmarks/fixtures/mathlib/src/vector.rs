/// A 2-D vector with basic arithmetic.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vec2 {
    pub x: f64,
    pub y: f64,
}

impl Vec2 {
    /// Construct a vector. Returns the zero vector for non-finite components.
    pub fn new(x: f64, y: f64) -> Self {
        if !x.is_finite() || !y.is_finite() {
            return Self { x: 0.0, y: 0.0 };
        }
        Self { x, y }
    }

    /// Squared length. Avoids a square root for comparisons.
    pub fn norm_sq(&self) -> f64 {
        self.x * self.x + self.y * self.y
    }

    /// Euclidean length.
    pub fn norm(&self) -> f64 {
        self.norm_sq().sqrt()
    }

    /// Unit vector in the same direction. Returns the zero vector for a zero
    /// input so callers never see NaN.
    pub fn normalized(&self) -> Self {
        let n = self.norm();
        if n == 0.0 {
            return Self { x: 0.0, y: 0.0 };
        }
        Self {
            x: self.x / n,
            y: self.y / n,
        }
    }

    /// Dot product.
    pub fn dot(&self, other: &Self) -> f64 {
        self.x * other.x + self.y * other.y
    }
}

impl std::ops::Add for Vec2 {
    type Output = Vec2;
    fn add(self, rhs: Vec2) -> Vec2 {
        Vec2 {
            x: self.x + rhs.x,
            y: self.y + rhs.y,
        }
    }
}

impl std::ops::Sub for Vec2 {
    type Output = Vec2;
    fn sub(self, rhs: Vec2) -> Vec2 {
        Vec2 {
            x: self.x - rhs.x,
            y: self.y - rhs.y,
        }
    }
}

impl std::ops::Mul<f64> for Vec2 {
    type Output = Vec2;
    fn mul(self, rhs: f64) -> Vec2 {
        Vec2 {
            x: self.x * rhs,
            y: self.y * rhs,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arithmetic() {
        let a = Vec2::new(1.0, 2.0);
        let b = Vec2::new(3.0, 4.0);
        assert_eq!(a + b, Vec2::new(4.0, 6.0));
        assert_eq!(b - a, Vec2::new(2.0, 2.0));
        assert_eq!(a * 2.0, Vec2::new(2.0, 4.0));
    }

    #[test]
    fn norm_and_normalize() {
        assert_eq!(Vec2::new(3.0, 4.0).norm(), 5.0);
        let u = Vec2::new(3.0, 4.0).normalized();
        assert!((u.norm() - 1.0).abs() < 1e-12);
        assert_eq!(Vec2::new(0.0, 0.0).normalized(), Vec2::new(0.0, 0.0));
    }

    #[test]
    fn non_finite_input_becomes_zero() {
        assert_eq!(Vec2::new(f64::NAN, 1.0), Vec2::new(0.0, 0.0));
    }

    #[test]
    fn dot_product() {
        assert_eq!(Vec2::new(1.0, 2.0).dot(&Vec2::new(3.0, 4.0)), 11.0);
    }
}
