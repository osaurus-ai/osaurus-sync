//! Signatures, hashing, payload encryption, key sealing and blob encryption.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use hkdf::Hkdf;
use sha2::Sha256;

use crate::env::{Env, random_array};
use crate::error::{Error, Result};

pub fn hash(bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(bytes).as_bytes()
}

pub fn sign(key: &SigningKey, msg: &[u8]) -> [u8; 64] {
    key.sign(msg).to_bytes()
}

pub fn verify(public: &[u8; 32], msg: &[u8], sig: &[u8; 64]) -> bool {
    let Ok(vk) = VerifyingKey::from_bytes(public) else { return false };
    vk.verify_strict(msg, &Signature::from_bytes(sig)).is_ok()
}

fn cipher(key: &[u8; 32]) -> XChaCha20Poly1305 {
    XChaCha20Poly1305::new_from_slice(key).expect("32-byte key")
}

pub fn aead_seal(key: &[u8; 32], nonce: &[u8; 24], aad: &[u8], pt: &[u8]) -> Vec<u8> {
    cipher(key).encrypt(&XNonce::from(*nonce), Payload { msg: pt, aad }).expect("encryption cannot fail for in-memory buffers")
}

pub fn aead_open(key: &[u8; 32], nonce: &[u8; 24], aad: &[u8], ct: &[u8]) -> Result<Vec<u8>> {
    cipher(key).decrypt(&XNonce::from(*nonce), Payload { msg: ct, aad }).map_err(|_| Error::Crypto("authentication failed".into()))
}

/// First 8 bytes of BLAKE3 over a group key. Identifies which key encrypted a payload.
pub fn key_check(key: &[u8; 32]) -> [u8; 8] {
    hash(key)[..8].try_into().unwrap()
}

/// Encrypts an event payload with a scope epoch key. Layout: key_check(8) | nonce(24) | ciphertext.
pub fn encrypt_payload(env: &dyn Env, key: &[u8; 32], aad: &[u8], pt: &[u8]) -> Vec<u8> {
    let nonce: [u8; 24] = random_array(env);
    let mut out = Vec::with_capacity(32 + pt.len() + 16);
    out.extend_from_slice(&key_check(key));
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&aead_seal(key, &nonce, aad, pt));
    out
}

pub fn payload_key_check(ct: &[u8]) -> Option<[u8; 8]> {
    ct.get(..8).map(|b| b.try_into().unwrap())
}

pub fn decrypt_payload(key: &[u8; 32], aad: &[u8], ct: &[u8]) -> Result<Vec<u8>> {
    if ct.len() < 32 + 16 {
        return Err(Error::Crypto("payload too short".into()));
    }
    let nonce: [u8; 24] = ct[8..32].try_into().unwrap();
    aead_open(key, &nonce, aad, &ct[32..])
}

fn x25519_public(device: &[u8; 32]) -> Result<x25519_dalek::PublicKey> {
    let vk = VerifyingKey::from_bytes(device).map_err(|_| Error::Crypto("bad device key".into()))?;
    Ok(x25519_dalek::PublicKey::from(vk.to_montgomery().to_bytes()))
}

fn seal_key(shared: &[u8; 32], eph: &[u8; 32], recipient: &[u8; 32]) -> [u8; 32] {
    let mut salt = [0u8; 64];
    salt[..32].copy_from_slice(eph);
    salt[32..].copy_from_slice(recipient);
    let hk = Hkdf::<Sha256>::new(Some(&salt), shared);
    let mut okm = [0u8; 32];
    hk.expand(b"osaurus-sync/seal/v1", &mut okm).expect("hkdf length");
    okm
}

/// Seals a secret to a device's Ed25519 key through X25519. Layout: eph_pub(32) | nonce(24) | ct.
pub fn seal_to_device(env: &dyn Env, recipient: &[u8; 32], pt: &[u8]) -> Result<Vec<u8>> {
    let their = x25519_public(recipient)?;
    let eph = x25519_dalek::StaticSecret::from(random_array::<32>(env));
    let eph_pub = x25519_dalek::PublicKey::from(&eph);
    let shared = eph.diffie_hellman(&their);
    let key = seal_key(shared.as_bytes(), eph_pub.as_bytes(), recipient);
    let nonce: [u8; 24] = random_array(env);
    let mut out = Vec::with_capacity(56 + pt.len() + 16);
    out.extend_from_slice(eph_pub.as_bytes());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&aead_seal(&key, &nonce, recipient, pt));
    Ok(out)
}

