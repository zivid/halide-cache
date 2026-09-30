use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

use crate::{Error, Result};

const COMPRESSION_LEVEL: i32 = 1;

pub(crate) fn write_files<W: Write>(files: &[&Path], writer: W) -> Result<()> {
    let mut archive = tar::Builder::new(zstd::Encoder::new(writer, COMPRESSION_LEVEL)?);
    archive.mode(tar::HeaderMode::Deterministic);
    for path in files {
        let name = path.file_name().ok_or_else(|| Error::Runtime {
            msg: format!("cannot store {}: it has no file name", path.display()),
        })?;
        archive.append_path_with_name(path, name)?;
    }
    archive.into_inner()?.finish()?;
    Ok(())
}

pub(crate) fn read_files<R: Read>(reader: R, destinations: &[&Path]) -> Result<()> {
    let mut archive = tar::Archive::new(zstd::Decoder::new(reader)?);
    let mut entries = archive.entries()?;
    for path in destinations {
        let mut entry = entries
            .next()
            .ok_or(Error::Corrupt("entry holds fewer files than expected"))??;
        std::io::copy(&mut entry, &mut File::create(path)?)?;
    }
    if entries.next().is_some() {
        return Err(Error::Corrupt("entry holds more files than expected"));
    }
    Ok(())
}
