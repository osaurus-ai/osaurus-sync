//! Ephemeral streaming frames on their own ALPN.
#![no_main]

use libfuzzer_sys::fuzz_target;
use osaurus_sync_core::sync::proto::StreamFrame;

fuzz_target!(|data: &[u8]| {
    let Ok(frame) = StreamFrame::decode(data) else { return };
    assert_eq!(StreamFrame::decode(&frame.encode()).ok(), Some(frame), "re-encoding changed the frame");
});
