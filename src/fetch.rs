//! Synchronous acquisition and content-addressed caching.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use flate2::{Compression, GzBuilder};
use reqwest::blocking::{Client, Response};
use serde::de::DeserializeOwned;
use tar::{Builder as TarBuilder, Header};
use tempfile::NamedTempFile;

use crate::digest::{sha256_bytes, sha256_file, tree_digest};
use crate::progress::SyncProgress;
use crate::{Error, Result};

const ARTIFACTS_DIRECTORY: &str = "artifacts";
const REFERENCES_DIRECTORY: &str = "refs";

#[derive(Debug, Clone)]
pub struct Cache {
    root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedArtifact {
    pub path: PathBuf,
    pub sha256: String,
    pub from_cache: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct CacheStats {
    pub artifacts: u64,
    pub bytes: u64,
}

impl Cache {
    pub fn from_environment() -> Result<Self> {
        if let Some(root) = std::env::var_os("OKR_CACHE_DIR") {
            return Ok(Self::new(root));
        }
        let home = std::env::var_os("HOME").ok_or_else(|| {
            Error::Fetch(
                "cannot determine the cache directory: set OKR_CACHE_DIR (HOME is unset)".into(),
            )
        })?;
        Ok(Self::new(PathBuf::from(home).join(".cache/okr")))
    }

    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn get(&self, digest: &str) -> Result<Option<CachedArtifact>> {
        let digest = normalize_digest(digest)?;
        let path = self.artifact_path(digest);
        if !path.try_exists()? {
            return Ok(None);
        }
        let actual = sha256_file(&path)?;
        if actual != digest {
            return Err(Error::Fetch(format!(
                "cached artifact {} failed SHA-256 verification (expected {digest}, found {actual}); remove it and retry",
                path.display()
            )));
        }
        Ok(Some(CachedArtifact {
            path,
            sha256: digest.to_owned(),
            from_cache: true,
        }))
    }

    pub fn lookup(&self, key: &str) -> Result<Option<CachedArtifact>> {
        let path = self.reference_path(key);
        if !path.try_exists()? {
            return Ok(None);
        }
        let digest = fs::read_to_string(&path)?;
        self.get(digest.trim())
    }

    pub fn put_bytes(
        &self,
        key: &str,
        bytes: &[u8],
        expected_sha256: Option<&str>,
    ) -> Result<CachedArtifact> {
        self.put_reader(key, io::Cursor::new(bytes), expected_sha256)
    }

    pub fn put_file(
        &self,
        key: &str,
        source: &Path,
        expected_sha256: Option<&str>,
    ) -> Result<CachedArtifact> {
        self.put_reader(key, BufReader::new(File::open(source)?), expected_sha256)
    }

    pub fn stats(&self) -> Result<CacheStats> {
        let directory = self.root.join(ARTIFACTS_DIRECTORY);
        if !directory.try_exists()? {
            return Ok(CacheStats::default());
        }
        let mut stats = CacheStats::default();
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                stats.artifacts += 1;
                stats.bytes += entry.metadata()?.len();
            }
        }
        Ok(stats)
    }

    /// Cache a clone-produced, already-pruned tree as a normalized tarball.
    pub fn put_normalized_tree(&self, key: &str, root: &Path) -> Result<CachedArtifact> {
        self.ensure_directories()?;
        let inventory = tree_digest(root)?;
        let mut temporary = NamedTempFile::new_in(&self.root)?;
        {
            let encoder = GzBuilder::new()
                .mtime(0)
                .write(temporary.as_file_mut(), Compression::default());
            let mut archive = TarBuilder::new(encoder);
            for relative in inventory.files.keys() {
                let source = relative
                    .split('/')
                    .fold(root.to_path_buf(), |path, part| path.join(part));
                let mut file = File::open(&source)?;
                let mut header = Header::new_gnu();
                header.set_size(file.metadata()?.len());
                header.set_mode(0o644);
                header.set_uid(0);
                header.set_gid(0);
                header.set_mtime(0);
                header.set_entry_type(tar::EntryType::Regular);
                header.set_cksum();
                archive.append_data(&mut header, format!("source/{relative}"), &mut file)?;
            }
            let encoder = archive.into_inner()?;
            encoder.finish()?;
        }
        temporary.as_file().sync_all()?;
        self.put_file(key, temporary.path(), None)
    }

