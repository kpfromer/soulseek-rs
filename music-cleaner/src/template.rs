//! File naming template rendering for music-cleaner.
//!
//! A template is a plain string containing `{variable}` placeholders. The
//! rendered result is used as the destination file path relative to the output
//! root directory.
//!
//! # Supported Variables
//!
//! | Variable         | Description                                               |
//! |------------------|-----------------------------------------------------------|
//! | `{title}`        | Track title                                               |
//! | `{album}`        | Album title                                               |
//! | `{artist}`       | Primary track artist (first in the credits list)          |
//! | `{album_artist}` | Primary album artist (first in the credits list)          |
//! | `{track_number}` | Two-digit zero-padded track number (e.g. `04`)            |
//! | `{disc_number}`  | Disc number (defaults to `1` when not available)          |
//! | `{year}`         | Album release year, or empty string when unknown          |
//! | `{ext}`          | File extension **without** the leading dot (e.g. `flac`)  |
//!
//! # Folder Creation
//!
//! Forward slashes (`/`) inside the template are treated as directory
//! separators. All required intermediate directories are created automatically
//! under the configured output root. For example:
//!
//! ```text
//! {artist}/{album}/{track_number} - {title}.{ext}
//! ```
//!
//! resolves to a path such as:
//!
//! ```text
//! <output_dir>/The Beatles/Abbey Road/07 - Here Comes the Sun.flac
//! ```
//!
//! A flat (single-level) example:
//!
//! ```text
//! {disc_number}-{track_number} - {title}.{ext}
//! ```
//!
//! resolves to:
//!
//! ```text
//! <output_dir>/1-07 - Here Comes the Sun.flac
//! ```

use std::path::PathBuf;

use musicbrainz::TrackMetadata;

/// Render a naming template into a relative [`PathBuf`] using the provided
/// track metadata and file extension.
///
/// The returned path may contain multiple components (when the template
/// contains `/` separators). The caller is responsible for joining it with the
/// output root and creating any intermediate directories.
pub fn render(template: &str, meta: &TrackMetadata, source_ext: &str) -> PathBuf {
    let artist = meta
        .track_artists
        .first()
        .map(|a| a.name.as_str())
        .unwrap_or("Unknown Artist");

    let album_artist = meta
        .album_artists
        .first()
        .map(|a| a.name.as_str())
        .unwrap_or(artist);

    let year = meta
        .album_year
        .map(|y| y.to_string())
        .unwrap_or_default();

    let track_number = format!("{:02}", meta.track_number);

    // disc_number is not yet exposed by TrackMetadata; default to "1".
    let disc_number = "1";

    let rendered = template
        .replace("{title}", &sanitize_component(&meta.track_title))
        .replace("{album}", &sanitize_component(&meta.album_title))
        .replace("{artist}", &sanitize_component(artist))
        .replace("{album_artist}", &sanitize_component(album_artist))
        .replace("{track_number}", &track_number)
        .replace("{disc_number}", disc_number)
        .replace("{year}", &sanitize_component(&year))
        .replace("{ext}", source_ext);

    PathBuf::from(rendered)
}

/// Replace characters that are problematic in file or directory names.
///
/// - `:`, `?`, `*`, `"`, `<`, `>`, `|`, `\` → `-`
/// - Null bytes → space
/// - Leading/trailing whitespace is trimmed from each path component.
///
/// Note: `/` is intentionally **not** sanitised here because it acts as the
/// path-component separator in templates.
fn sanitize_component(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            ':' | '?' | '*' | '"' | '<' | '>' | '|' | '\\' => '-',
            '\0' => ' ',
            other => other,
        })
        .collect::<String>()
        // Trim each path segment individually so that e.g. " Artist " becomes
        // "Artist" without disturbing the `/` separators that are used for
        // folder creation.
        .split('/')
        .map(str::trim)
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use musicbrainz::{Artist, TrackMetadata};
    use std::time::Duration;

    fn make_meta(title: &str, album: &str, artist: &str, track_number: i32) -> TrackMetadata {
        TrackMetadata {
            sha256: String::new(),
            track_title: title.to_string(),
            track_number,
            duration: Duration::from_secs(180),
            track_musicbrainz_id: None,
            release_mbid: "release-id".to_string(),
            album_title: album.to_string(),
            album_musicbrainz_id: None,
            album_year: Some(2024),
            track_artists: vec![Artist {
                name: artist.to_string(),
                musicbrainz_id: None,
            }],
            album_artists: vec![Artist {
                name: artist.to_string(),
                musicbrainz_id: None,
            }],
        }
    }

    #[test]
    fn flat_template() {
        let meta = make_meta("My Song", "My Album", "My Artist", 3);
        let path = render("{track_number} - {title}.{ext}", &meta, "flac");
        assert_eq!(path, PathBuf::from("03 - My Song.flac"));
    }

    #[test]
    fn nested_template_creates_components() {
        let meta = make_meta("My Song", "My Album", "My Artist", 7);
        let path = render("{artist}/{album}/{track_number} - {title}.{ext}", &meta, "mp3");
        assert_eq!(
            path,
            PathBuf::from("My Artist/My Album/07 - My Song.mp3")
        );
    }

    #[test]
    fn sanitizes_special_characters() {
        let meta = make_meta("Song: Part 1", "Album?", "Artist*Name", 1);
        let path = render("{artist}/{album}/{title}.{ext}", &meta, "flac");
        assert_eq!(
            path,
            PathBuf::from("Artist-Name/Album-/Song- Part 1.flac")
        );
    }
}
