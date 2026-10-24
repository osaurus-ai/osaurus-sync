//! Decrypted event payloads: control and content events of every kind, and the certificates
//! and bindings inside them.
#![no_main]

use libfuzzer_sys::fuzz_target;
use osaurus_sync_core::identity::{AgentBinding, DeviceCert};
use osaurus_sync_core::payload::{self, Content, Control};

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }
    let kind = u16::from_le_bytes([data[0], data[1]]);
    let bytes = &data[2..];
    if let Ok(Some(c)) = Control::decode(kind, bytes) {
        let _ = format!("{c:?}");
    }
    if let Ok(Some(c)) = Content::decode(kind, bytes) {
        let encoded = c.encode();
        let again = Content::decode(c.kind(), &encoded).expect("re-encoded content decodes");
        assert!(again.is_some());
    }
    if let Ok(cert) = payload::decode::<DeviceCert>(bytes) {
        let _ = cert.verify();
    }
    if let Ok(binding) = payload::decode::<AgentBinding>(bytes) {
        let _ = binding.verify();
    }
});