    fn put_reader(
        &self,
        key: &str,
        mut reader: impl Read,
        expected_sha256: Option<&str>,
    ) -> Result<CachedArtifact> {
        self.ensure_directories()?;
        let expected = expected_sha256.map(normalize_digest).transpose()?;
        let mut temporary = NamedTempFile::new_in(self.root.join(ARTIFACTS_DIRECTORY))?;
        let mut hasher = sha2::Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            use sha2::Digest as _;
            hasher.update(&buffer[..count]);
            temporary.write_all(&buffer[..count])?;
        }
        use sha2::Digest as _;
        let digest = encode_hex(hasher.finalize().as_slice());
        if let Some(expected) = expected
            && digest != expected
        {
            return Err(Error::Fetch(format!(
                "SHA-256 mismatch: expected {expected}, downloaded {digest}"
            )));
        }
        temporary.flush()?;
        temporary.as_file().sync_all()?;

        let target = self.artifact_path(&digest);
        if target.try_exists()? {
            let actual = sha256_file(&target)?;
            if actual != digest {
                return Err(Error::Fetch(format!(
                    "cached artifact {} failed SHA-256 verification (expected {digest}, found {actual}); remove it and retry",
                    target.display()
                )));
            }
        } else if let Err(error) = temporary.persist_noclobber(&target)
            && !target.try_exists()?
        {
            return Err(Error::Io(error.error));
        }

        self.write_reference(key, &digest)?;
        Ok(CachedArtifact {
            path: target,
            sha256: digest,
            from_cache: false,
        })
    }

    fn write_reference(&self, key: &str, digest: &str) -> Result<()> {
        let path = self.reference_path(key);
        let mut temporary = NamedTempFile::new_in(self.root.join(REFERENCES_DIRECTORY))?;
        writeln!(temporary, "{digest}")?;
        temporary.flush()?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(path)
            .map_err(|error| Error::Io(error.error))?;
        Ok(())
    }

    fn ensure_directories(&self) -> Result<()> {
        fs::create_dir_all(self.root.join(ARTIFACTS_DIRECTORY))?;
        fs::create_dir_all(self.root.join(REFERENCES_DIRECTORY))?;
        Ok(())
    }

    fn artifact_path(&self, digest: &str) -> PathBuf {
        self.root.join(ARTIFACTS_DIRECTORY).join(digest)
    }

    fn reference_path(&self, key: &str) -> PathBuf {
        self.root.join(REFERENCES_DIRECTORY).join(sha256_bytes(key))
    }
}

pub struct Fetcher {
    cache: Cache,
    client: Client,
    offline: bool,
    progress: Option<SyncProgress>,
}

