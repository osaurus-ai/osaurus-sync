//! Range reconciliation against ranges chosen by a hostile peer: no panics, and replies
//! stay bounded by what was asked.
#![no_main]

use libfuzzer_sys::fuzz_target;
use osaurus_sync_core::ids::EventId;
use osaurus_sync_core::payload;
use osaurus_sync_core::sync::recon::{self, Key, MIN_KEY, Range};

fuzz_target!(|data: &[u8]| {
    let Some((&n, rest)) = data.split_first() else { return };
    let Ok(incoming) = payload::decode::<Vec<Range>>(rest) else { return };
    let mut mine: Vec<Key> = (0..u64::from(n) * 8)
        .map(|i| {
            let mut id = [0u8; 16];
            id[..8].copy_from_slice(&i.wrapping_mul(0x9E37_79B9_7F4A_7C15).to_be_bytes());
            (i / 3, EventId(id))
        })
        .collect();
    mine.sort();
    let Ok(reply) = recon::respond(&mine, MIN_KEY, &incoming) else { return };
    let asked: usize = incoming.iter().map(|r| if let recon::Item::Ids { ids, .. } = &r.item { ids.len() } else { 0 }).sum();
    assert!(reply.need.len() <= asked, "needs more ids than the peer listed");
    assert!(reply.ranges.len() <= incoming.len() * 16, "reply grew past the split fan-out");
    let _ = recon::initiate(&mine, MIN_KEY, recon::MAX_KEY);
});
