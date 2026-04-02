use std::{fs::File, io::Cursor, path::Path, time::Duration};
use symphonia::core::{
    errors::Error as SymphoniaError,
    formats::FormatOptions,
    io::{MediaSource, MediaSourceStream},
    meta::MetadataOptions,
    probe::Hint,
};
use thiserror::Error;

#[derive(Debug)]
pub struct Hertz(u32);

impl Hertz {
    pub fn new(value: u32) -> Self {
        Self(value)
    }
}

#[derive(Debug)]
pub struct AudioInfo {
    pub duration: Duration,
    /// Total number of PCM frames across all packets
    pub total_frames: usize,
    pub sample_rate: Hertz,
    /// Number of non fatal errors (common in damaged mp3s)
    pub soft_errors: usize,
}

#[derive(Debug, Error)]
pub enum AudioError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Unknown format: {0}")]
    UnknownFormat(&'static str),
    #[error("No tracks")]
    NoTracks,
    #[error("Packet error")]
    PacketError,
    #[error("Too many decode errors")]
    TooManyDecodeErrors { count: usize, last: &'static str },
    #[error("Unknown error")]
    Unknown,
    #[error("Empty")]
    Empty,
}

impl From<SymphoniaError> for AudioError {
    fn from(error: SymphoniaError) -> Self {
        match error {
            SymphoniaError::IoError(e) => AudioError::Io(e),
            SymphoniaError::Unsupported(feature) => AudioError::UnknownFormat(feature),
            SymphoniaError::DecodeError(_) => AudioError::PacketError,
            SymphoniaError::SeekError(_) => AudioError::PacketError,
            SymphoniaError::LimitError(_) | SymphoniaError::ResetRequired => AudioError::Unknown,
        }
    }
}

pub struct CheckOptions {
    /// how many consecutive decode errors are allowed before considering the file damaged
    /// mp3 files often have minor errors at boundaries
    pub max_soft_errors: usize,
}

impl Default for CheckOptions {
    fn default() -> Self {
        Self {
            max_soft_errors: 10,
        }
    }
}

pub fn check_file(path: impl AsRef<Path>) -> Result<AudioInfo, AudioError> {
    check_file_with_options(path, &CheckOptions::default())
}

pub fn check_file_with_options(
    path: impl AsRef<Path>,
    options: &CheckOptions,
) -> Result<AudioInfo, AudioError> {
    let path = path.as_ref();

    let file = File::open(path).map_err(|e| AudioError::Io(e))?;
    let mut hint = Hint::new();

    if let Some(extension) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(extension);
    }

    check_reader(file, hint, options)
}

pub fn check_reader_by_bytes(
    data: &[u8],
    extension_hint: Option<&str>,
    options: &CheckOptions,
) -> Result<AudioInfo, AudioError> {
    let cursor = Cursor::new(data.to_vec());
    let mut hint = Hint::new();
    if let Some(extension) = extension_hint {
        hint.with_extension(extension);
    }
    check_reader(cursor, hint, options)
}

fn check_reader<R>(
    src: R,
    extension_hint: Hint,
    options: &CheckOptions,
) -> Result<AudioInfo, AudioError>
where
    R: MediaSource + Send + Sync + 'static,
{
    let mss = MediaSourceStream::new(Box::new(src), Default::default());

    let format_options = FormatOptions {
        enable_gapless: false,
        ..Default::default()
    };

    let probed = symphonia::default::get_probe()
        .format(
            &extension_hint,
            mss,
            &format_options,
            &MetadataOptions::default(),
        )
        .map_err(|_| AudioError::UnknownFormat("unknown format"))?;

    let mut format = probed.format;

    let track = format.default_track().ok_or(AudioError::NoTracks)?;
    let track_id = track.id;

    let mut decoder =
        symphonia::default::get_codecs().make(&track.codec_params, &Default::default())?;

    let mut total_frames = 0;
    let mut sample_rate: Option<Hertz> = None;
    let mut soft_errors = 0;

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                break;
            }
            Err(SymphoniaError::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(e) => {
                return Err(e.into());
            }
        };

        if packet.track_id() != track_id {
            continue;
        }

        match decoder.decode(&packet) {
            Ok(buff) => {
                total_frames += buff.frames();
                if sample_rate.is_none() {
                    sample_rate = Some(Hertz(buff.spec().rate));
                }
            }
            Err(SymphoniaError::DecodeError(e)) => {
                soft_errors += 1;
                if soft_errors > options.max_soft_errors {
                    return Err(AudioError::TooManyDecodeErrors {
                        count: soft_errors,
                        last: e,
                    });
                }
            }
            Err(_e) => return Err(AudioError::PacketError),
        }
    }

    if total_frames == 0 {
        return Err(AudioError::Empty);
    }

    let sample_rate = sample_rate.unwrap_or(Hertz(0));
    let duration = if sample_rate.0 > 0 {
        Duration::from_secs_f64(total_frames as f64 / sample_rate.0 as f64)
    } else {
        Duration::from_secs(0)
    };

    Ok(AudioInfo {
        duration,
        total_frames,
        sample_rate,
        soft_errors,
    })
}
