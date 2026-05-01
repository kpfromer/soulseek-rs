use mm_text_match::{duration_score, normalize_str, similarity};
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

            // Min-bitrate filter. Lossless formats are always kept (their
            // "bitrate" value is meaningless / variable).
            if let Some(min) = query.min_bitrate_kbps
                && !file_type.is_lossless()
                && !file
                    .attributes
                    .bitrate
                    .is_some_and(|br| br >= min)
            {
                return None;
            }

            // Parse metadata from path.
            let parsed = parse_soulseek_filename(file.name.as_str());

            // Score this file.
            let score = compare_tracks(
                query,
                &parsed,
                &file.attributes,
                &file_type,
                file.name.as_str(),
                wanted_file_types,
            );

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
    wanted: &WantedFileTypes,
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

    let dur_score = match (attrs.duration, query.duration_secs) {
        (Some(f), Some(target)) => duration_score(f, target, 5, 30),
        _ => 0.5,
    };

    let format_score = score_format_quality(file_type, attrs.bitrate);
    let priority_nudge = wanted
        .priority_index(file_type)
        .map(|i| 1.0 - (i as f64 * 0.05).min(0.4))
        .unwrap_or(1.0);

    let raw = title_score * 0.45 + artist_score * 0.30 + dur_score * 0.10 + format_score * 0.15;
    let score = raw * priority_nudge;
    trace!(
        path = path_for_trace,
        title_score, artist_score, dur_score, format_score, priority_nudge, score, "compare_tracks"
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_score_format_lossless() {
        assert!((score_format_quality(&FileType::Flac, None) - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_score_format_mp3_320() {
        assert!((score_format_quality(&FileType::Mp3, Some(320)) - 0.85).abs() < 1e-10);
    }
}