impl Fetcher {
    pub fn new(cache: Cache, offline: bool) -> Result<Self> {
        let client = Client::builder()
            .user_agent(concat!("okr/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| Error::Fetch(format!("could not initialize HTTP client: {error}")))?;
        Ok(Self {
            cache,
            client,
            offline,
            progress: None,
        })
    }

    #[must_use]
    pub(crate) fn with_progress(mut self, progress: SyncProgress) -> Self {
        self.progress = Some(progress);
        self
    }

    #[must_use]
    pub fn cache(&self) -> &Cache {
        &self.cache
    }

    #[must_use]
    pub const fn is_offline(&self) -> bool {
        self.offline
    }

    pub fn fetch_url(
        &self,
        url: &str,
        expected_sha256: Option<&str>,
        label: &str,
    ) -> Result<CachedArtifact> {
        self.fetch_url_if_exists(url, expected_sha256, label)?
            .ok_or_else(|| {
                Error::Fetch(format!(
                    "could not fetch {label} from {url}: HTTP 404 Not Found"
                ))
            })
    }

    /// Fetch an artifact, returning `None` only when the server reports that
    /// the URL does not exist. Successful responses use the normal verified
    /// content-addressed cache.
    pub(crate) fn fetch_url_if_exists(
        &self,
        url: &str,
        expected_sha256: Option<&str>,
        label: &str,
    ) -> Result<Option<CachedArtifact>> {
        let key = format!("url:{url}");
        if let Some(expected) = expected_sha256
            && let Some(hit) = self.cache.get(expected)?
        {
            return Ok(Some(hit));
        }
        if let Some(hit) = self.cache.lookup(&key)? {
            if let Some(expected) = expected_sha256
                && hit.sha256 != normalize_digest(expected)?
            {
                return Err(Error::Fetch(format!(
                    "cached {label} has SHA-256 {}, expected {}",
                    hit.sha256,
                    normalize_digest(expected)?
                )));
            }
            return Ok(Some(hit));
        }
        if self.offline {
            return Err(Error::Fetch(format!(
                "offline mode: missing cached artifact for {label} ({url})"
            )));
        }

        if url.starts_with("file://") {
            let path = file_url_path(url)?;
            return self.cache.put_file(&key, &path, expected_sha256).map(Some);
        }

        let response = self.client.get(url).send().map_err(|error| {
            Error::Fetch(format!("could not fetch {label} from {url}: {error}"))
        })?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(Error::Fetch(format!(
                "could not fetch {label} from {url}: HTTP {}",
                response.status()
            )));
        }
        self.cache_response(&key, response, expected_sha256, label)
            .map(Some)
    }

    pub fn fetch_url_with_bearer(
        &self,
        url: &str,
        token: &str,
        expected_sha256: Option<&str>,
        label: &str,
    ) -> Result<CachedArtifact> {
        let key = format!("url:{url}");
        if let Some(expected) = expected_sha256
            && let Some(hit) = self.cache.get(expected)?
        {
            return Ok(hit);
        }
        if let Some(hit) = self.cache.lookup(&key)? {
            if let Some(expected) = expected_sha256
                && hit.sha256 != normalize_digest(expected)?
            {
                return Err(Error::Fetch(format!(
                    "cached {label} has SHA-256 {}, expected {}",
                    hit.sha256,
                    normalize_digest(expected)?
                )));
            }
            return Ok(hit);
        }
        if self.offline {
            return Err(Error::Fetch(format!(
                "offline mode: missing cached artifact for {label} ({url})"
            )));
        }
        let response = self
            .client
            .get(url)
            .bearer_auth(token)
            .header("Accept", "application/vnd.github+json")
            .send()
            .map_err(|error| {
                Error::Fetch(format!("could not fetch {label} from {url}: {error}"))
            })?;
        if !response.status().is_success() {
            return Err(Error::Fetch(format!(
                "could not fetch {label} from {url}: HTTP {}",
                response.status()
            )));
        }
        self.cache_response(&key, response, expected_sha256, label)
    }

    pub fn get_json<T: DeserializeOwned>(&self, url: &str, label: &str) -> Result<T> {
        if self.offline {
            return Err(Error::Fetch(format!(
                "offline mode: cannot query {label} ({url})"
            )));
        }
        let response = self
            .client
            .get(url)
            .header("Accept", "application/json")
            .send()
            .map_err(|error| Error::Fetch(format!("could not query {label} at {url}: {error}")))?;
        if !response.status().is_success() {
            return Err(Error::Fetch(format!(
                "could not query {label} at {url}: HTTP {}",
                response.status()
            )));
        }
        response
            .json()
            .map_err(|error| Error::Fetch(format!("invalid {label} response from {url}: {error}")))
    }

    fn cache_response(
        &self,
        key: &str,
        response: Response,
        expected_sha256: Option<&str>,
        label: &str,
    ) -> Result<CachedArtifact> {
        let transfer = self
            .progress
            .as_ref()
            .map(|progress| progress.download(label, response.content_length()));
        let result = if let Some(transfer) = &transfer {
            self.cache
                .put_reader(key, transfer.wrap_read(response), expected_sha256)
        } else {
            self.cache.put_reader(key, response, expected_sha256)
        };
        if let Some(transfer) = transfer {
            transfer.finish();
        }
        result
    }
}

fn normalize_digest(digest: &str) -> Result<&str> {
    let digest = digest.strip_prefix("sha256:").unwrap_or(digest);
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(Error::Fetch(format!(
            "invalid SHA-256 cache key `{digest}`"
        )));
    }
    Ok(digest)
}

fn file_url_path(url: &str) -> Result<PathBuf> {
    let encoded = url
        .strip_prefix("file://")
        .ok_or_else(|| Error::Fetch(format!("invalid local fixture URL `{url}`")))?;
    let encoded = encoded.strip_prefix("localhost").unwrap_or(encoded);
    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err(Error::Fetch(format!("invalid percent escape in `{url}`")));
            }
            let high = hex_value(bytes[index + 1]);
            let low = hex_value(bytes[index + 2]);
            let (Some(high), Some(low)) = (high, low) else {
                return Err(Error::Fetch(format!("invalid percent escape in `{url}`")));
            };
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    let decoded = String::from_utf8(decoded)
        .map_err(|_| Error::Fetch(format!("file URL path is not UTF-8: `{url}`")))?;
    Ok(PathBuf::from(OsString::from(decoded)))
}

