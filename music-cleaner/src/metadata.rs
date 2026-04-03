//! Audio tag reading and writing via lofty.
//!
//! The public interface is intentionally small:
//! - [`is_already_processed`] – quickly checks for `KYLE_PROCESSED_AT`
//! - [`apply_metadata`] – writes MusicBrainz data + cover art, and (on the
//!   first run only) backs up the pre-existing tags under `KYLE_*` prefixed
//!   custom fields.

use std::path::Path;

use chrono::Utc;
use cover_art_archive::AlbumArt;
use lofty::config::WriteOptions;
use lofty::file::TaggedFileExt;
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::tag::{Accessor, ItemKey, ItemValue, Tag, TagExt, TagItem};
use musicbrainz::TrackMetadata;

const PROCESSED_AT_KEY: &str = "KYLE_PROCESSED_AT";
const SOURCE_KEY: &str = "KYLE_MUSICBRAINZ_SOURCE";
const KYLE_PREFIX: &str = "KYLE_";

// ─── Public types ────────────────────────────────────────────────────────────

/// Metadata extracted from a file's existing embedded audio tags.
pub struct FileTagMetadata {
    pub track_title: String,
    pub track_number: Option<i32>,
    pub album_title: String,
    pub album_year: Option<i32>,
    pub track_artists: Vec<String>,
    pub album_artists: Vec<String>,
}

/// The resolved source of metadata for a track.
pub enum ResolvedTrackMetadata {
    MusicBrainz(TrackMetadata),
    ExistingFile(FileTagMetadata),
}

// ─── Public API ─────────────────────────────────────────────────────────────

/// Returns `true` when `KYLE_PROCESSED_AT` is present in the file's primary
/// tag, indicating that music-cleaner has already processed this file.
pub fn is_already_processed(path: &Path) -> anyhow::Result<bool> {
    let tagged_file = lofty::read_from_path(path)?;
    let present = tagged_file
        .primary_tag()
        .and_then(|tag| tag.get_string(&ItemKey::Unknown(PROCESSED_AT_KEY.to_string())))
        .is_some();
    Ok(present)
}

/// Read and enrich the tags of the audio file at `path`.
///
/// # First-run behaviour (when `already_processed` is `false`)
///
/// 1. All existing text tag fields are copied verbatim into `KYLE_`-prefixed
///    custom fields (e.g. the current `TITLE` becomes `KYLE_TITLE`).
/// 2. A `KYLE_PROCESSED_AT` timestamp (RFC 3339) is added.
/// 3. Standard metadata is written over the main fields (MusicBrainz variant only).
/// 4. If `art` is provided, the front cover picture is replaced.
///
/// # Re-run behaviour (when `already_processed` is `true`)
///
/// Only steps 3 and 4 are performed. Existing `KYLE_*` backup tags are
/// **never** touched so that the original data from the very first run is
/// always preserved.
pub fn apply_metadata(
    path: &Path,
    meta: &ResolvedTrackMetadata,
    art: Option<&AlbumArt>,
    already_processed: bool,
) -> anyhow::Result<()> {
    let mut tagged_file = lofty::read_from_path(path)?;

    // Ensure a writable primary tag exists.
    if tagged_file.primary_tag().is_none() {
        let tag_type = tagged_file.primary_tag_type();
        tagged_file.insert_tag(Tag::new(tag_type));
    }

    // --- Backup pass (first run only) ---
    if !already_processed {
        let tag = tagged_file.primary_tag_mut().unwrap();
        write_kyle_backup(tag);
    }

    // --- Metadata + art pass ---
    {
        let tag = tagged_file.primary_tag_mut().unwrap();
        match meta {
            ResolvedTrackMetadata::MusicBrainz(m) => {
                write_musicbrainz_tags(tag, m);
                tag.insert_unchecked(TagItem::new(
                    ItemKey::Unknown(SOURCE_KEY.to_string()),
                    ItemValue::Text("musicbrainz".to_string()),
                ));
            }
            ResolvedTrackMetadata::ExistingFile(_) => {
                // Tags are already correct — only stamp the source marker.
                tag.insert_unchecked(TagItem::new(
                    ItemKey::Unknown(SOURCE_KEY.to_string()),
                    ItemValue::Text("existing_tags".to_string()),
                ));
            }
        }
        if let Some(art) = art {
            embed_cover_art(tag, art);
        }
    }

    // Save via TagExt::save_to_path (immutable borrow, borrows released above).
    let tag = tagged_file.primary_tag().unwrap();
    tag.save_to_path(path, WriteOptions::default())?;

    Ok(())
}