pub fn unseal(device: &SigningKey, sealed: &[u8]) -> Result<Vec<u8>> {
    if sealed.len() < 56 + 16 {
        return Err(Error::Crypto("sealed box too short".into()));
    }
    let eph: [u8; 32] = sealed[..32].try_into().unwrap();
    let nonce: [u8; 24] = sealed[32..56].try_into().unwrap();
    let mine = x25519_dalek::StaticSecret::from(device.to_scalar_bytes());
    let shared = mine.diffie_hellman(&x25519_dalek::PublicKey::from(eph));
    let recipient = device.verifying_key().to_bytes();
    let key = seal_key(shared.as_bytes(), &eph, &recipient);
    aead_open(&key, &nonce, &recipient, &sealed[56..])
}

pub const BLOB_CHUNK: usize = 64 * 1024;
const BLOB_MAGIC: &[u8; 4] = b"OSB1";

/// Result of encrypting a file into the blob store.
pub struct EncryptedBlob {
    pub hash: [u8; 32],
    pub size: u64,
    pub plain_size: u64,
}

fn chunk_nonce(prefix: &[u8; 16], index: u64) -> [u8; 24] {
    let mut n = [0u8; 24];
    n[..16].copy_from_slice(prefix);
    n[16..].copy_from_slice(&index.to_be_bytes());
    n
}