const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").expect("writing to a String cannot fail");
    }
    output
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::thread;

    use flate2::read::GzDecoder;
    use tempfile::tempdir;

    use super::{Cache, Fetcher};
    use crate::digest::sha256_bytes;

    #[test]
    fn content_is_stored_by_verified_digest() {
        let directory = tempdir().unwrap();
        let cache = Cache::new(directory.path());
        let digest = sha256_bytes(b"artifact");
        let stored = cache
            .put_bytes("fixture", b"artifact", Some(&digest))
            .unwrap();
        assert!(!stored.from_cache);
        assert_eq!(stored.sha256, digest);
        assert_eq!(fs::read(&stored.path).unwrap(), b"artifact");

        let hit = cache.lookup("fixture").unwrap().unwrap();
        assert!(hit.from_cache);
        assert_eq!(hit.path, stored.path);
    }

    #[test]
    fn digest_mismatch_is_never_committed() {
        let directory = tempdir().unwrap();
        let cache = Cache::new(directory.path());
        let error = cache
            .put_bytes("fixture", b"artifact", Some(&"0".repeat(64)))
            .unwrap_err();
        assert!(error.to_string().contains("SHA-256 mismatch"));
        assert_eq!(cache.stats().unwrap().artifacts, 0);
    }

    #[test]
    fn cache_hits_are_reverified() {
        let directory = tempdir().unwrap();
        let cache = Cache::new(directory.path());
        let stored = cache.put_bytes("fixture", b"original", None).unwrap();
        fs::write(&stored.path, b"corrupt").unwrap();
        let error = cache.lookup("fixture").unwrap_err();
        assert!(error.to_string().contains("failed SHA-256 verification"));
    }

    #[test]
    fn offline_fetch_names_a_missing_artifact() {
        let directory = tempdir().unwrap();
        let fetcher = Fetcher::new(Cache::new(directory.path()), true).unwrap();
        let error = fetcher
            .fetch_url("https://example.test/pkg.tar.gz", None, "package pkg")
            .unwrap_err();
        assert!(error.to_string().contains("offline mode"));
        assert!(error.to_string().contains("package pkg"));
    }

    #[test]
    fn optional_fetch_distinguishes_http_not_found_from_other_failures() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1024];
            let bytes_read = stream.read(&mut request).unwrap();
            assert!(bytes_read > 0);
            stream
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
        });
        let directory = tempdir().unwrap();
        let fetcher = Fetcher::new(Cache::new(directory.path()), false).unwrap();
        let result = fetcher
            .fetch_url_if_exists(
                &format!("http://{address}/missing"),
                None,
                "missing fixture",
            )
            .unwrap();

        server.join().unwrap();
        assert_eq!(result, None);
        assert_eq!(fetcher.cache().stats().unwrap().artifacts, 0);
    }

    #[test]
    fn file_fixture_fetch_populates_an_offline_hit() {
        let root = tempdir().unwrap();
        let source = root.path().join("fixture tarball");
        fs::write(&source, b"fixture bytes").unwrap();
        let encoded = source.to_string_lossy().replace(' ', "%20");
        let url = format!("file://{encoded}");
        let cache = Cache::new(root.path().join("cache"));

        let online = Fetcher::new(cache.clone(), false)
            .unwrap()
            .fetch_url(&url, None, "fixture")
            .unwrap();
        fs::remove_file(source).unwrap();
        let offline = Fetcher::new(cache, true)
            .unwrap()
            .fetch_url(&url, Some(&online.sha256), "fixture")
            .unwrap();
        assert!(offline.from_cache);
        assert_eq!(offline.sha256, online.sha256);
    }

    #[test]
    fn normalized_tree_archives_are_byte_stable() {
        let first = tempdir().unwrap();
        let second = tempdir().unwrap();
        fs::create_dir(first.path().join("R")).unwrap();
        fs::write(first.path().join("DESCRIPTION"), b"Package: pkg\n").unwrap();
        fs::write(first.path().join("R/code.R"), b"f <- function() 1\n").unwrap();
        fs::create_dir(second.path().join("R")).unwrap();
        fs::write(second.path().join("R/code.R"), b"f <- function() 1\n").unwrap();
        fs::write(second.path().join("DESCRIPTION"), b"Package: pkg\n").unwrap();

        let cache_root = tempdir().unwrap();
        let cache = Cache::new(cache_root.path());
        let one = cache
            .put_normalized_tree("clone:one", first.path())
            .unwrap();
        let two = cache
            .put_normalized_tree("clone:two", second.path())
            .unwrap();
        assert_eq!(one.sha256, two.sha256);

        let file = fs::File::open(one.path).unwrap();
        let mut archive = tar::Archive::new(GzDecoder::new(file));
        let entries = archive
            .entries()
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (
                    entry.path().unwrap().to_string_lossy().into_owned(),
                    entry.header().mtime().unwrap(),
                    entry.header().uid().unwrap(),
                    entry.header().gid().unwrap(),
                    entry.header().mode().unwrap(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            entries,
            [
                ("source/DESCRIPTION".into(), 0, 0, 0, 0o644),
                ("source/R/code.R".into(), 0, 0, 0, 0o644),
            ]
        );
    }

    #[test]
    fn cache_stats_count_only_content_artifacts() {
        let directory = tempdir().unwrap();
        let cache = Cache::new(directory.path());
        cache.put_bytes("one", b"123", None).unwrap();
        cache.put_bytes("two", b"4567", None).unwrap();
        let stats = cache.stats().unwrap();
        assert_eq!(stats.artifacts, 2);
        assert_eq!(stats.bytes, 7);
    }
}
