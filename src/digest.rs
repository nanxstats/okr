//! Deterministic file and source tree digests.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::{Component, Path};

use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeDigest {
    /// Prefixed digest suitable for lockfile storage.
    pub digest: String,
    /// `/`-normalized relative path to unprefixed file SHA-256.
    pub files: BTreeMap<String, String>,
}

/// Hash bytes as lowercase hexadecimal SHA-256.
#[must_use]
pub fn sha256_bytes(bytes: impl AsRef<[u8]>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes.as_ref());
    encode_hex(hasher.finalize().as_slice())
}

/// Hash a file without loading it all into memory.
pub fn sha256_file(path: &Path) -> Result<String> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(encode_hex(hasher.finalize().as_slice()))
}

/// Hash the sorted `(relative-path, file-sha256)` inventory of a tree.
///
/// Each pair is encoded as `path<TAB>digest`; pairs are joined by `LF` with
/// no trailing newline. Paths are reconstructed from components so host path
/// separators never enter the digest.
pub fn tree_digest(root: &Path) -> Result<TreeDigest> {
    let mut files = BTreeMap::new();
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry.map_err(walk_error)?;
        if entry.path() == root || entry.file_type().is_dir() {
            continue;
        }
        if !entry.file_type().is_file() {
            return Err(Error::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "source tree contains unsupported non-file entry {}",
                    entry.path().display()
                ),
            )));
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|error| Error::Io(io::Error::new(io::ErrorKind::InvalidData, error)))?;
        let normalized = normalize_relative_path(relative)?;
        let file_digest = sha256_file(entry.path())?;
        files.insert(normalized, file_digest);
    }

    let mut inventory = String::new();
    for (index, (path, digest)) in files.iter().enumerate() {
        if index != 0 {
            inventory.push('\n');
        }
        inventory.push_str(path);
        inventory.push('\t');
        inventory.push_str(digest);
    }

    Ok(TreeDigest {
        digest: format!("sha256:{}", sha256_bytes(inventory)),
        files,
    })
}

fn normalize_relative_path(path: &Path) -> Result<String> {
    let mut normalized = String::new();
    for component in path.components() {
        let Component::Normal(part) = component else {
            return Err(Error::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "tree path is not relative and normalized: {}",
                    path.display()
                ),
            )));
        };
        let part = part.to_str().ok_or_else(|| {
            Error::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("tree path is not valid UTF-8: {}", path.display()),
            ))
        })?;
        if part.contains(['\n', '\r', '\t']) {
            return Err(Error::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("tree path contains a control separator: {}", path.display()),
            )));
        }
        if !normalized.is_empty() {
            normalized.push('/');
        }
        normalized.push_str(part);
    }
    if normalized.is_empty() {
        return Err(Error::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "tree file has an empty relative path",
        )));
    }
    Ok(normalized)
}

fn walk_error(error: walkdir::Error) -> Error {
    Error::Io(io::Error::other(error))
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").expect("writing to a String cannot fail");
    }
    output
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{sha256_bytes, sha256_file, tree_digest};

    #[test]
    fn byte_and_file_sha256_match_known_vectors() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("message");
        fs::write(&path, b"abc").unwrap();
        let expected = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(sha256_bytes(b"abc"), expected);
        assert_eq!(sha256_file(&path).unwrap(), expected);
    }

    #[test]
    fn tree_digest_is_stable_across_creation_order() {
        let first = tempdir().unwrap();
        let second = tempdir().unwrap();

        fs::create_dir(first.path().join("nested")).unwrap();
        fs::write(first.path().join("a.txt"), b"alpha\n").unwrap();
        fs::write(first.path().join("nested/b.txt"), b"beta\n").unwrap();

        fs::create_dir(second.path().join("nested")).unwrap();
        fs::write(second.path().join("nested/b.txt"), b"beta\n").unwrap();
        fs::write(second.path().join("a.txt"), b"alpha\n").unwrap();

        let first_digest = tree_digest(first.path()).unwrap();
        let second_digest = tree_digest(second.path()).unwrap();
        assert_eq!(first_digest, second_digest);
        assert_eq!(
            first_digest.digest,
            "sha256:48a0877e64f07f616fe2843d428f61aae6144d0c0428622b4299f5417f8e3c6c"
        );
    }

    #[test]
    fn inventory_paths_always_use_forward_slashes() {
        let directory = tempdir().unwrap();
        fs::create_dir_all(directory.path().join("one/two")).unwrap();
        fs::write(directory.path().join("one/two/file.R"), b"f <- 1\n").unwrap();
        let digest = tree_digest(directory.path()).unwrap();
        assert_eq!(
            digest.files.keys().map(String::as_str).collect::<Vec<_>>(),
            ["one/two/file.R"]
        );
    }

    #[test]
    fn empty_tree_has_the_sha256_of_an_empty_inventory() {
        let directory = tempdir().unwrap();
        assert_eq!(
            tree_digest(directory.path()).unwrap().digest,
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn changing_one_file_changes_its_hash_and_the_tree_hash() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("source.R");
        fs::write(&path, b"value <- 1\n").unwrap();
        let before = tree_digest(directory.path()).unwrap();
        fs::write(&path, b"value <- 2\n").unwrap();
        let after = tree_digest(directory.path()).unwrap();
        assert_ne!(before.files["source.R"], after.files["source.R"]);
        assert_ne!(before.digest, after.digest);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_rejected_instead_of_followed() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().unwrap();
        fs::write(directory.path().join("target"), b"contents").unwrap();
        symlink("target", directory.path().join("link")).unwrap();
        let error = tree_digest(directory.path()).unwrap_err();
        assert!(error.to_string().contains("unsupported non-file"));
    }
}
