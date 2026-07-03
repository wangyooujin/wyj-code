/// Compute the arithmetic mean of a slice. Returns 0.0 for empty input.
pub fn avg(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let sum: f64 = values.iter().sum();
    sum / values.len() as f64
}

/// Compute the median. For even-length input, returns the mean of the two
/// middle elements. Returns 0.0 for empty input.
pub fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mid = sorted.len() / 2;
    if sorted.len() % 2 == 0 {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    } else {
        sorted[mid]
    }
}

/// Population variance. Returns 0.0 for empty input.
pub fn variance(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let m = avg(values);
    values.iter().map(|v| (v - m) * (v - m)).sum::<f64>() / values.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avg_basic() {
        assert_eq!(avg(&[2.0, 4.0]), 3.0);
        assert_eq!(avg(&[1.0, 2.0, 3.0, 4.0]), 2.5);
    }

    #[test]
    fn avg_empty() {
        assert_eq!(avg(&[]), 0.0);
    }

    #[test]
    fn median_odd_even() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), 2.5);
    }

    #[test]
    fn variance_basic() {
        assert_eq!(variance(&[2.0, 4.0]), 1.0);
    }
}
