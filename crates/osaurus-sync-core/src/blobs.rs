//! Encrypted blobs on disk, addressed by the BLAKE3 hash of their ciphertext.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::crypto;
use crate::env::{Env, random_array};
use crate::error::{Error, Result};
use crate::payload::BlobRef;

#[derive(Clone, Debug)]
pub struct BlobStore {
    dir: PathBuf,
}

impl BlobStore {
    pub fn new(dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&dir)?;
        fs::create_dir_all(dir.join("plain"))?;
        Ok(Self { dir })
    }

    pub fn path(&self, hash: &[u8; 32]) -> PathBuf {
        self.dir.join(hex::encode(hash))
    }

    fn part_path(&self, hash: &[u8; 32]) -> PathBuf {
        self.dir.join(format!("{}.part", hex::encode(hash)))
    }

    pub fn has(&self, hash: &[u8; 32]) -> bool {
        self.path(hash).exists()
    }

    /// Encrypts a file with a fresh per-blob key and stores it.
    pub fn import(&self, env: &dyn Env, src: &Path, media_type: &str, name: &str) -> Result<BlobRef> {
        let key: [u8; 32] = random_array(env);
        let tmp = self.dir.join(format!("import-{}.tmp", hex::encode(random_array::<8>(env))));
        let enc = crypto::encrypt_blob_file(env, &key, src, &tmp)?;
        fs::rename(&tmp, self.path(&enc.hash))?;
        Ok(BlobRef { hash: enc.hash, size: enc.size, plain_size: enc.plain_size, media_type: media_type.into(), name: name.into(), key })
    }

    pub fn import_bytes(&self, env: &dyn Env, bytes: &[u8], media_type: &str, name: &str) -> Result<BlobRef> {
        let tmp = self.dir.join(format!("bytes-{}.tmp", hex::encode(random_array::<8>(env))));
        fs::write(&tmp, bytes)?;
        let r = self.import(env, &tmp, media_type, name);
        let _ = fs::remove_file(&tmp);
        r
    }

    /// Bytes of a partial download, for resuming.
    pub fn partial_len(&self, hash: &[u8; 32]) -> u64 {
        fs::metadata(self.part_path(hash)).map(|m| m.len()).unwrap_or(0)
    }

    pub fn append_partial(&self, hash: &[u8; 32], offset: u64, data: &[u8]) -> Result<()> {
        let mut f = OpenOptions::new().create(true).truncate(false).write(true).open(self.part_path(hash))?;
        f.set_len(offset)?;
        f.seek(SeekFrom::Start(offset))?;
        f.write_all(data)?;
        Ok(())
    }

    /// Verifies a completed download and moves it into place.
    pub fn finalize(&self, hash: &[u8; 32]) -> Result<()> {
        let part = self.part_path(hash);
        if crypto::hash_file(&part)? != *hash {
            let _ = fs::remove_file(&part);
            return Err(Error::Crypto("blob hash mismatch".into()));
        }
        fs::rename(part, self.path(hash))?;
        Ok(())
    }

    pub fn read_range(&self, hash: &[u8; 32], offset: u64, max: usize) -> Result<Vec<u8>> {
        let mut f = File::open(self.path(hash)).map_err(|_| Error::NotFound("blob".into()))?;
        f.seek(SeekFrom::Start(offset))?;
        let mut buf = vec![0u8; max];
        let mut n = 0;
        while n < max {
            match f.read(&mut buf[n..])? {
                0 => break,
                k => n += k,
            }
        }
        buf.truncate(n);
        Ok(buf)
    }

    pub fn size(&self, hash: &[u8; 32]) -> Option<u64> {
        fs::metadata(self.path(hash)).ok().map(|m| m.len())
    }

    /// Decrypts a stored blob into the plaintext cache and returns the path.
    pub fn open_plain(&self, r: &BlobRef) -> Result<PathBuf> {
        let safe: String = r.name.chars().map(|c| if c.is_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' }).collect();
        let dst = self.dir.join("plain").join(format!("{}-{}", &hex::encode(r.hash)[..16], safe));
        if dst.exists() {
            // Opening counts as use for eviction.
            if let Ok(f) = File::options().append(true).open(&dst) {
                let _ = f.set_modified(SystemTime::now());
            }
        } else {
            crypto::decrypt_blob_file(&r.key, &self.path(&r.hash), &dst)?;
        }
        Ok(dst)
    }

    /// Opens a decrypted copy and keeps the plaintext cache under `cap` bytes.
    pub fn open_plain_capped(&self, r: &BlobRef, cap: u64) -> Result<PathBuf> {
        let dst = self.open_plain(r)?;
        self.evict_plain(cap, Some(&dst));
        Ok(dst)
    }

    /// Deletes the least recently opened decrypted copies until the cache fits in `cap` bytes.
    /// `keep` is never deleted. Returns how many files were removed.
    pub fn evict_plain(&self, cap: u64, keep: Option<&Path>) -> usize {
        let Ok(entries) = fs::read_dir(self.dir.join("plain")) else { return 0 };
        let mut files: Vec<(SystemTime, u64, PathBuf)> = entries
            .flatten()
            .filter_map(|e| {
                let m = e.metadata().ok()?;
                m.is_file().then(|| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len(), e.path()))
            })
            .collect();
        let mut total: u64 = files.iter().map(|f| f.1).sum();
        if total <= cap {
            return 0;
        }
        files.sort();
        let mut removed = 0;
        for (_, len, path) in files {
            if total <= cap {
                break;
            }
            if keep.is_some_and(|k| k == path) {
                continue;
            }
            if fs::remove_file(&path).is_ok() {
                total -= len;
                removed += 1;
            }
        }
        removed
    }

    /// Bytes of encrypted blobs (including partial downloads) and of the decrypted cache.
    pub fn usage(&self) -> (u64, u64) {
        let sum = |dir: &Path| -> u64 {
            fs::read_dir(dir).map_or(0, |entries| entries.flatten().filter_map(|e| e.metadata().ok()).filter(|m| m.is_file()).map(|m| m.len()).sum())
        };
        (sum(&self.dir), sum(&self.dir.join("plain")))
    }

    pub fn delete(&self, hash: &[u8; 32]) {
        let _ = fs::remove_file(self.path(hash));
        let _ = fs::remove_file(self.part_path(hash));
        if let Ok(entries) = fs::read_dir(self.dir.join("plain")) {
            let prefix = hex::encode(hash)[..16].to_string();
            for e in entries.flatten() {
                if e.file_name().to_string_lossy().starts_with(&prefix) {
                    let _ = fs::remove_file(e.path());
                }
            }
        }
    }
}