/// Attempt to read metadata from the file's existing embedded audio tags.
///
/// Returns `None` when no tag is found or the tag has no title.
pub fn read_existing_tags(path: &Path) -> anyhow::Result<Option<FileTagMetadata>> {
    let tagged_file = lofty::read_from_path(path)?;
    let tag = tagged_file.primary_tag().or_else(|| tagged_file.first_tag());
    let Some(tag) = tag else {
        return Ok(None);
    };

    let title = tag.title().map(|c| c.into_owned()).unwrap_or_default();
    if title.is_empty() {
        return Ok(None);
    }

    let artist = tag.artist().map(|c| c.into_owned()).unwrap_or_default();
    let album = tag.album().map(|c| c.into_owned()).unwrap_or_default();
    let album_artist = tag
        .get_string(&ItemKey::AlbumArtist)
        .map(|s| s.to_owned())
        .unwrap_or_else(|| artist.clone());

    Ok(Some(FileTagMetadata {
        track_title: title,
        track_number: tag.track().map(|n| n as i32),
        album_title: album,
        album_year: tag.year().map(|y| y as i32),
        track_artists: if artist.is_empty() { vec![] } else { vec![artist] },
        album_artists: if album_artist.is_empty() { vec![] } else { vec![album_artist] },
    }))
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Copy every existing text field into a `KYLE_`-prefixed custom field, then
/// stamp `KYLE_PROCESSED_AT` with the current UTC time.
///
/// Fields that already start with `KYLE_` (from a previous, partial run) are
/// skipped so we never double-prefix a backup.
fn write_kyle_backup(tag: &mut Tag) {
    // Collect first to avoid simultaneous mutable + shared borrow.
    let existing: Vec<(String, String)> = tag
        .items()
        .filter_map(|item| {
            let key = item_key_to_backup_name(item.key());
            if key.starts_with(KYLE_PREFIX) {
                return None; // already a backup field
            }
            match item.value() {
                ItemValue::Text(v) => Some((key, v.clone())),
                _ => None,
            }
        })
        .collect();

    for (key, value) in existing {
        tag.insert_unchecked(TagItem::new(
            ItemKey::Unknown(format!("{KYLE_PREFIX}{key}")),
            ItemValue::Text(value),
        ));
    }

    tag.insert_unchecked(TagItem::new(
        ItemKey::Unknown(PROCESSED_AT_KEY.to_string()),
        ItemValue::Text(Utc::now().to_rfc3339()),
    ));
}

/// Overwrite standard tag fields with data from `TrackMetadata`.
///
/// Writes:
/// - Track title, artist(s), album, album artist, track number, year
/// - MusicBrainz recording ID, release ID, and release-group ID
fn write_musicbrainz_tags(tag: &mut Tag, meta: &TrackMetadata) {
    let primary_artist = meta
        .track_artists
        .first()
        .map(|a| a.name.clone())
        .unwrap_or_default();

    let primary_album_artist = meta
        .album_artists
        .first()
        .map(|a| a.name.clone())
        .unwrap_or_else(|| primary_artist.clone());

    tag.set_title(meta.track_title.clone());
    tag.set_artist(primary_artist.clone());
    tag.set_album(meta.album_title.clone());

    // AlbumArtist is not covered by the `Accessor` trait convenience methods.
    tag.insert_text(ItemKey::AlbumArtist, primary_album_artist);

    tag.set_track(meta.track_number as u32);

    if let Some(year) = meta.album_year {
        tag.set_year(year as u32);
    }

    // MusicBrainz IDs — lofty maps these to the correct native format fields.
    tag.insert_text(
        ItemKey::MusicBrainzReleaseId,
        meta.release_mbid.clone(),
    );

    if let Some(ref mbid) = meta.track_musicbrainz_id {
        // MusicBrainzRecordingId maps to MUSICBRAINZ_TRACKID (Vorbis) /
        // "MusicBrainz Track Id" TXXX frame (ID3) / ----:com.apple.iTunes (MP4)
        tag.insert_text(ItemKey::MusicBrainzRecordingId, mbid.clone());
    }

    if let Some(ref mbid) = meta.album_musicbrainz_id {
        tag.insert_text(ItemKey::MusicBrainzReleaseGroupId, mbid.clone());
    }
}

/// Replace (or add) the front-cover picture in the tag.
fn embed_cover_art(tag: &mut Tag, art: &AlbumArt) {
    let mime_type = match art.extension.to_lowercase().as_str() {
        "jpg" | "jpeg" => MimeType::Jpeg,
        "png" => MimeType::Png,
        "gif" => MimeType::Gif,
        "bmp" => MimeType::Bmp,
        "tiff" | "tif" => MimeType::Tiff,
        ext => MimeType::Unknown(format!("image/{ext}")),
    };

    tag.remove_picture_type(PictureType::CoverFront);

    let picture = Picture::new_unchecked(
        PictureType::CoverFront,
        Some(mime_type),
        None, // description
        art.data.clone(),
    );

    tag.push_picture(picture);
}

/// Convert a lofty [`ItemKey`] to a stable string suitable for building
/// `KYLE_*` backup key names.
///
/// Named variants use Vorbis-comment style names (all-caps). Unknown keys pass
/// through as-is. Any unrecognised named variants fall back to their debug
/// representation.
fn item_key_to_backup_name(key: &ItemKey) -> String {
    match key {
        ItemKey::TrackTitle => "TITLE".to_string(),
        ItemKey::TrackArtist => "ARTIST".to_string(),
        ItemKey::AlbumTitle => "ALBUM".to_string(),
        ItemKey::AlbumArtist => "ALBUMARTIST".to_string(),
        ItemKey::TrackNumber => "TRACKNUMBER".to_string(),
        ItemKey::TrackTotal => "TRACKTOTAL".to_string(),
        ItemKey::DiscNumber => "DISCNUMBER".to_string(),
        ItemKey::DiscTotal => "DISCTOTAL".to_string(),
        ItemKey::Year => "YEAR".to_string(),
        ItemKey::RecordingDate => "DATE".to_string(),
        ItemKey::Genre => "GENRE".to_string(),
        ItemKey::Comment => "COMMENT".to_string(),
        ItemKey::Composer => "COMPOSER".to_string(),
        ItemKey::MusicBrainzRecordingId => "MUSICBRAINZ_TRACKID".to_string(),
        ItemKey::MusicBrainzTrackId => "MUSICBRAINZ_RELEASETRACKID".to_string(),
        ItemKey::MusicBrainzReleaseId => "MUSICBRAINZ_ALBUMID".to_string(),
        ItemKey::MusicBrainzReleaseGroupId => "MUSICBRAINZ_RELEASEGROUPID".to_string(),
        ItemKey::MusicBrainzArtistId => "MUSICBRAINZ_ARTISTID".to_string(),
        ItemKey::MusicBrainzReleaseArtistId => "MUSICBRAINZ_ALBUMARTISTID".to_string(),
        ItemKey::Unknown(s) => s.clone(),
        other => format!("{other:?}"),
    }
}
