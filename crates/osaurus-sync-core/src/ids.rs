use std::fmt;

use serde::{Deserialize, Serialize};

/// Serde helpers for fixed-size byte arrays larger than serde's built-in limit.
pub mod fixed {
    use serde::{Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer, const N: usize>(v: &[u8; N], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(v)
    }

    pub fn deserialize<'de, D: Deserializer<'de>, const N: usize>(d: D) -> Result<[u8; N], D::Error> {
        let v: Vec<u8> = serde_bytes_vec(d)?;
        v.try_into().map_err(|_| D::Error::custom("wrong byte length"))
    }

    fn serde_bytes_vec<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = Vec<u8>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("bytes")
            }
            fn visit_bytes<E: Error>(self, v: &[u8]) -> Result<Vec<u8>, E> {
                Ok(v.to_vec())
            }
            fn visit_borrowed_bytes<E: Error>(self, v: &'de [u8]) -> Result<Vec<u8>, E> {
                Ok(v.to_vec())
            }
            fn visit_byte_buf<E: Error>(self, v: Vec<u8>) -> Result<Vec<u8>, E> {
                Ok(v)
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<u8>, A::Error> {
                let mut out = Vec::new();
                while let Some(b) = seq.next_element::<u8>()? {
                    out.push(b);
                }
                Ok(out)
            }
        }
        d.deserialize_bytes(V)
    }

    /// Same as the parent module, for `Vec<[u8; N]>`-free use with `Option<[u8; N]>`.
    pub mod option {
        use serde::{Deserialize, Serialize};
        use serde::{Deserializer, Serializer};

        #[derive(Serialize, Deserialize)]
        struct W<const N: usize>(#[serde(with = "super")] [u8; N]);

        pub fn serialize<S: Serializer, const N: usize>(v: &Option<[u8; N]>, s: S) -> Result<S::Ok, S::Error> {
            v.map(W).serialize(s)
        }

        pub fn deserialize<'de, D: Deserializer<'de>, const N: usize>(d: D) -> Result<Option<[u8; N]>, D::Error> {
            Ok(Option::<W<N>>::deserialize(d)?.map(|w| w.0))
        }
    }
}

macro_rules! byte_id {
    ($(#[$m:meta])* $name:ident, $len:expr) => {
        $(#[$m])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        pub struct $name(#[serde(with = "crate::ids::fixed")] pub [u8; $len]);

        impl $name {
            pub const LEN: usize = $len;

            pub fn from_slice(b: &[u8]) -> Option<Self> {
                b.try_into().ok().map(Self)
            }

            pub fn to_hex(&self) -> String {
                hex::encode(self.0)
            }

            pub fn from_hex(s: &str) -> Option<Self> {
                hex::decode(s).ok().and_then(|v| Self::from_slice(&v))
            }

            pub fn as_bytes(&self) -> &[u8] {
                &self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), &self.to_hex()[..12])
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.to_hex())
            }
        }
    };
}

byte_id!(
    /// UUIDv7 event id: globally unique, roughly time-sortable, generated offline.
    EventId, 16
);
byte_id!(
    /// A conversation or workspace. Equal to the id of the scope's genesis event.
    ScopeId, 16
);
byte_id!(
    /// BLAKE3 hash of an event's signed bytes.
    EventHash, 32
);
byte_id!(
    /// Ed25519 public key of a device. Also the device's Iroh endpoint id.
    DeviceKey, 32
);
byte_id!(
    /// Ed25519 public key of a user.
    UserKey, 32
);
byte_id!(
    /// Compressed secp256k1 public key of an agent.
    AgentId, 33
);

impl EventId {
    /// Builds a UUIDv7 from a millisecond timestamp and 10 random bytes.
    pub fn new_v7(unix_ms: u64, random: [u8; 10]) -> Self {
        Self(*uuid::Builder::from_unix_timestamp_millis(unix_ms, &random).into_uuid().as_bytes())
    }
}

impl From<EventId> for ScopeId {
    fn from(id: EventId) -> Self {
        ScopeId(id.0)
    }
}

/// A scope member: a user or an agent.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Debug)]
pub enum Principal {
    User(UserKey),
    Agent(AgentId),
}

impl Principal {
    pub fn to_hex(&self) -> String {
        match self {
            Principal::User(k) => format!("u:{}", k.to_hex()),
            Principal::Agent(k) => format!("a:{}", k.to_hex()),
        }
    }

    pub fn from_hex(s: &str) -> Option<Self> {
        if let Some(rest) = s.strip_prefix("u:") {
            UserKey::from_hex(rest).map(Principal::User)
        } else if let Some(rest) = s.strip_prefix("a:") {
            AgentId::from_hex(rest).map(Principal::Agent)
        } else {
            None
        }
    }
}

/// Workspace roles, ordered from least to most privileged.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Debug)]
pub enum Role {
    Viewer = 0,
    Member = 1,
    Admin = 2,
    Owner = 3,
}

impl Role {
    pub fn from_u8(v: u8) -> Option<Role> {
        Some(match v {
            0 => Role::Viewer,
            1 => Role::Member,
            2 => Role::Admin,
            3 => Role::Owner,
            _ => return None,
        })
    }
}
