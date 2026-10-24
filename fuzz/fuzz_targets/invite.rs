//! Invite links, pairing codes and agent cards, which arrive as text from QR codes, links, the
//! clipboard and agents' public URLs.
#![no_main]

use libfuzzer_sys::fuzz_target;
use osaurus_sync_core::identity::AgentCard;
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
    if let Ok(c) = AgentCard::decode(s) {
        let again = AgentCard::decode(&c.encode()).expect("a decoded agent card decodes again");
        assert_eq!(again, c);
        let _ = AgentCard::open(s, c.agent.address_hex().as_deref(), 0);
    }
    for prefix in ["osaurus-invite:1:", "osaurus-pair:1:", "osaurus-agent-card:1:"] {
        let _ = InviteToken::decode(&format!("{prefix}{s}"));
        let _ = PairingInvite::decode(&format!("{prefix}{s}"));
        let _ = AgentCard::decode(&format!("{prefix}{s}"));
    }
});
