use crate::Error;
use std::path::Path;
use std::process::Command;

/// Run `fpcalc` on `path` and return `(fingerprint, duration_secs)`.
pub fn fingerprint(path: &Path) -> Result<(String, u32), Error> {
    if which::which("fpcalc").is_err() {
        return Err(Error::FpcalcNotFound);
    }

    let output = Command::new("fpcalc").arg(path).output()?;
    let stdout = String::from_utf8(output.stdout)?;

    let mut fp: Option<String> = None;
    let mut duration: Option<u32> = None;

    for line in stdout.lines() {
        if let Some(v) = line.strip_prefix("FINGERPRINT=") {
            fp = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix("DURATION=") {
            duration = Some(v.parse()?);
        }
    }

    Ok((
        fp.ok_or(Error::FpcalcMissingFingerprint)?,
        duration.ok_or(Error::FpcalcMissingDuration)?,
    ))
}
