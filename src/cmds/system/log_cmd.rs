//! Deduplicates repeated log lines and shows counts instead.

use crate::core::sketch::deduplicate_sketched;
use crate::core::tracking;
use anyhow::Result;
use lazy_static::lazy_static;
use regex::Regex;
use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead};
use std::path::Path;

/// Default sketch similarity threshold for fuzzy dedup (0.85 = 85% similar).
const SKETCH_SIMILARITY_THRESHOLD: f64 = 0.85;

lazy_static! {
    static ref TIMESTAMP_RE: Regex =
        Regex::new(r"^\d{4}[-/]\d{2}[-/]\d{2}[T ]\d{2}:\d{2}:\d{2}[.,]?\d*\s*").unwrap();
    static ref UUID_RE: Regex =
        Regex::new(r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}")
            .unwrap();
    static ref HEX_RE: Regex = Regex::new(r"0x[0-9a-fA-F]+").unwrap();
    static ref NUM_RE: Regex = Regex::new(r"\b\d{4,}\b").unwrap();
    static ref PATH_RE: Regex = Regex::new(r"/[\w./\-]+").unwrap();
}

/// Filter and deduplicate log output
pub fn run_file(file: &Path, verbose: u8) -> Result<()> {
    let timer = tracking::TimedExecution::start();

    if verbose > 0 {
        eprintln!("Analyzing log: {}", file.display());
    }

    let content = fs::read_to_string(file)?;
    let result = analyze_logs(&content);
    println!("{}", result);
    timer.track(
        &format!("cat {}", file.display()),
        "rtk log",
        &content,
        &result,
    );
    Ok(())
}

/// Filter logs from stdin
pub fn run_stdin(_verbose: u8) -> Result<()> {
    let timer = tracking::TimedExecution::start();

    let mut content = String::new();
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        content.push_str(&line?);
        content.push('\n');
    }

    let result = analyze_logs(&content);
    println!("{}", result);

    timer.track("log (stdin)", "rtk log (stdin)", &content, &result);

    Ok(())
}

/// For use by other modules
pub fn run_stdin_str(content: &str) -> String {
    analyze_logs(content)
}

fn analyze_logs(content: &str) -> String {
    // Try sketch-enhanced analysis; fall back to exact dedup on failure (RTK fallback pattern)
    match analyze_logs_sketched(content) {
        Some(output) => output,
        None => analyze_logs_exact(content),
    }
}

/// Sketch-enhanced log analysis: normalize first, then use sketch similarity
/// for fuzzy dedup within each category.
fn analyze_logs_sketched(content: &str) -> Option<String> {
    let mut result = Vec::new();

    // Collect normalized lines by category, keeping originals for display
    let mut error_lines: Vec<(String, String)> = Vec::new(); // (normalized, original)
    let mut warn_lines: Vec<(String, String)> = Vec::new();
    let mut info_count: usize = 0;

    for line in content.lines() {
        let line_lower = line.to_lowercase();
        let normalized =
            normalize_log_line(line, &TIMESTAMP_RE, &UUID_RE, &HEX_RE, &NUM_RE, &PATH_RE);

        if line_lower.contains("error")
            || line_lower.contains("fatal")
            || line_lower.contains("panic")
        {
            error_lines.push((normalized, line.to_string()));
        } else if line_lower.contains("warn") {
            warn_lines.push((normalized, line.to_string()));
        } else if line_lower.contains("info") {
            info_count += 1;
        }
    }

    // Use sketch-based fuzzy dedup on normalized lines
    let error_clusters = sketch_cluster_lines(&error_lines, SKETCH_SIMILARITY_THRESHOLD);
    let warn_clusters = sketch_cluster_lines(&warn_lines, SKETCH_SIMILARITY_THRESHOLD);

    let total_errors: usize = error_lines.len();
    let total_warnings: usize = warn_lines.len();

    result.push("Log Summary".to_string());
    result.push(format!(
        "   [error] {} errors ({} unique)",
        total_errors,
        error_clusters.len()
    ));
    result.push(format!(
        "   [warn] {} warnings ({} unique)",
        total_warnings,
        warn_clusters.len()
    ));
    result.push(format!("   [info] {} info messages", info_count));
    result.push(String::new());

    // Errors with counts
    if !error_clusters.is_empty() {
        result.push("[ERRORS]".to_string());
        format_clusters(&error_lines, &error_clusters, 10, &mut result);

        if error_clusters.len() > 10 {
            result.push(format!(
                "   ... +{} more unique errors",
                error_clusters.len() - 10
            ));
        }
        result.push(String::new());
    }

    // Warnings with counts
    if !warn_clusters.is_empty() {
        result.push("[WARNINGS]".to_string());
        format_clusters(&warn_lines, &warn_clusters, 5, &mut result);

        if warn_clusters.len() > 5 {
            result.push(format!(
                "   ... +{} more unique warnings",
                warn_clusters.len() - 5
            ));
        }
    }

    Some(result.join("\n"))
}

