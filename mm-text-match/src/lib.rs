//! Shared text-matching primitives.
//!
//! Two consumers today:
//!
//! - `song-rs` (soulseek search ranking) — fuzzy-matches a user's
//!   `(title, artist, duration)` query against the filenames + attributes
//!   of files that soulseek peers report.
//! - `mm-matching` (music-manager v2) — fuzzy-matches an unmatched local
//!   file's tags against the user's existing track library and against
//!   MusicBrainz free-text search results.
//!
//! Both want the same normalization rules and the same edit-distance
//! similarity measure. The duration-falloff curve has the same *shape* in
//! both, but the tolerance/cliff thresholds differ (soulseek peer-reported
//! durations are noisy, decoded local durations aren't), so [`duration_score`]
//! is parameterized by both.

use unicode_normalization::UnicodeNormalization;

/// NFC-normalize, lowercase, strip parenthetical/bracketed runs
/// (`(feat. X)`, `[Deluxe Edition]`), and collapse runs of whitespace.
///
/// Pure function — same input always produces the same output. Designed
/// to be called on both sides of a [`similarity`] comparison so that
/// trivial differences (case, punctuation, parenthetical noise) don't
/// cost score.
pub fn normalize_str(s: &str) -> String {
    let normalized: String = s.nfc().collect();
    let lower = normalized.to_lowercase();
    let trimmed = lower.trim();

    let mut result = String::with_capacity(trimmed.len());
    let mut depth = 0usize;
    let mut close = ' ';
    for c in trimmed.chars() {
        match c {
            '(' | '[' if depth == 0 => {
                depth = 1;
                close = if c == '(' { ')' } else { ']' };
            }
            c if depth > 0 && c == close => {
                depth = 0;
            }
            _ if depth > 0 => {}
            c => result.push(c),
        }
    }

    result.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Edit-distance similarity in `[0.0, 1.0]`.
///
/// `1.0 − levenshtein(a, b) / max(|a|, |b|)`. Two empty strings score 1.0;
/// any other pair scores by how many edits separate them as a fraction of
/// the longer string's length.
///
/// Both inputs should normally be passed through [`normalize_str`] first.
pub fn similarity(a: &str, b: &str) -> f64 {
    if a == b {
        return 1.0;
    }
    let max_len = a.len().max(b.len());
    if max_len == 0 {
        return 1.0;
    }
    let dist = levenshtein(a, b);
    1.0 - dist as f64 / max_len as f64
}

/// Standard iterative Levenshtein edit distance over `char`s.
///
/// O(n × m) time, O(min(n, m)) space (only the previous row is kept).
/// Exposed as a building block in case callers want the raw distance
/// rather than the normalized [`similarity`] value.
pub fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let m = a.len();
    let n = b.len();

    let mut dp = vec![0usize; n + 1];
    for (j, cell) in dp.iter_mut().enumerate() {
        *cell = j;
    }

    for i in 1..=m {
        let mut prev = dp[0];
        dp[0] = i;
        for j in 1..=n {
            let old = dp[j];
            dp[j] = if a[i - 1] == b[j - 1] {
                prev
            } else {
                1 + prev.min(dp[j]).min(dp[j - 1])
            };
            prev = old;
        }
    }

    dp[n]
}

/// Duration-similarity score with a flat-then-linear-falloff curve.
///
/// Returns `1.0` when `|actual - expected| <= tolerance_secs`,
/// `0.0` when `|actual - expected| >= falloff_secs`, and a linear
/// interpolation between the two thresholds otherwise.
///
/// Soulseek calls this with `(5, 30)` (peer-reported durations are
/// noisy enough that 5 s of slack is normal). Music-manager v2's local
/// matching calls it with `(2, 30)` (decoded local durations are precise
/// to a frame, so anything beyond 2 s is suspicious).
///
/// Panics on `tolerance_secs >= falloff_secs` (programmer error — the
/// falloff window would be empty or inverted).
pub fn duration_score(
    actual_secs: u32,
    expected_secs: u32,
    tolerance_secs: u32,
    falloff_secs: u32,
) -> f64 {
    assert!(
        tolerance_secs < falloff_secs,
        "tolerance_secs ({tolerance_secs}) must be < falloff_secs ({falloff_secs})"
    );
    let diff = (actual_secs as i64 - expected_secs as i64).unsigned_abs() as u32;
    if diff <= tolerance_secs {
        1.0
    } else if diff >= falloff_secs {
        0.0
    } else {
        1.0 - (diff - tolerance_secs) as f64 / (falloff_secs - tolerance_secs) as f64
    }
}

/// Convenience wrapper for callers that hold milliseconds (music-manager
/// stores both file durations and track durations in ms). Just rounds
/// to seconds and forwards to [`duration_score`].
pub fn duration_score_ms(
    actual_ms: i64,
    expected_ms: i64,
    tolerance_secs: u32,
    falloff_secs: u32,
) -> f64 {
    let to_secs = |ms: i64| (ms / 1000).max(0) as u32;
    duration_score(
        to_secs(actual_ms),
        to_secs(expected_ms),
        tolerance_secs,
        falloff_secs,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn similarity_identical() {
        assert!((similarity("hello", "hello") - 1.0).abs() < 1e-10);
    }

    #[test]
    fn similarity_empty() {
        assert!((similarity("", "") - 1.0).abs() < 1e-10);
    }

    #[test]
    fn similarity_one_edit() {
        // "kitten" → "sitten" is one substitution out of 6 chars.
        let s = similarity("kitten", "sitten");
        assert!((s - (1.0 - 1.0 / 6.0)).abs() < 1e-10);
    }

    #[test]
    fn normalize_strips_parens() {
        assert_eq!(normalize_str("Song (feat. Artist)"), "song");
        assert_eq!(normalize_str("Album [Deluxe Edition]"), "album");
    }

    #[test]
    fn normalize_lowercases_and_trims() {
        assert_eq!(normalize_str("  Hello   WORLD  "), "hello world");
    }

    #[test]
    fn duration_score_thresholds() {
        // Soulseek-style: 5 s flat, 30 s cliff.
        assert!((duration_score(100, 100, 5, 30) - 1.0).abs() < 1e-10);
        assert!((duration_score(105, 100, 5, 30) - 1.0).abs() < 1e-10);
        assert!((duration_score(130, 100, 5, 30) - 0.0).abs() < 1e-10);
        // Halfway through the falloff window: tolerance=5, falloff=30 →
        // diff=17 (=5+12) → score = 1 - 12/25 = 0.52.
        let mid = duration_score(117, 100, 5, 30);
        assert!((mid - (1.0 - 12.0 / 25.0)).abs() < 1e-10);
    }

    #[test]
    fn duration_score_is_symmetric() {
        let a = duration_score(120, 100, 2, 30);
        let b = duration_score(100, 120, 2, 30);
        assert!((a - b).abs() < 1e-10);
    }

    #[test]
    fn duration_score_ms_rounds_to_seconds() {
        // 100_500 ms → 100 s (floor toward zero), expected 100_000 ms → 100 s
        // → diff 0 → 1.0 with the v2 thresholds (2, 30).
        assert!((duration_score_ms(100_500, 100_000, 2, 30) - 1.0).abs() < 1e-10);
    }

    #[test]
    #[should_panic]
    fn duration_score_rejects_inverted_window() {
        let _ = duration_score(0, 0, 30, 5);
    }
}
