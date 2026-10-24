//! Invite links and pairing codes, which arrive as text from QR codes, links and the clipboard.
#![no_main]

use libfuzzer_sys::fuzz_target;
use osaurus_sync_core::replica::InviteToken;
use osaurus_sync_core::sync::engine::PairingInvite;

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else { return };
    if let Ok(t) = InviteToken::decode(s) {
        let again = InviteToken::decode(&t.encode()).expect("a decoded invite decodes again");
        assert_eq!(again.encode(), t.encode());
    }
    if let Ok(p) = PairingInvite::decode(s) {
        let again = PairingInvite::decode(&p.encode()).expect("a decoded pairing code decodes again");
        assert_eq!(again.encode(), p.encode());
    }
    for prefix in ["osaurus-invite:1:", "osaurus-pair:1:"] {
        let _ = InviteToken::decode(&format!("{prefix}{s}"));
        let _ = PairingInvite::decode(&format!("{prefix}{s}"));
    }
});
