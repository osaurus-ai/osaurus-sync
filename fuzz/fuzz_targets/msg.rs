//! Sync protocol messages, the first thing parsed from every frame a peer sends.
#![no_main]

use libfuzzer_sys::fuzz_target;
use osaurus_sync_core::sync::proto::Msg;

fuzz_target!(|data: &[u8]| {
    let Ok(msg) = Msg::decode(data) else { return };
    assert_eq!(Msg::decode(&msg.encode()).ok(), Some(msg.clone()), "re-encoding changed the message");
    let slices = msg.clone().into_slices(16);
    assert!(!slices.is_empty());
    if let Msg::Events { events, last, .. } = msg {
        let total: usize = slices.iter().map(|m| if let Msg::Events { events, .. } = m { events.len() } else { 0 }).sum();
        assert_eq!(total, events.len(), "slicing lost events");
        if let Some(Msg::Events { last: l, .. }) = slices.last() {
            assert_eq!(*l, last);
        }
    }
});
