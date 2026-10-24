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
        crypto::verify(&self.user.0, &self.body(), &self.sig)
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
        use k256::ecdsa::signature::Verifier;
        let Ok(vk) = k256::ecdsa::VerifyingKey::from_sec1_bytes(&self.agent.0) else { return false };
        let Ok(sig) = k256::ecdsa::Signature::from_slice(&self.sig) else { return false };
        vk.verify(&self.body(), &sig).is_ok()
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
}
