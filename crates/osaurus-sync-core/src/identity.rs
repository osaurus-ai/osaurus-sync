//! Device certificates and agent bindings.

use serde::{Deserialize, Serialize};

use crate::crypto;
use crate::error::{Error, Result};
use crate::ids::{AgentId, DeviceKey, UserKey};
use crate::keys::UserSecret;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Platform {
    MacOs,
    Windows,
    Ios,
    Server,
    Other,
}

impl Platform {
    pub fn name(&self) -> &'static str {
        match self {
            Platform::MacOs => "macOS",
            Platform::Windows => "Windows",
            Platform::Ios => "iOS",
            Platform::Server => "Server",
            Platform::Other => "Other",
        }
    }
}

/// A user's statement that a device key acts for them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCert {
    pub user: UserKey,
    pub device: DeviceKey,
    pub name: String,
    pub platform: Platform,
    pub issued_ms: u64,
    #[serde(with = "crate::ids::fixed")]
    pub sig: [u8; 64],
}

#[derive(Serialize)]
struct DeviceCertBody<'a> {
    domain: &'static str,
    user: &'a UserKey,
    device: &'a DeviceKey,
    name: &'a str,
    platform: Platform,
    issued_ms: u64,
}

impl DeviceCert {
    fn body(&self) -> Vec<u8> {
        postcard::to_allocvec(&DeviceCertBody {
            domain: "osaurus-sync/device-cert/v1",
            user: &self.user,
            device: &self.device,
            name: &self.name,
            platform: self.platform,
            issued_ms: self.issued_ms,
        })
        .expect("serializable")
    }

    pub fn issue(user: &UserSecret, device: DeviceKey, name: &str, platform: Platform, issued_ms: u64) -> Self {
        let mut cert = DeviceCert { user: user.public(), device, name: name.to_owned(), platform, issued_ms, sig: [0; 64] };
        cert.sig = crypto::sign(user.signing_key(), &cert.body());
        cert
    }

    pub fn verify(&self) -> bool {
        let body = self.body();
        crypto::verify_memo(&[b"device-cert", &self.user.0, &body, &self.sig], || crypto::verify(&self.user.0, &body, &self.sig))
    }
}

/// An agent's statement, signed with its secp256k1 key, naming the device that hosts it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentBinding {
    pub agent: AgentId,
    pub host_device: DeviceKey,
    pub expires_ms: u64,
    #[serde(with = "crate::ids::fixed")]
    pub sig: [u8; 64],
}

#[derive(Serialize)]
struct AgentBindingBody<'a> {
    domain: &'static str,
    agent: &'a AgentId,
    host_device: &'a DeviceKey,
    expires_ms: u64,
}

impl AgentBinding {
    fn body(&self) -> Vec<u8> {
        postcard::to_allocvec(&AgentBindingBody {
            domain: "osaurus-sync/agent-binding/v1",
            agent: &self.agent,
            host_device: &self.host_device,
            expires_ms: self.expires_ms,
        })
        .expect("serializable")
    }

    pub fn agent_id_from_secret(secret: &[u8]) -> Result<AgentId> {
        let sk = k256::ecdsa::SigningKey::from_slice(secret).map_err(|_| Error::InvalidInput("agent key must be a 32-byte secp256k1 scalar".into()))?;
        let point = sk.verifying_key().to_sec1_point(true);
        AgentId::from_slice(point.as_bytes()).ok_or_else(|| Error::Crypto("unexpected agent key encoding".into()))
    }

    pub fn sign(agent_secret: &[u8], host_device: DeviceKey, expires_ms: u64) -> Result<Self> {
        use k256::ecdsa::signature::Signer;
        let sk = k256::ecdsa::SigningKey::from_slice(agent_secret).map_err(|_| Error::InvalidInput("agent key must be a 32-byte secp256k1 scalar".into()))?;
        let agent = Self::agent_id_from_secret(agent_secret)?;
        let mut b = AgentBinding { agent, host_device, expires_ms, sig: [0; 64] };
        let sig: k256::ecdsa::Signature = sk.sign(&b.body());
        b.sig = sig.to_bytes().as_slice().try_into().expect("64-byte signature");
        Ok(b)
    }