/// Cluster lines using sketch similarity on the normalized text.
/// Returns `(exemplar_index, count)` pairs sorted by count descending.
fn sketch_cluster_lines(
    lines: &[(String, String)],
    threshold: f64,
) -> Vec<(usize, usize)> {
    if lines.is_empty() {
        return Vec::new();
    }

    let normalized_refs: Vec<&str> = lines.iter().map(|(n, _)| n.as_str()).collect();
    let mut clusters = deduplicate_sketched(&normalized_refs, threshold);
    clusters.sort_by(|a, b| b.1.cmp(&a.1));
    clusters
}

/// Format clustered lines into output, showing exemplar + count.
fn format_clusters(
    lines: &[(String, String)],
    clusters: &[(usize, usize)],
    limit: usize,
    result: &mut Vec<String>,
) {
    for &(exemplar_idx, count) in clusters.iter().take(limit) {
        let original = &lines[exemplar_idx].1;
        let truncated = if original.len() > 100 {
            let t: String = original.chars().take(97).collect();
            format!("{}...", t)
        } else {
            original.to_string()
        };

        if count > 1 {
            result.push(format!("   [×{}] {}", count, truncated));
        } else {
            result.push(format!("   {}", truncated));
        }
    }
}

/// Exact dedup fallback (original algorithm) -- used if sketch analysis fails.
fn analyze_logs_exact(content: &str) -> String {
    let mut result = Vec::new();
    let mut error_counts: HashMap<String, usize> = HashMap::new();
    let mut warn_counts: HashMap<String, usize> = HashMap::new();
    let mut info_counts: HashMap<String, usize> = HashMap::new();
    let mut unique_errors: Vec<String> = Vec::new();
    let mut unique_warnings: Vec<String> = Vec::new();

    for line in content.lines() {
        let line_lower = line.to_lowercase();
        let normalized =
            normalize_log_line(line, &TIMESTAMP_RE, &UUID_RE, &HEX_RE, &NUM_RE, &PATH_RE);

        if line_lower.contains("error")
            || line_lower.contains("fatal")
            || line_lower.contains("panic")
        {
            let count = error_counts.entry(normalized.clone()).or_insert(0);
            if *count == 0 {
                unique_errors.push(line.to_string());
            }
            *count += 1;
        } else if line_lower.contains("warn") {
            let count = warn_counts.entry(normalized.clone()).or_insert(0);
            if *count == 0 {
                unique_warnings.push(line.to_string());
            }
            *count += 1;
        } else if line_lower.contains("info") {
            *info_counts.entry(normalized).or_insert(0) += 1;
        }
    }

    let total_errors: usize = error_counts.values().sum();
    let total_warnings: usize = warn_counts.values().sum();
    let total_info: usize = info_counts.values().sum();

    result.push("Log Summary".to_string());
    result.push(format!(
        "   [error] {} errors ({} unique)",
        total_errors,
        error_counts.len()
    ));
    result.push(format!(
        "   [warn] {} warnings ({} unique)",
        total_warnings,
        warn_counts.len()
    ));
    result.push(format!("   [info] {} info messages", total_info));
    result.push(String::new());

    if !unique_errors.is_empty() {
        result.push("[ERRORS]".to_string());

        let mut error_list: Vec<_> = error_counts.iter().collect();
        error_list.sort_by(|a, b| b.1.cmp(a.1));

        for (normalized, count) in error_list.iter().take(10) {
            let original = unique_errors
                .iter()
                .find(|e| {
                    &normalize_log_line(e, &TIMESTAMP_RE, &UUID_RE, &HEX_RE, &NUM_RE, &PATH_RE)
                        == *normalized
                })
                .map(|s| s.as_str())
                .unwrap_or(normalized);

            let truncated = if original.len() > 100 {
                let t: String = original.chars().take(97).collect();
                format!("{}...", t)
            } else {
                original.to_string()
            };

            if **count > 1 {
                result.push(format!("   [×{}] {}", count, truncated));
            } else {
                result.push(format!("   {}", truncated));
            }
        }

        if error_list.len() > 10 {
            result.push(format!(
                "   ... +{} more unique errors",
                error_list.len() - 10
            ));
        }
        result.push(String::new());
    }

    if !unique_warnings.is_empty() {
        result.push("[WARNINGS]".to_string());

        let mut warn_list: Vec<_> = warn_counts.iter().collect();
        warn_list.sort_by(|a, b| b.1.cmp(a.1));

        for (normalized, count) in warn_list.iter().take(5) {
            let original = unique_warnings
                .iter()
                .find(|w| {
                    &normalize_log_line(w, &TIMESTAMP_RE, &UUID_RE, &HEX_RE, &NUM_RE, &PATH_RE)
                        == *normalized
                })
                .map(|s| s.as_str())
                .unwrap_or(normalized);

            let truncated = if original.len() > 100 {
                let t: String = original.chars().take(97).collect();
                format!("{}...", t)
            } else {
                original.to_string()
            };

            if **count > 1 {
                result.push(format!("   [×{}] {}", count, truncated));
            } else {
                result.push(format!("   {}", truncated));
            }
        }

        if warn_list.len() > 5 {
            result.push(format!(
                "   ... +{} more unique warnings",
                warn_list.len() - 5
            ));
        }
    }

    result.join("\n")
}

