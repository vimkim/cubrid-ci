use regex::Regex;
use sha2::{Digest, Sha256};

pub fn stable_test_id(name: &str) -> String {
    let mut slug = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    while slug.contains("__") {
        slug = slug.replace("__", "_");
    }
    slug = slug.trim_matches('_').to_owned();
    if slug.len() > 80 {
        slug.truncate(80);
    }
    if slug.is_empty() {
        slug.push_str("test");
    }
    let digest = Sha256::digest(name.as_bytes());
    format!("{slug}-{}", &hex::encode(digest)[..10])
}

pub fn node_index(message: &str) -> Option<u32> {
    Regex::new(r"parallel-([0-9]+)")
        .expect("valid node regex")
        .captures(message)
        .and_then(|capture| capture.get(1))
        .and_then(|value| value.as_str().parse().ok())
}

pub fn extract_diff(message: &str) -> Option<String> {
    let normalized = message.replace("\r\n", "\n");
    let trimmed = normalized.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("Test failed") {
        return None;
    }

    if let Some(index) = normalized.find("[Diff]") {
        let diff = normalized[index..].trim();
        if diff.lines().count() > 1 {
            return Some(diff.to_owned());
        }
    }

    let lines: Vec<_> = normalized.lines().collect();
    if let Some(start) = lines.iter().position(|line| line.starts_with("--- answer")) {
        return Some(lines[start..].join("\n").trim().to_owned());
    }

    if let Some(start) = lines.iter().position(|line| {
        let lower = line.to_ascii_lowercase();
        lower.starts_with("diff ") && lower.contains(" failed")
    }) {
        let end = lines[start..]
            .iter()
            .position(|line| line.contains("CONSOLE OUTPUT"))
            .map(|offset| start + offset)
            .unwrap_or(lines.len());
        let candidate = lines[start..end].join("\n").trim().to_owned();
        if candidate.lines().count() > 1 {
            return Some(candidate);
        }
    }

    let diagnostics: Vec<_> = lines
        .iter()
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains("nok timeout")
                || lower.contains("assert")
                || lower.contains("core dump")
                || lower.contains("coredump")
                || lower.contains("fatal")
                || lower.contains("segmentation fault")
        })
        .copied()
        .collect();
    (!diagnostics.is_empty()).then(|| diagnostics.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn does_not_invent_diff_for_bare_failure() {
        assert_eq!(extract_diff("Test failed"), None);
    }

    #[test]
    fn extracts_sql_diff() {
        let message =
            "unexpected result\n[Query]\nselect 1;\n[Diff]\n--- answer\n+++ actual\n-a\n+b";
        assert_eq!(
            extract_diff(message).unwrap(),
            "[Diff]\n--- answer\n+++ actual\n-a\n+b"
        );
    }

    #[test]
    fn extracts_parallel_node_and_stable_safe_id() {
        assert_eq!(node_index("EnvIdentify: local[parallel-45]"), Some(45));
        let id = stable_test_id("../../shell/x.sh");
        assert!(!id.contains('/'));
        assert!(!id.contains(".."));
    }
}
