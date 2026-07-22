use std::collections::BTreeMap;

use crate::model::{SlowTest, TestCase, TestDurationStats};

pub fn result_counts(tests: &[TestCase]) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for test in tests {
        *counts.entry(test.result.to_ascii_lowercase()).or_insert(0) += 1;
    }
    counts
}

pub fn duration_stats(tests: &[TestCase]) -> TestDurationStats {
    let mut values: Vec<_> = tests
        .iter()
        .filter_map(|test| {
            test.run_time
                .filter(|value| value.is_finite() && *value >= 0.0)
        })
        .collect();
    values.sort_by(f64::total_cmp);

    let mut slowest: Vec<_> = tests
        .iter()
        .filter_map(|test| {
            test.run_time
                .filter(|value| value.is_finite() && *value >= 0.0)
                .map(|seconds| SlowTest {
                    name: test.name.clone(),
                    seconds,
                })
        })
        .collect();
    slowest.sort_by(|a, b| {
        b.seconds
            .total_cmp(&a.seconds)
            .then_with(|| a.name.cmp(&b.name))
    });
    slowest.truncate(20);

    TestDurationStats {
        count: values.len(),
        total_seconds: values.iter().sum(),
        min_seconds: values.first().copied(),
        median_seconds: quantile(&values, 0.5),
        p95_seconds: quantile(&values, 0.95),
        max_seconds: values.last().copied(),
        slowest,
    }
}

fn quantile(sorted: &[f64], fraction: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    if sorted.len() == 1 {
        return Some(sorted[0]);
    }
    let rank = fraction.clamp(0.0, 1.0) * (sorted.len() - 1) as f64;
    let lower = rank.floor() as usize;
    let upper = rank.ceil() as usize;
    if lower == upper {
        Some(sorted[lower])
    } else {
        let weight = rank - lower as f64;
        Some(sorted[lower] * (1.0 - weight) + sorted[upper] * weight)
    }
}

pub fn known_counts(counts: &BTreeMap<String, usize>) -> (usize, usize, usize, usize, usize) {
    let success = counts.get("success").copied().unwrap_or(0);
    let failure = counts.get("failure").copied().unwrap_or(0);
    let skipped = counts.get("skipped").copied().unwrap_or(0);
    let error = counts.get("error").copied().unwrap_or(0);
    let unknown = counts
        .iter()
        .filter(|(name, _)| !matches!(name.as_str(), "success" | "failure" | "skipped" | "error"))
        .map(|(_, count)| count)
        .sum();
    (success, failure, skipped, error, unknown)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test(name: &str, result: &str, seconds: f64) -> TestCase {
        TestCase {
            name: name.to_owned(),
            file: None,
            class_name: None,
            source: None,
            result: result.to_owned(),
            message: None,
            run_time: Some(seconds),
            extra: Default::default(),
        }
    }

    #[test]
    fn preserves_unknown_results_and_computes_percentiles() {
        let tests = vec![
            test("a", "success", 1.0),
            test("b", "failure", 2.0),
            test("c", "future-state", 3.0),
        ];
        let counts = result_counts(&tests);
        assert_eq!(known_counts(&counts), (1, 1, 0, 0, 1));
        let durations = duration_stats(&tests);
        assert_eq!(durations.median_seconds, Some(2.0));
        assert_eq!(durations.slowest[0].name, "c");
    }
}
