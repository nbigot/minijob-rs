//! Percentiles of sorted durations (`metrics/percentiles.go`, `percentilesmetrics.go`).

/// Percentile values of a sorted slice: index `ceil(len × p) − 1` (0 when empty).
pub fn percentiles(sorted: &[i64], percentiles: &[f64]) -> Vec<i64> {
    if sorted.is_empty() {
        return vec![0; percentiles.len()];
    }
    percentiles
        .iter()
        .map(|p| {
            let index = ((sorted.len() as f64 * p).ceil() as usize).saturating_sub(1);
            sorted[index.min(sorted.len() - 1)]
        })
        .collect()
}

/// Label of a percentile bucket: `p` + truncated percentage (`p50`, `p99`, `p100`).
pub fn label(percentile: f64) -> String {
    format!("p{}", (percentile * 100.0) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Port of TestPercentiles and TestPercentilesEdgeCases.
    #[test]
    fn percentiles_like_go() {
        assert_eq!(percentiles(&[], &[0.5, 0.9, 0.99]), vec![0, 0, 0]);
        assert_eq!(percentiles(&[100], &[0.1, 0.5, 0.9]), vec![100, 100, 100]);
        assert_eq!(
            percentiles(
                &[10, 20, 30, 40, 50, 60, 70, 80, 90, 100],
                &[0.0, 0.25, 0.5, 0.75, 1.0]
            ),
            vec![10, 30, 50, 80, 100]
        );
        assert_eq!(
            percentiles(&[15, 20, 35, 40, 50], &[0.0, 1.0]),
            vec![15, 50]
        );
        // The function assumes sorted input.
        assert_eq!(
            percentiles(&[50, 20, 30, 40, 10], &[0.5, 0.8]),
            vec![30, 40]
        );
        let input: Vec<i64> = (0..10000).collect();
        let out = percentiles(&input, &[0.01, 0.5, 0.99]);
        for (v, e) in out.iter().zip([100, 5000, 9900]) {
            assert!((v - e).abs() <= 10);
        }
    }

    // Port of TestPercentilesMetrics_Init / TestNewPercentilesMetrics.
    #[test]
    fn labels() {
        let l: Vec<String> = [0.5, 0.75, 0.9, 0.95, 0.99]
            .into_iter()
            .map(label)
            .collect();
        assert_eq!(l, ["p50", "p75", "p90", "p95", "p99"]);
        let l: Vec<String> = [0.1, 0.5, 0.9].into_iter().map(label).collect();
        assert_eq!(l, ["p10", "p50", "p90"]);
        let l: Vec<String> = [0.05, 1.0].into_iter().map(label).collect();
        assert_eq!(l, ["p5", "p100"]);
    }
}
