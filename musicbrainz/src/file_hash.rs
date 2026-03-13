use crate::Error;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Read;
use std::path::Path;

pub fn compute_sha256(path: &Path) -> Result<String, Error> {
    let mut file =
        File::open(path).map_err(|e| Error::Sha256(format!("open {}: {}", path.display(), e)))?;

    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 8192];

    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| Error::Sha256(format!("read {}: {}", path.display(), e)))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }

    Ok(format!("{:x}", hasher.finalize()))
}
