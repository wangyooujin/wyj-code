use crate::stats::{avg, median};

/// Render a one-line textual summary of a data series.
pub fn summarize(name: &str, values: &[f64]) -> String {
    format!(
        "{}: n={} avg={:.2} median={:.2}",
        name,
        values.len(),
        avg(values),
        median(values)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarize_basic() {
        assert_eq!(summarize("s", &[2.0, 4.0]), "s: n=2 avg=3.00 median=3.00");
    }
}
