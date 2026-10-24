//! Agent identity records: an agent's binding, signed by its secp256k1 key and naming its host
//! endpoint, published in the host's discovery record.

use iroh::address_lookup::UserData;
use osaurus_sync_core::identity::AgentBinding;

const PREFIX: &str = "osa-agent1:";
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

pub fn encode(binding: &AgentBinding) -> Option<UserData> {
    let bytes = postcard::to_allocvec(binding).ok()?;
    format!("{PREFIX}{}", b64(&bytes)).parse().ok()
}

/// Returns the binding if the record is well formed and its signature verifies.
pub fn decode(data: &UserData) -> Option<AgentBinding> {
    let s: &str = data.as_ref();
    let bytes = unb64(s.strip_prefix(PREFIX)?)?;
    let b: AgentBinding = postcard::from_bytes(&bytes).ok()?;
    b.verify().then_some(b)
}

fn b64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |acc, (i, b)| acc | (*b as u32) << (16 - 8 * i));
        for i in 0..chunk.len() + 1 {
            out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

fn unb64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let vals: Option<Vec<u32>> = s.bytes().map(|c| ALPHABET.iter().position(|a| *a == c).map(|p| p as u32)).collect();
    for chunk in vals?.chunks(4) {
        if chunk.len() == 1 {
            return None;
        }
        let n = chunk.iter().enumerate().fold(0u32, |acc, (i, v)| acc | v << (18 - 6 * i));
        for i in 0..chunk.len() - 1 {
            out.push((n >> (16 - 8 * i)) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use osaurus_sync_core::ids::DeviceKey;

    #[test]
    fn base64_roundtrip() {
        for n in 0..40 {
            let v: Vec<u8> = (0..n).map(|i| (i * 37 + 11) as u8).collect();
            assert_eq!(unb64(&b64(&v)).unwrap(), v);
        }
    }

    #[test]
    fn record_fits_and_verifies() {
        let b = AgentBinding::sign(&[7u8; 32], DeviceKey([3; 32]), 1_900_000_000_000).unwrap();
        let ud = encode(&b).expect("fits in a discovery record");
        assert_eq!(decode(&ud), Some(b));
    }
}
