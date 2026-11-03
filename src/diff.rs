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

/// Whether a retained failure id names one directory beneath `failures/`.
///
/// Loosen this rule freely; tightening it rejects evidence that was valid when
/// written.
pub fn is_safe_stable_id(id: &str) -> bool {
    !matches!(id, "" | "." | "..")
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
}

/// Whether `retained` is a `diff.txt` cut from `message` by some extractor.
///
/// Retained evidence must stay valid after `extract_diff` or `stable_test_id`
/// change, so this does not re-run the extractor. It asks only what any
/// extractor preserves: one trailing newline, a non-blank body, and body lines
/// that match distinct message lines in order. Inner lines match whole lines.
/// The first line may start mid-line and the last may stop early, because
/// extraction starts at a marker and trims trailing whitespace. A lone line may
/// be any substring. Trailing `\r` characters are ignored, because `\r\r\n`
/// survives `extract_diff`'s single CRLF fold in a form that `lines()` and
/// `split('\n')` read differently. Loosen this rule freely; tightening it
/// rejects evidence that was valid when written.
pub fn is_retained_excerpt(message: &str, retained: &[u8]) -> bool {
    let Some(body) = std::str::from_utf8(retained)
        .ok()
        .and_then(|text| text.strip_suffix('\n'))
    else {
        return false;
    };
    if body.ends_with('\n') || body.trim().is_empty() {
        return false;
    }
    fn bare(line: &str) -> &str {
        line.trim_end_matches('\r')
    }
    let normalized = message.replace("\r\n", "\n");
    let mut source = normalized.split('\n').map(bare);
    let excerpt: Vec<_> = body.split('\n').map(bare).collect();
    let last = excerpt.len() - 1;
    excerpt.into_iter().enumerate().all(|(index, line)| {
        source.any(|candidate| match (index == 0, index == last) {
            (true, true) => candidate.contains(line),
            (true, false) => candidate.ends_with(line),
            (false, true) => candidate.starts_with(line),
            (false, false) => candidate == line,
        })
    })
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
                || lower.contains(": nok")
                || lower.contains("nok case")
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

    #[test]
    fn written_ids_are_safe_path_segments() {
        for name in ["../../shell/x.sh", "..", "", "sql/\u{c4}/x.sql"] {
            let id = stable_test_id(name);
            assert!(is_safe_stable_id(&id), "{name:?} -> {id:?}");
        }
    }

    #[test]
    fn stable_ids_are_single_safe_path_segments() {
        for id in [
            "a",
            "sql_bugs-1a2b3c4d5e",
            "sql-bugs-cases-bug_123.sql",
            "..a",
            "a..",
        ] {
            assert!(is_safe_stable_id(id), "{id:?}");
        }
        for id in ["", ".", "..", "a/b", "../a", "a b", "a\n", "\u{c4}", "a\0"] {
            assert!(!is_safe_stable_id(id), "{id:?}");
        }
    }

    #[test]
    fn excerpt_accepts_what_every_extraction_branch_selects() {
        for message in [
            "unexpected\n[Query]\nq;\n[Diff]\n--- answer\n+++ actual\n-a\n+b\n\n",
            "run x [Diff] -a\r\n+b  \r\n",
            "ok\n--- answer\n+++ actual\n-a\n+b\n  \n",
            "Diff t failed\n-a\n+b\nCONSOLE OUTPUT\nnoise",
            "  assert a\nnoise\nmore noise\nFATAL b \ntrailer",
            "only assert here",
        ] {
            let diff = extract_diff(message).expect(message);
            assert!(
                is_retained_excerpt(message, format!("{diff}\n").as_bytes()),
                "{message:?} -> {diff:?}"
            );
        }
    }

    #[test]
    fn every_extraction_output_is_a_retained_excerpt() {
        const PIECES: [&str; 10] = [
            "",
            "  ",
            "Test failed",
            "[Diff]",
            "x [Diff] y",
            "--- answer",
            "Diff t FAILED",
            "CONSOLE OUTPUT",
            "assert foo ",
            "plain\r",
        ];
        const SEPARATORS: [&str; 3] = ["\n", "\r\n", "\r\r\n"];
        let mut checked = 0;
        for length in 1..=4 {
            for code in 0..PIECES.len().pow(length) {
                let lines: Vec<_> = (0..length)
                    .map(|position| PIECES[code / PIECES.len().pow(position) % PIECES.len()])
                    .collect();
                for separator in SEPARATORS {
                    let message = lines.join(separator);
                    if let Some(diff) = extract_diff(&message) {
                        assert!(
                            is_retained_excerpt(&message, format!("{diff}\n").as_bytes()),
                            "{message:?} -> {diff:?}"
                        );
                        checked += 1;
                    }
                }
            }
        }
        assert!(checked > 5_000, "{checked}");
    }

    #[test]
    fn excerpt_rejects_text_that_is_not_cut_from_the_message() {
        let message = "head\n[Diff]\n--- answer\n+++ actual\n-a\n+b\ntail";
        let excerpt = |text: &str| is_retained_excerpt(message, text.as_bytes());
        assert!(excerpt("[Diff]\n--- answer\n+++ actual\n-a\n+b\n"));
        assert!(excerpt("--- answer\n+++ actual\n-a\n"));
        assert!(excerpt("ff]\n--- answer\n+++ actual\n-a\n+\n"));
        assert!(excerpt("answer\n"));

        assert!(!excerpt(""));
        assert!(!excerpt("\n"));
        assert!(!excerpt("  \n"));
        assert!(!excerpt("--- answer\n+++ actual"));
        assert!(!excerpt("--- answer\n+++ actual\n\n"));
        assert!(!excerpt("--- answer\n+invented\n+++ actual\n"));
        assert!(!excerpt("--- answer\n+++ actual\n-a\n+b\ntail\n+more\n"));
        assert!(!excerpt("+++ actual\n--- answer\n"));
        assert!(!excerpt("--- answer\n--- answer\n"));
        assert!(!excerpt("--- answ\n+++ actual\n-a\n"));
        assert!(!excerpt("--- answer\n+++ act\n-a\n"));
        assert!(!is_retained_excerpt(message, b"--- answer\n\xff\n"));
    }
}
