use lager::{Address, Lager};
use std::fs::File;
use std::io::{Error, ErrorKind, Read, Write};
use std::path::Path;

const COMPRESSION_LEVEL: i32 = 1;

pub fn store(lager: &Lager, address: &Address, files: &[&Path]) -> lager::Result<()> {
    lager.store_at(address, |out| pack(files, out)).map(|_| ())
}

pub fn restore(lager: &Lager, address: &Address, destinations: &[&Path]) -> lager::Result<()> {
    Ok(unpack(lager.retrieve(address)?, destinations)?)
}

fn pack<W: Write>(files: &[&Path], writer: W) -> std::io::Result<()> {
    let mut archive = tar::Builder::new(zstd::Encoder::new(writer, COMPRESSION_LEVEL)?);
    archive.mode(tar::HeaderMode::Deterministic);
    for path in files {
        let name = path.file_name().ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidInput,
                format!("cannot store {}: it has no file name", path.display()),
            )
        })?;
        archive.append_path_with_name(path, name)?;
    }
    archive.into_inner()?.finish()?;
    Ok(())
}

fn unpack<R: Read>(reader: R, destinations: &[&Path]) -> std::io::Result<()> {
    let corrupt = |msg| Error::new(ErrorKind::InvalidData, msg);
    let mut archive = tar::Archive::new(zstd::Decoder::new(reader)?);
    let mut entries = archive.entries()?;
    for path in destinations {
        let mut entry = entries
            .next()
            .ok_or_else(|| corrupt("entry holds fewer files than expected"))??;
        std::io::copy(&mut entry, &mut File::create(path)?)?;
    }
    if entries.next().is_some() {
        return Err(corrupt("entry holds more files than expected"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn files(dir: &Path, contents: &[&str]) -> Vec<PathBuf> {
        contents
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let p = dir.join(format!("in{i}"));
                std::fs::write(&p, c).unwrap();
                p
            })
            .collect()
    }

    fn refs(paths: &[PathBuf]) -> Vec<&Path> {
        paths.iter().map(PathBuf::as_path).collect()
    }

    fn packed(dir: &Path, contents: &[&str]) -> Vec<u8> {
        let mut bytes = Vec::new();
        pack(&refs(&files(dir, contents)), &mut bytes).unwrap();
        bytes
    }

    #[test]
    fn several_files_round_trip_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = packed(dir.path(), &["object", "", "header"]);

        let outs: Vec<PathBuf> = (0..3).map(|i| dir.path().join(format!("out{i}"))).collect();
        unpack(bytes.as_slice(), &refs(&outs)).unwrap();
        for (i, expected) in ["object", "", "header"].iter().enumerate() {
            assert_eq!(std::fs::read_to_string(&outs[i]).unwrap(), *expected);
        }
    }

    #[test]
    fn unpacking_a_different_number_of_files_is_invalid_data() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = packed(dir.path(), &["a", "b"]);

        for n in [1, 3] {
            let outs: Vec<PathBuf> = (0..n).map(|i| dir.path().join(format!("o{i}"))).collect();
            let err = unpack(bytes.as_slice(), &refs(&outs)).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::InvalidData);
        }
    }

    #[test]
    fn restoring_from_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("lager");
        std::fs::create_dir_all(&root).unwrap();
        let lager = Lager::new(&root).unwrap();
        let address = Address::from_hex(&"ab".repeat(64)).unwrap();
        store(&lager, &address, &refs(&files(dir.path(), &["payload"]))).unwrap();

        let out = dir.path().join("out");
        restore(&lager, &address, &[&out]).unwrap();
        assert_eq!(std::fs::read(out).unwrap(), b"payload");
    }
}
