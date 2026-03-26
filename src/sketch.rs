//! QJL-inspired line-level sketch for fuzzy deduplication.
//!
//! Computes a 64-bit sketch of a text line by hashing each whitespace-separated
//! token and folding the hashes into a single u64 via XOR with position-based
//! rotation. Similarity is measured via Hamming distance on the bit patterns.

use std::hash::{Hash, Hasher};

/// A 64-bit sketch representing the token structure of a text line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineSketch {
    pub bits: u64,
}

/// Compute a 64-bit sketch for a text line.
///
/// Each whitespace-separated token is hashed with `DefaultHasher`, then
/// the hash is rotated left by `(position % 64)` bits before being XORed
/// into the accumulator. This preserves positional information while
/// remaining cheap to compute.
pub fn sketch_line(line: &str) -> LineSketch {
    let mut bits: u64 = 0;
    for (pos, token) in line.split_whitespace().enumerate() {
        let mut hasher = std::hash::DefaultHasher::new();
        token.hash(&mut hasher);
        let h = hasher.finish();
        let rotated = h.rotate_left((pos % 64) as u32);
        bits ^= rotated;
    }
    LineSketch { bits }
}

/// Compute the similarity between two sketches as a value in `[0.0, 1.0]`.
///
/// Uses Hamming similarity: `1.0 - popcount(a XOR b) / 64`.
pub fn sketch_similarity(a: &LineSketch, b: &LineSketch) -> f64 {
    let diff_bits = (a.bits ^ b.bits).count_ones() as f64;
    1.0 - diff_bits / 64.0
}

/// Deduplicate lines using sketch similarity.
///
/// Returns a list of `(exemplar_index, count)` pairs. Each exemplar
/// represents a cluster of lines whose sketches exceed `threshold`
/// similarity with the exemplar.
///
/// Worst case O(n * k) where k is the number of unique clusters (O(n^2)
/// if all lines are unique), but O(n) typical when most lines match an
/// existing cluster (early exit on first match).
pub fn deduplicate_sketched(lines: &[&str], threshold: f64) -> Vec<(usize, usize)> {
    if lines.is_empty() {
        return Vec::new();
    }

    // Each cluster: (exemplar_index, sketch, count)
    let mut clusters: Vec<(usize, LineSketch, usize)> = Vec::new();

    for (i, line) in lines.iter().enumerate() {
        let sketch = sketch_line(line);

        // Try to find an existing cluster that matches
        let mut matched = false;
        for cluster in clusters.iter_mut() {
            if sketch_similarity(&cluster.1, &sketch) >= threshold {
                cluster.2 += 1;
                matched = true;
                break; // Early exit on first match
            }
        }

        if !matched {
            clusters.push((i, sketch, 1));
        }
    }

    clusters.iter().map(|(idx, _, count)| (*idx, *count)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sketch_identical_lines() {
        let a = sketch_line("ERROR: connection refused to server");
        let b = sketch_line("ERROR: connection refused to server");
        assert_eq!(a, b);
        assert!((sketch_similarity(&a, &b) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_sketch_similar_lines() {
        let a = sketch_line("ERROR: connection refused to host alpha");
        let b = sketch_line("ERROR: connection refused to host beta");
        let sim = sketch_similarity(&a, &b);
        // These lines differ by one token so similarity should be high
        assert!(sim > 0.5, "Expected similarity > 0.5, got {}", sim);
    }

    #[test]
    fn test_sketch_different_lines() {
        let a = sketch_line("ERROR: connection refused to server");
        let b = sketch_line("INFO: starting application on port 8080");
        let sim = sketch_similarity(&a, &b);
        // Very different lines should have low similarity
        assert!(sim < 0.85, "Expected similarity < 0.85, got {}", sim);
    }

    #[test]
    fn test_sketch_empty_line() {
        let s = sketch_line("");
        assert_eq!(s.bits, 0);
    }

    #[test]
    fn test_sketch_single_token() {
        let s = sketch_line("ERROR");
        assert_ne!(s.bits, 0);
    }

    #[test]
    fn test_deduplicate_identical() {
        let lines = vec![
            "ERROR: connection failed",
            "ERROR: connection failed",
            "ERROR: connection failed",
        ];
        let clusters = deduplicate_sketched(&lines, 0.85);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0], (0, 3));
    }

    #[test]
    fn test_deduplicate_distinct() {
        let lines = vec![
            "ERROR: connection refused",
            "WARN: disk space low",
            "INFO: server started",
        ];
        let clusters = deduplicate_sketched(&lines, 0.85);
        assert_eq!(clusters.len(), 3);
        for cluster in &clusters {
            assert_eq!(cluster.1, 1);
        }
    }

    #[test]
    fn test_deduplicate_empty() {
        let lines: Vec<&str> = vec![];
        let clusters = deduplicate_sketched(&lines, 0.85);
        assert!(clusters.is_empty());
    }

    #[test]
    fn test_deduplicate_mixed() {
        // Two groups of identical lines plus one outlier
        let lines = vec![
            "ERROR: connection refused to host alpha",
            "ERROR: connection refused to host alpha",
            "ERROR: connection refused to host alpha",
            "WARN: disk space critically low on volume sda1",
            "WARN: disk space critically low on volume sda1",
            "INFO: completely different message here",
        ];
        let clusters = deduplicate_sketched(&lines, 0.85);
        // 3 clusters: the ERROR group (x3), the WARN group (x2), and the INFO (x1)
        assert_eq!(
            clusters.len(),
            3,
            "Expected 3 clusters, got {}",
            clusters.len()
        );
    }

    #[test]
    fn test_similarity_symmetry() {
        let a = sketch_line("foo bar baz");
        let b = sketch_line("foo bar qux");
        assert!((sketch_similarity(&a, &b) - sketch_similarity(&b, &a)).abs() < f64::EPSILON);
    }

    #[test]
    fn test_similarity_range() {
        let a = sketch_line("hello world");
        let b = sketch_line("goodbye universe");
        let sim = sketch_similarity(&a, &b);
        assert!((0.0..=1.0).contains(&sim), "Similarity {} out of range", sim);
    }
}
