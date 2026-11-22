//! Content-addressed, compressed execution artifacts. Metadata never certifies
//! payload durability until the stream and containing directory are synced.
use crate::raw::Image;
use cq_core::{Error, Result};
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{BufReader, BufWriter, Write},
    path::Path,
};
pub(crate) fn retain<T: Serialize>(directory: &Path, value: &T) -> Result<Image> {
    crate::atomic::create_directory(directory)?;
    let mut tmp = tempfile::NamedTempFile::new_in(directory)?;
    {
        let mut out = lz4_flex::frame::FrameEncoder::new(BufWriter::new(tmp.as_file_mut()));
        serde_json::to_writer(&mut out, value)?;
        let mut out = out.finish().map_err(|e| Error::Io(e.into()))?;
        out.flush()?;
    }
    tmp.as_file().sync_all()?;
    let mut hash = Sha256::new();
    let bytes = std::io::copy(&mut File::open(tmp.path())?, &mut hash)?;
    let sha256 = format!("{:x}", hash.finalize());
    let path = directory.join(&sha256);
    if !path.exists() {
        std::fs::hard_link(tmp.path(), &path)?;
    }
    File::open(directory)?.sync_all()?;
    Ok(Image {
        path,
        bytes,
        sha256,
    })
}
pub(crate) fn restore<T: DeserializeOwned>(directory: &Path, image: &Image) -> Result<T> {
    let path = image.path.canonicalize()?;
    if !path.starts_with(directory.canonicalize()?) {
        return Err(Error::new(
            "InvalidStageArtifact",
            "outside execution directory",
        ));
    }
    let mut hash = Sha256::new();
    let bytes = std::io::copy(&mut File::open(&path)?, &mut hash)?;
    if bytes != image.bytes || format!("{:x}", hash.finalize()) != image.sha256 {
        return Err(Error::new("DigestMismatch", "execution artifact"));
    }
    Ok(serde_json::from_reader(
        lz4_flex::frame::FrameDecoder::new(BufReader::new(File::open(path)?)),
    )?)
}
