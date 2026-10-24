//! Loro updates from other members, imported into a scope's document. Any member can write
//! these, so the bytes are untrusted.
#![no_main]

use libfuzzer_sys::fuzz_target;
use osaurus_sync_core::payload::LoroUpdatePayload;
use osaurus_sync_core::replica::fuzzing::apply_loro_update;

fuzz_target!(|data: &[u8]| {
    let Some((&flags, bytes)) = data.split_first() else { return };
    let _ = apply_loro_update(&LoroUpdatePayload { doc: 0, shallow: flags & 1 == 1, bytes: bytes.to_vec() });
});