    pub fn verify(&self) -> bool {
        let body = self.body();
        crypto::verify_memo(&[b"agent-binding", &self.agent.0, &body, &self.sig], || verify_agent_sig(&self.agent, &body, &self.sig))
    }
}

fn verify_agent_sig(agent: &AgentId, body: &[u8], sig: &[u8; 64]) -> bool {
    use k256::ecdsa::signature::Verifier;
    let Ok(vk) = k256::ecdsa::VerifyingKey::from_sec1_bytes(&agent.0) else { return false };
    let Ok(sig) = k256::ecdsa::Signature::from_slice(sig) else { return false };
    vk.verify(body, &sig).is_ok()
}

const AGENT_CARD_PREFIX: &str = "osaurus-agent-card:1:";

/// An agent's public statement, signed with its secp256k1 key, naming its host device and the
/// user that device acts for. Served on the agent's public URL so a caller that knows only the
/// agent's address can find and verify its host. It grants nothing by itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCard {
    pub agent: AgentId,
    pub host_device: DeviceKey,
    pub user: UserKey,
    pub expires_ms: u64,
    #[serde(with = "crate::ids::fixed")]
    pub sig: [u8; 64],
}

#[derive(Serialize)]
struct AgentCardBody<'a> {
    domain: &'static str,
    agent: &'a AgentId,
    host_device: &'a DeviceKey,
    user: &'a UserKey,
    expires_ms: u64,
}

impl AgentCard {
    fn body(&self) -> Vec<u8> {
        postcard::to_allocvec(&AgentCardBody {
            domain: "osaurus-sync/agent-card/v1",
            agent: &self.agent,
            host_device: &self.host_device,
            user: &self.user,
            expires_ms: self.expires_ms,
        })
        .expect("serializable")
    }

    pub fn sign(agent_secret: &[u8], host_device: DeviceKey, user: UserKey, expires_ms: u64) -> Result<Self> {
        use k256::ecdsa::signature::Signer;
        let sk = k256::ecdsa::SigningKey::from_slice(agent_secret).map_err(|_| Error::InvalidInput("agent key must be a 32-byte secp256k1 scalar".into()))?;
        let agent = AgentBinding::agent_id_from_secret(agent_secret)?;
        let mut c = AgentCard { agent, host_device, user, expires_ms, sig: [0; 64] };
        let sig: k256::ecdsa::Signature = sk.sign(&c.body());
        c.sig = sig.to_bytes().as_slice().try_into().expect("64-byte signature");
        Ok(c)
    }

    pub fn verify(&self) -> bool {
        verify_agent_sig(&self.agent, &self.body(), &self.sig)
    }

    /// `osaurus-agent-card:1:<hex>`.
    pub fn encode(&self) -> String {
        format!("{AGENT_CARD_PREFIX}{}", hex::encode(crate::payload::encode(self)))
    }

    /// Parses a card without checking its signature or expiry.
    pub fn decode(s: &str) -> Result<Self> {
        let hexed = s.trim().strip_prefix(AGENT_CARD_PREFIX).ok_or_else(|| Error::InvalidInput("not an agent card".into()))?;
        let bytes = hex::decode(hexed).map_err(|_| Error::InvalidInput("bad agent card encoding".into()))?;
        crate::payload::decode(&bytes).map_err(|_| Error::InvalidInput("bad agent card".into()))
    }

