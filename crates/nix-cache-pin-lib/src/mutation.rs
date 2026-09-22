//! Cooperating writers share a persistent inode. Never delete the anchor:
//! replacing it could let another writer lock a different inode concurrently.
use nix::fcntl::{Flock, FlockArg};
use std::{
    fs::{self, File, OpenOptions},
    io,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

pub(crate) struct Mutation {
    _lock: Flock<File>,
}

impl Mutation {
    pub(crate) fn acquire(directory: &Path) -> io::Result<Self> {
        let directory = directory.canonicalize()?;
        let git = std::process::Command::new("git")
            .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .current_dir(&directory)
            .output()?;
        let anchor_dir = if git.status.success() {
            PathBuf::from(
                String::from_utf8(git.stdout)
                    .map_err(io::Error::other)?
                    .trim(),
            )
        } else {
            directory
        };
        let anchor = anchor_dir.join("nix-cache-pin.mutation.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(nix::fcntl::OFlag::O_NOFOLLOW.bits())
            .open(&anchor)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::other(
                "cache-pin lock anchor is not a regular file",
            ));
        }
        let lock = Flock::lock(file, FlockArg::LockExclusiveNonblock).map_err(|(_, error)| {
            io::Error::other(format!(
                "cache-pin mutation lock {} unavailable: {error}",
                anchor.display()
            ))
        })?;
        Ok(Self { _lock: lock })
    }
}

pub(crate) fn read(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

pub(crate) fn unchanged(path: &Path, before: &Option<Vec<u8>>) -> io::Result<()> {
    if read(path)? != *before {
        return Err(io::Error::other(format!(
            "{} changed while cache pins were being prepared; refusing replacement",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn competing_writers_and_foreign_edits_fail_closed() {
        let root = std::env::temp_dir().join(format!("cache-pin-mutation-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let first = Mutation::acquire(&root).unwrap();
        assert!(Mutation::acquire(&root).is_err());
        let path = root.join("flake.lock");
        fs::write(&path, b"before").unwrap();
        let baseline = read(&path).unwrap();
        fs::write(&path, b"foreign edit").unwrap();
        assert!(unchanged(&path, &baseline).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"foreign edit");
        drop(first);
        let next = Mutation::acquire(&root).unwrap();
        drop(next);
        assert!(root.join("nix-cache-pin.mutation.lock").exists());
        fs::remove_dir_all(root).unwrap();
    }
}
