use cq_core::{Error, Result};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
};
/// Persist directory entries as well as file data. Callers opening a store may
/// use sync_directory_chain to finish a previous interrupted initialization.
pub(crate) fn sync_directory_chain(path: &Path) -> Result<()> {
    let mut next = Some(path);
    while let Some(p) = next {
        if p.as_os_str().is_empty() {
            break;
        }
        fs::File::open(p)?.sync_all()?;
        next = p.parent();
    }
    Ok(())
}
pub(crate) fn create_directory(path: &Path) -> Result<()> {
    let existed = path.is_dir();
    fs::create_dir_all(path)?;
    if !existed {
        sync_directory_chain(path)?;
    }
    Ok(())
}
pub fn immutable_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::new("InvalidPath", "no parent"))?;
    create_directory(parent)?;
    let tmp = parent.join(format!(".tmp-{}", cq_capture_protocol::new_id()?));
    let result = (|| {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        match fs::hard_link(&tmp, path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if fs::read(path)? != bytes {
                    return Err(Error::new("ConflictingContent", path.display().to_string()));
                }
            }
            Err(e) => return Err(e.into()),
        }
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    let _ = fs::remove_file(tmp);
    result
}
pub fn valid_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())
}