    /// Decodes and checks a card: the signature, that it hasn't expired at `now_ms`, and, if
    /// given, that it's for the agent with this `0x` address (any case).
    pub fn open(s: &str, address: Option<&str>, now_ms: u64) -> Result<Self> {
        let card = Self::decode(s)?;
        if !card.verify() {
            return Err(Error::Crypto("agent card signature does not verify".into()));
        }
        if card.expires_ms <= now_ms {
            return Err(Error::Crypto("agent card has expired".into()));
        }
        if let Some(want) = address {
            let have = card.agent.address_hex().ok_or_else(|| Error::Crypto("agent key is not a valid point".into()))?;
            if !have.eq_ignore_ascii_case(want.trim()) {
                return Err(Error::Crypto("agent card is for a different address".into()));
            }
        }
        Ok(card)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::TestEnv;
    use crate::keys::DeviceSecret;

    #[test]
    fn device_cert_verifies_and_detects_tamper() {
        let env = TestEnv::new(5, 0);
        let user = UserSecret::generate(&env);
        let dev = DeviceSecret::generate(&env);
        let mut cert = DeviceCert::issue(&user, dev.public(), "MacBook", Platform::MacOs, 42);
        assert!(cert.verify());
        cert.name = "Evil".into();
        assert!(!cert.verify());
    }

    #[test]
    fn agent_binding_verifies() {
        let env = TestEnv::new(6, 0);
        let dev = DeviceSecret::generate(&env);
        let secret = [3u8; 32];
        let mut b = AgentBinding::sign(&secret, dev.public(), 1_000).unwrap();
        assert!(b.verify());
        assert_eq!(b.agent, AgentBinding::agent_id_from_secret(&secret).unwrap());
        b.expires_ms = 2_000;
        assert!(!b.verify());
    }

    #[test]
    fn agent_card_roundtrips_and_checks() {
        let env = TestEnv::new(7, 0);
        let dev = DeviceSecret::generate(&env);
        let user = UserSecret::generate(&env).public();
        let secret = [5u8; 32];
        let card = AgentCard::sign(&secret, dev.public(), user, 10_000).unwrap();
        let s = card.encode();
        assert!(s.starts_with("osaurus-agent-card:1:"));
        assert_eq!(AgentCard::decode(&s).unwrap(), card);

        let addr = card.agent.address_hex().unwrap();
        assert_eq!(AgentCard::open(&s, Some(&addr.to_uppercase().replace("0X", "0x")), 9_999).unwrap(), card);
        assert_eq!(AgentCard::open(&s, None, 0).unwrap(), card);
        assert!(matches!(AgentCard::open(&s, Some(&addr), 10_000), Err(Error::Crypto(_))), "expired");
        let other = AgentCard::sign(&[6u8; 32], dev.public(), user, 10_000).unwrap();
        let other_addr = other.agent.address_hex().unwrap();
        assert!(matches!(AgentCard::open(&s, Some(&other_addr), 0), Err(Error::Crypto(_))), "wrong address");
        assert!(matches!(AgentCard::open("osaurus-agent-card:1:zz", None, 0), Err(Error::InvalidInput(_))));
        assert!(matches!(AgentCard::open("hello", None, 0), Err(Error::InvalidInput(_))));
    }

    #[test]
    fn agent_card_detects_tamper() {
        let env = TestEnv::new(8, 0);
        let dev = DeviceSecret::generate(&env);
        let user = UserSecret::generate(&env).public();
        let card = AgentCard::sign(&[5u8; 32], dev.public(), user, 10_000).unwrap();
        assert!(card.verify());

        let mut c = card.clone();
        c.host_device = DeviceSecret::generate(&env).public();
        assert!(!c.verify(), "host");
        let mut c = card.clone();
        c.user = UserSecret::generate(&env).public();
        assert!(!c.verify(), "user");
        let mut c = card.clone();
        c.expires_ms += 1;
        assert!(!c.verify(), "expiry");
        let mut c = card.clone();
        c.agent = AgentBinding::agent_id_from_secret(&[6u8; 32]).unwrap();
        assert!(!c.verify(), "agent");
        let mut c = card.clone();
        c.sig[0] ^= 1;
        assert!(!c.verify(), "signature");
        assert!(matches!(AgentCard::open(&c.encode(), None, 0), Err(Error::Crypto(_))));

        // A binding signature over the same fields doesn't pass as a card.
        let b = AgentBinding::sign(&[5u8; 32], dev.public(), 10_000).unwrap();
        let mut c = card.clone();
        c.sig = b.sig;
        assert!(!c.verify(), "domain separation");
    }
}
