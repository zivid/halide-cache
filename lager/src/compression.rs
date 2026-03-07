
use std::io::{BufWriter, Read, Write};
use std::path::Path;

use crate::Result;

const COMPRESSION_LEVEL: i32 = 1;

pub(crate) fn write_dir<P: AsRef<Path>, W: Write>(dir_path: P, writer: W) -> Result<()> {
    let enc = zstd::Encoder::new(writer, COMPRESSION_LEVEL)?.auto_finish();

    let buf_writer = BufWriter::new(enc);

    let mut tar = tar::Builder::new(buf_writer);
    tar.append_dir_all(".", dir_path)?;
    tar.finish()?;

    Ok(())
}

pub(crate) fn read_dir<P: AsRef<Path>, R: Read>(dir_path: P, reader: R) -> Result<()> {
    let dec = zstd::Decoder::new(reader)?;

    let mut archive = tar::Archive::new(dec);
    archive.unpack(dir_path)?;

    Ok(())
}

pub(crate) fn write_file<W: Write>(file_path: &Path, writer: W) -> Result<()> {
    let mut enc = zstd::Encoder::new(writer, COMPRESSION_LEVEL)?.auto_finish();

    let mut file = std::fs::File::open(file_path)?;
    std::io::copy(&mut file, &mut enc)?;

    Ok(())
}

pub(crate) fn read_file<R: Read>(reader: R, file_path: &Path) -> Result<()> {
    let mut dec = zstd::Decoder::new(reader)?;

    let mut file = std::fs::File::create(file_path)?;
    std::io::copy(&mut dec, &mut file)?;

    Ok(())
}