fn read_full(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

/// Encrypts a file in 64 KiB chunks so arbitrarily large files never sit fully in memory.
/// Each chunk's nonce is the random prefix plus the chunk index; the final chunk is marked in
/// the associated data so truncation is detected.
pub fn encrypt_blob_file(env: &dyn Env, key: &[u8; 32], src: &Path, dst: &Path) -> Result<EncryptedBlob> {
    let prefix: [u8; 16] = random_array(env);
    let c = cipher(key);
    let mut input = BufReader::new(File::open(src)?);
    let mut out = BufWriter::new(File::create(dst)?);
    let mut hasher = blake3::Hasher::new();
    let mut write = |bytes: &[u8], out: &mut BufWriter<File>| -> Result<()> {
        hasher.update(bytes);
        out.write_all(bytes)?;
        Ok(())
    };
    let mut header = Vec::with_capacity(20);
    header.extend_from_slice(BLOB_MAGIC);
    header.extend_from_slice(&prefix);
    write(&header, &mut out)?;

    let mut cur = vec![0u8; BLOB_CHUNK];
    let mut next = vec![0u8; BLOB_CHUNK];
    let mut cur_len = read_full(&mut input, &mut cur)?;
    let mut index = 0u64;
    let mut plain_size = 0u64;
    let mut size = header.len() as u64;
    loop {
        let next_len = if cur_len == BLOB_CHUNK { read_full(&mut input, &mut next)? } else { 0 };
        let last = next_len == 0;
        let ct = c
            .encrypt(&XNonce::from(chunk_nonce(&prefix, index)), Payload { msg: &cur[..cur_len], aad: &[last as u8] })
            .map_err(|_| Error::Crypto("blob encryption failed".into()))?;
        write(&ct, &mut out)?;
        plain_size += cur_len as u64;
        size += ct.len() as u64;
        if last {
            break;
        }
        std::mem::swap(&mut cur, &mut next);
        cur_len = next_len;
        index += 1;
    }
    out.flush()?;
    Ok(EncryptedBlob { hash: *hasher.finalize().as_bytes(), size, plain_size })
}

pub fn decrypt_blob_file(key: &[u8; 32], src: &Path, dst: &Path) -> Result<u64> {
    let c = cipher(key);
    let mut input = BufReader::new(File::open(src)?);
    let mut header = [0u8; 20];
    input.read_exact(&mut header)?;
    if &header[..4] != BLOB_MAGIC {
        return Err(Error::Crypto("not a blob".into()));
    }
    let prefix: [u8; 16] = header[4..].try_into().unwrap();
    let mut out = BufWriter::new(File::create(dst)?);
    let mut cur = vec![0u8; BLOB_CHUNK + 16];
    let mut next = vec![0u8; BLOB_CHUNK + 16];
    let mut cur_len = read_full(&mut input, &mut cur)?;
    let mut index = 0u64;
    let mut total = 0u64;
    loop {
        let next_len = if cur_len == BLOB_CHUNK + 16 { read_full(&mut input, &mut next)? } else { 0 };
        let last = next_len == 0;
        let pt = c
            .decrypt(&XNonce::from(chunk_nonce(&prefix, index)), Payload { msg: &cur[..cur_len], aad: &[last as u8] })
            .map_err(|_| Error::Crypto("blob authentication failed".into()))?;
        out.write_all(&pt)?;
        total += pt.len() as u64;
        if last {
            break;
        }
        std::mem::swap(&mut cur, &mut next);
        cur_len = next_len;
        index += 1;
    }
    out.flush()?;
    Ok(total)
}

pub fn hash_file(path: &Path) -> Result<[u8; 32]> {
    let mut hasher = blake3::Hasher::new();
    let mut f = BufReader::new(File::open(path)?);
    let mut buf = vec![0u8; BLOB_CHUNK];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(*hasher.finalize().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::TestEnv;

    #[test]
    fn payload_roundtrip_and_aad_binding() {
        let env = TestEnv::new(1, 0);
        let key = [7u8; 32];
        let ct = encrypt_payload(&env, &key, b"id-1", b"hello");
        assert_eq!(payload_key_check(&ct).unwrap(), key_check(&key));
        assert_eq!(decrypt_payload(&key, b"id-1", &ct).unwrap(), b"hello");
        assert!(decrypt_payload(&key, b"id-2", &ct).is_err());
    }

    #[test]
    fn seal_roundtrip() {
        let env = TestEnv::new(2, 0);
        let sk = SigningKey::from_bytes(&random_array(&env));
        let sealed = seal_to_device(&env, &sk.verifying_key().to_bytes(), b"scope key").unwrap();
        assert_eq!(unseal(&sk, &sealed).unwrap(), b"scope key");
        let other = SigningKey::from_bytes(&random_array(&env));
        assert!(unseal(&other, &sealed).is_err());
    }

    #[test]
    fn blob_roundtrip_sizes() {
        let env = TestEnv::new(3, 0);
        let dir = tempfile::tempdir().unwrap();
        for len in [0usize, 1, BLOB_CHUNK - 1, BLOB_CHUNK, BLOB_CHUNK + 1, 3 * BLOB_CHUNK + 17] {
            let src = dir.path().join("src");
            let enc = dir.path().join("enc");
            let dec = dir.path().join("dec");
            let data: Vec<u8> = (0..len).map(|i| (i * 31 % 251) as u8).collect();
            std::fs::write(&src, &data).unwrap();
            let key = [9u8; 32];
            let b = encrypt_blob_file(&env, &key, &src, &enc).unwrap();
            assert_eq!(b.plain_size, len as u64);
            assert_eq!(hash_file(&enc).unwrap(), b.hash);
            assert_eq!(decrypt_blob_file(&key, &enc, &dec).unwrap(), len as u64);
            assert_eq!(std::fs::read(&dec).unwrap(), data);
        }
    }

    #[test]
    fn blob_truncation_detected() {
        let env = TestEnv::new(4, 0);
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::write(&src, vec![1u8; 2 * BLOB_CHUNK + 5]).unwrap();
        let enc = dir.path().join("enc");
        encrypt_blob_file(&env, &[1; 32], &src, &enc).unwrap();
        let bytes = std::fs::read(&enc).unwrap();
        std::fs::write(&enc, &bytes[..20 + BLOB_CHUNK + 16]).unwrap();
        assert!(decrypt_blob_file(&[1; 32], &enc, &dir.path().join("dec")).is_err());
    }
}
