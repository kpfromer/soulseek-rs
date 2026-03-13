use unicode_normalization::UnicodeNormalization;

use soulseek_rs::types::File;

use crate::parser::parse_soulseek_filename;
use crate::types::{FileType, SongQuery, SongResult, WantedFileTypes};
use crate::{debug, trace};

const MIN_SCORE_THRESHOLD: f64 = 0.50;

/// Convert a list of soulseek-rs Files into scored, filtered, sorted SongResults.
pub(crate) fn rank_results(
    query: &SongQuery,
    files: &[File],
    wanted_file_types: &WantedFileTypes,
) -> Vec<SongResult> {
    debug!("rank_results: {} raw files", files.len());
    let mut results: Vec<SongResult> = files
        .iter()
        .filter_map(|file| {
            // Parse file type from extension.
            let ext = file.name.filename().rsplit('.').next().unwrap_or("");
            let file_type = FileType::from_extension(ext);

            if !wanted_file_types.is_compatible(&file_type) {
                return None;
            }

            // Parse metadata from path.
            let parsed = parse_soulseek_filename(file.name.as_str());

            // Score this file.
            let score = compare_tracks(query, &parsed, &file.attributes, &file_type, file.name.as_str());

            if score >= MIN_SCORE_THRESHOLD {
                Some(SongResult {
                    username: file.username.clone(),
                    filename: file.name.clone(),
                    file_type,
                    size: file.size,
                    bitrate: file.attributes.bitrate,
                    duration: file.attributes.duration,
                    sample_rate: file.attributes.sample_rate,
                    bit_depth: file.attributes.bit_depth,
                    vbr: file.attributes.vbr,
                    score,
                })
            } else {
                None
            }
        })
        .collect();

    debug!(
        "rank_results: {} files above score threshold {MIN_SCORE_THRESHOLD}",
        results.len()
    );
    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    results
}

#[cfg_attr(not(feature = "tracing"), allow(unused_variables))]
fn compare_tracks(
    query: &SongQuery,
    parsed: &crate::parser::ParsedSoulseekMetadata,
    attrs: &soulseek_rs::types::FileAttributes,
    file_type: &FileType,
    path_for_trace: &str,
) -> f64 {
    let norm_query_title = normalize_str(&query.title);
    let norm_parsed_title = normalize_str(&parsed.title);
    let title_score = similarity(&norm_query_title, &norm_parsed_title);

    let norm_query_artist = normalize_str(&query.artist);
    let norm_parsed_artist = normalize_str(&parsed.artist);
    let artist_score = if parsed.artist.is_empty() {
        0.5
    } else {
        similarity(&norm_query_artist, &norm_parsed_artist)
    };

    let duration_score = match attrs.duration {
        Some(f) => {
            let diff = (query.duration_secs as i64 - f as i64).unsigned_abs() as u32;
            if diff <= 5 {
                1.0
            } else if diff >= 30 {
                0.0
            } else {
                // Linear falloff from 1.0 at 5s to 0.0 at 30s.
                1.0 - (diff - 5) as f64 / 25.0
            }
        }
        _ => 0.5,
    };

    let format_score = score_format_quality(file_type, attrs.bitrate);

    let score = title_score * 0.45 + artist_score * 0.30 + duration_score * 0.10 + format_score * 0.15;
    trace!(
        path = path_for_trace,
        title_score,
        artist_score,
        duration_score,
        format_score,
        score,
        "compare_tracks"
    );
    score
}

fn score_format_quality(file_type: &FileType, bitrate: Option<u32>) -> f64 {
    if file_type.is_lossless() {
        return 1.0;
    }
    match bitrate {
        Some(br) if br >= 320 => 0.85,
        Some(br) if br >= 256 => 0.70,
        Some(br) if br >= 192 => 0.55,
        _ => 0.35,
    }
}

fn normalize_str(s: &str) -> String {
    // NFC normalize, lowercase, trim.
    let normalized: String = s.nfc().collect();
    let lower = normalized.to_lowercase();
    let trimmed = lower.trim();

    // Strip parenthetical and bracketed content: "(feat. X)", "[Deluxe Edition]", etc.
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

    // Collapse whitespace.
    result.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn similarity(a: &str, b: &str) -> f64 {
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

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let m = a.len();
    let n = b.len();

    let mut dp = vec![0usize; n + 1];
    for j in 0..=n {
        dp[j] = j;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_similarity_identical() {
        assert!((similarity("hello", "hello") - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_similarity_empty() {
        assert!((similarity("", "") - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_normalize_strips_parens() {
        assert_eq!(normalize_str("Song (feat. Artist)"), "song");
        assert_eq!(normalize_str("Album [Deluxe Edition]"), "album");
    }

    #[test]
    fn test_score_format_lossless() {
        assert!((score_format_quality(&FileType::Flac, None) - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_score_format_mp3_320() {
        assert!((score_format_quality(&FileType::Mp3, Some(320)) - 0.85).abs() < 1e-10);
    }
}
