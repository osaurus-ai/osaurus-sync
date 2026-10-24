//! Device and user secrets, and the recovery phrase.

use ed25519_dalek::SigningKey;
use hkdf::Hkdf;
use sha2::Sha256;

use crate::env::{Env, random_array};
use crate::error::{Error, Result};
use crate::ids::{DeviceKey, UserKey};

/// Per-install device key. Its public half is the Iroh endpoint id and signs every event.
#[derive(Clone)]
pub struct DeviceSecret(SigningKey);

impl DeviceSecret {
    pub fn generate(env: &dyn Env) -> Self {
        Self(SigningKey::from_bytes(&random_array(env)))
    }

    pub fn from_bytes(b: &[u8]) -> Result<Self> {
        let arr: [u8; 32] = b.try_into().map_err(|_| Error::Crypto("device key must be 32 bytes".into()))?;
        Ok(Self(SigningKey::from_bytes(&arr)))
    }

    pub fn to_bytes(&self) -> [u8; 32] {
        self.0.to_bytes()
    }

    pub fn public(&self) -> DeviceKey {
        DeviceKey(self.0.verifying_key().to_bytes())
    }

    pub fn signing_key(&self) -> &SigningKey {
        &self.0
    }
}

/// The user's long-lived identity. Derived from 256 bits of entropy that the 24-word recovery
/// phrase encodes, so the phrase alone restores it.
#[derive(Clone)]
pub struct UserSecret {
    entropy: [u8; 32],
    key: SigningKey,
    /// Scope keys are also sealed to this key, so the phrase alone decrypts history.
    recovery: SigningKey,
}

impl UserSecret {
    pub fn generate(env: &dyn Env) -> Self {
        Self::from_entropy(random_array(env))
    }

    pub fn from_entropy(entropy: [u8; 32]) -> Self {
        let hk = Hkdf::<Sha256>::new(Some(b"osaurus-sync"), &entropy);
        let mut seed = [0u8; 32];
        hk.expand(b"osaurus-sync/user-key/v1", &mut seed).expect("hkdf length");
        let mut rseed = [0u8; 32];
        hk.expand(b"osaurus-sync/recovery-key/v1", &mut rseed).expect("hkdf length");
        Self { entropy, key: SigningKey::from_bytes(&seed), recovery: SigningKey::from_bytes(&rseed) }
    }

    pub fn from_phrase(phrase: &str) -> Result<Self> {
        let m = bip39::Mnemonic::parse_normalized(phrase.trim()).map_err(|e| Error::InvalidInput(format!("recovery phrase: {e}")))?;
        let entropy: [u8; 32] = m.to_entropy().try_into().map_err(|_| Error::InvalidInput("recovery phrase must be 24 words".into()))?;
        Ok(Self::from_entropy(entropy))
    }

    pub fn phrase(&self) -> Vec<String> {
        bip39::Mnemonic::from_entropy(&self.entropy).expect("32 bytes of entropy is a valid mnemonic").words().map(str::to_owned).collect()
    }

    pub fn entropy(&self) -> [u8; 32] {
        self.entropy
    }

    pub fn public(&self) -> UserKey {
        UserKey(self.key.verifying_key().to_bytes())
    }

    pub fn signing_key(&self) -> &SigningKey {
        &self.key
    }

    /// The recovery key, addressed in `KeyGrant`s like a device. It never signs anything.
    pub fn recovery_public(&self) -> DeviceKey {
        DeviceKey(self.recovery.verifying_key().to_bytes())
    }

    pub fn recovery_key(&self) -> &SigningKey {
        &self.recovery
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::TestEnv;

    #[test]
    fn phrase_restores_user_key() {
        let env = TestEnv::new(11, 0);
        let u = UserSecret::generate(&env);
        let words = u.phrase();
        assert_eq!(words.len(), 24);
        let restored = UserSecret::from_phrase(&words.join(" ")).unwrap();
        assert_eq!(restored.public(), u.public());
        assert_eq!(restored.recovery_public(), u.recovery_public());
        assert_ne!(u.recovery_public().0, u.public().0);
    }

    #[test]
    fn bad_phrase_rejected() {
        assert!(UserSecret::from_phrase("not a valid phrase").is_err());
        let twelve = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
        assert!(UserSecret::from_phrase(twelve).is_err());
    }
}