fn normalize_log_line(
    line: &str,
    timestamp_re: &Regex,
    uuid_re: &Regex,
    hex_re: &Regex,
    num_re: &Regex,
    path_re: &Regex,
) -> String {
    let mut normalized = timestamp_re.replace_all(line, "").to_string();
    normalized = uuid_re.replace_all(&normalized, "<UUID>").to_string();
    normalized = hex_re.replace_all(&normalized, "<HEX>").to_string();
    normalized = num_re.replace_all(&normalized, "<NUM>").to_string();
    normalized = path_re.replace_all(&normalized, "<PATH>").to_string();
    normalized.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_analyze_logs() {
        let logs = r#"
2024-01-01 10:00:00 ERROR: Connection failed to /api/server
2024-01-01 10:00:01 ERROR: Connection failed to /api/server
2024-01-01 10:00:02 ERROR: Connection failed to /api/server
2024-01-01 10:00:03 WARN: Retrying connection
2024-01-01 10:00:04 INFO: Connected
"#;
        let result = analyze_logs(logs);
        assert!(result.contains("×3"));
        assert!(result.contains("ERRORS"));
    }

    #[test]
    fn test_analyze_logs_multibyte() {
        let logs = format!(
            "2024-01-01 10:00:00 ERROR: {} connection failed\n\
             2024-01-01 10:00:01 WARN: {} retry attempt\n",
            "ข้อผิดพลาด".repeat(15),
            "คำเตือน".repeat(15)
        );
        let result = analyze_logs(&logs);
        // Should not panic even with very long multi-byte messages
        assert!(result.contains("ERRORS"));
    }

    #[test]
    fn test_sketch_fuzzy_dedup_groups_similar_errors() {
        // Lines differ only in the path segment -- after normalization paths become <PATH>,
        // so these should be grouped by both exact and sketch dedup.
        let logs = "\
2024-01-01 10:00:00 ERROR: Connection failed to /api/server1\n\
2024-01-01 10:00:01 ERROR: Connection failed to /api/server2\n\
2024-01-01 10:00:02 ERROR: Connection failed to /api/server3\n\
2024-01-01 10:00:03 INFO: OK\n";
        let result = analyze_logs(logs);
        assert!(result.contains("ERRORS"));
        // All three should be clustered (shown as x3)
        assert!(
            result.contains("×3"),
            "Expected fuzzy dedup to group 3 similar errors, got:\n{}",
            result
        );
    }

    #[test]
    fn test_sketch_dedup_keeps_distinct_messages() {
        let logs = "\
2024-01-01 10:00:00 ERROR: Connection refused\n\
2024-01-01 10:00:01 ERROR: Out of memory on allocation\n\
2024-01-01 10:00:02 ERROR: Permission denied for user root\n";
        let result = analyze_logs(logs);
        assert!(result.contains("ERRORS"));
        // Three distinct errors should remain separate (3 unique)
        assert!(
            result.contains("3 unique"),
            "Expected 3 unique errors, got:\n{}",
            result
        );
    }

    #[test]
    fn test_exact_fallback_works() {
        // Verify the exact dedup fallback produces valid output
        let logs = "\
2024-01-01 10:00:00 ERROR: something failed\n\
2024-01-01 10:00:01 ERROR: something failed\n";
        let result = analyze_logs_exact(logs);
        assert!(result.contains("×2"));
        assert!(result.contains("ERRORS"));
    }

    #[test]
    fn test_sketch_cluster_lines_empty() {
        let lines: Vec<(String, String)> = Vec::new();
        let clusters = sketch_cluster_lines(&lines, 0.85);
        assert!(clusters.is_empty());
    }
}
