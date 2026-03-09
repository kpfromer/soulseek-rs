/// Metadata parsed from a Soulseek file path.
#[derive(Debug, Default)]
pub(crate) struct ParsedSoulseekMetadata {
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
}

/// Parse a Soulseek path string into track metadata.
///
/// The path is expected to be in backslash-separated form, e.g.:
/// `@@share\Artist\Album\01 - Title.flac`
///
/// Strategy:
/// 1. Collect path components (skipping the virtual share prefix).
/// 2. The filename (last component, without extension) is the source for title/artist.
/// 3. The second-to-last directory (if present) is treated as the album.
/// 4. The third-to-last directory (if present) is treated as the artist fallback.
pub(crate) fn parse_soulseek_filename(path: &str) -> ParsedSoulseekMetadata {
    // Split on backslash (the Soulseek wire separator).
    let parts: Vec<&str> = path.split('\\').collect();

    // Drop the virtual share name (first component) and collect the rest.
    let meaningful: Vec<&str> = if parts.len() > 1 {
        parts[1..].to_vec()
    } else {
        parts.clone()
    };

    // Extract filename without extension.
    let filename_with_ext = meaningful.last().copied().unwrap_or("");
    let stem = if let Some(dot) = filename_with_ext.rfind('.') {
        &filename_with_ext[..dot]
    } else {
        filename_with_ext
    };

    // Directory components before the filename.
    let dirs: Vec<&str> = if meaningful.len() > 1 {
        meaningful[..meaningful.len() - 1].to_vec()
    } else {
        vec![]
    };

    // Album: second-to-last directory.
    let album = dirs.last().map(|s| s.to_string());

    // Artist fallback: third-to-last directory (one above album).
    let dir_artist = if dirs.len() >= 2 {
        Some(dirs[dirs.len() - 2].to_string())
    } else {
        None
    };

    // Try to parse "Artist - Title" from the filename stem.
    let stem_clean = strip_track_number(stem);
    if let Some((parsed_artist, parsed_title)) = parse_artist_title_from_filename(stem_clean) {
        ParsedSoulseekMetadata {
            title: parsed_title,
            artist: parsed_artist,
            album,
        }
    } else {
        // Fall back to directory-derived artist.
        ParsedSoulseekMetadata {
            title: stem_clean.trim().to_string(),
            artist: dir_artist.unwrap_or_default(),
            album,
        }
    }
}

/// Strip a leading track number from a filename stem.
///
/// Handles patterns like:
/// - `01 - Title`
/// - `01. Title`
/// - `01 Title`
fn strip_track_number(s: &str) -> &str {
    let s = s.trim();

    // Count leading digits (max 3, e.g. "001").
    let digit_end = s
        .char_indices()
        .take_while(|(_, c)| c.is_ascii_digit())
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);

    if digit_end == 0 || digit_end > 3 {
        return s;
    }

    // Skip optional separator(s): spaces, dashes, dots.
    let rest = s[digit_end..].trim_start_matches(|c: char| c == '-' || c == '.' || c == ' ');
    if !rest.is_empty() {
        rest
    } else {
        s
    }
}

/// Try to parse `"Artist - Title"` from a filename stem.
///
/// Returns `Some((artist, title))` when a single ` - ` separator is found.
fn parse_artist_title_from_filename(s: &str) -> Option<(String, String)> {
    // Use " - " as delimiter.
    let mut parts = s.splitn(2, " - ");
    let artist = parts.next()?.trim();
    let title = parts.next()?.trim();
    if artist.is_empty() || title.is_empty() {
        return None;
    }
    Some((artist.to_string(), title.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_track_number_dash() {
        assert_eq!(strip_track_number("01 - Song Title"), "Song Title");
    }

    #[test]
    fn test_strip_track_number_dot() {
        assert_eq!(strip_track_number("02. Song Title"), "Song Title");
    }

    #[test]
    fn test_strip_track_number_no_sep() {
        assert_eq!(strip_track_number("03 Song Title"), "Song Title");
    }

    #[test]
    fn test_strip_track_number_none() {
        assert_eq!(strip_track_number("Song Title"), "Song Title");
    }

    #[test]
    fn test_parse_artist_title() {
        let r = parse_artist_title_from_filename("Queen - Bohemian Rhapsody");
        assert_eq!(r, Some(("Queen".into(), "Bohemian Rhapsody".into())));
    }

    #[test]
    fn test_parse_soulseek_path() {
        let m = parse_soulseek_filename(
            "@@share\\Queen\\A Night at the Opera\\05 - Bohemian Rhapsody.flac",
        );
        assert_eq!(m.title, "Bohemian Rhapsody");
        assert_eq!(m.artist, "Queen");
        assert_eq!(m.album.as_deref(), Some("A Night at the Opera"));
    }

    #[test]
    fn test_parse_artist_title_from_filename_in_stem() {
        let m = parse_soulseek_filename("@@share\\misc\\Queen - Bohemian Rhapsody.mp3");
        assert_eq!(m.title, "Bohemian Rhapsody");
        assert_eq!(m.artist, "Queen");
    }
}
