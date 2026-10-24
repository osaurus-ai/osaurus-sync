//! Event envelopes as received from a peer: decoding, re-encoding and signature checks.
#![no_main]

use libfuzzer_sys::fuzz_target;
use osaurus_sync_core::envelope::{Decoded, Envelope};

fuzz_target!(|data: &[u8]| {
    let Ok(decoded) = Envelope::decode(data) else { return };
    let _ = decoded.hash();
    if let Decoded::Known(env) = decoded {
        let _ = env.verify_signature();
        let _ = env.all_parents().count();
        let again = env.encode();
        match Envelope::decode(&again) {
            Ok(Decoded::Known(e)) => assert_eq!(e, env, "re-encoding changed the envelope"),
            other => panic!("a decoded envelope didn't decode again: {other:?}"),
        }
        let purged = env.purged();
        assert_eq!(purged.hash(), env.hash(), "purging changed the event hash");
    }
});
