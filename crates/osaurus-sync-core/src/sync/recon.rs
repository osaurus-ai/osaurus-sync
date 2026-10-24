//! Range-based set reconciliation over `(hlc, id)` keys.
//!
//! Each message is a list of contiguous ranges covering `[lo, last upper)`. A range carries a
//! fingerprint, the full id list, or nothing (already in sync). The receiver compares each range
//! with its own keys and answers with smaller ranges until every range is in sync. Each side
//! learns which ids it lacks from the id lists it receives and pulls them separately, so the
//! responder keeps no state. A reply made only of `Skip` ranges is sent as an empty list.

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::ids::EventId;

pub type Key = (u64, EventId);
pub const MIN_KEY: Key = (0, EventId([0; 16]));
pub const MAX_KEY: Key = (u64::MAX, EventId([0xff; 16]));

/// Ranges with at most this many local keys are sent as id lists.
const SMALL: usize = 32;
/// Largest id list in one range.
const MAX_IDS: usize = 1024;
const BUCKETS: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Item {
    Skip,
    Fp {
        fp: [u8; 16],
        count: u32,
    },
    /// All ids in the range. `last` means the sender does not need the receiver's list back.
    Ids {
        ids: Vec<EventId>,
        last: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    /// Exclusive upper bound. The lower bound is the previous range's upper (or the message's lo).
    pub upper: Key,
    pub item: Item,
}

/// Sorted keys in `[lo, hi)`.
pub trait KeySource {
    fn keys(&self, lo: Key, hi: Key) -> Result<Vec<Key>>;
}

impl KeySource for Vec<Key> {
    fn keys(&self, lo: Key, hi: Key) -> Result<Vec<Key>> {
        let a = self.partition_point(|k| *k < lo);
        let b = self.partition_point(|k| *k < hi);
        Ok(self[a..b.max(a)].to_vec())
    }
}

pub fn fingerprint(keys: &[Key]) -> [u8; 16] {
    let mut acc: u128 = 0;
    for (hlc, id) in keys {
        let mut b = [0u8; 24];
        b[..8].copy_from_slice(&hlc.to_be_bytes());
        b[8..].copy_from_slice(&id.0);
        let h = blake3::hash(&b);
        acc = acc.wrapping_add(u128::from_le_bytes(h.as_bytes()[..16].try_into().unwrap()));
    }
    let mut out = acc.to_le_bytes();
    // Mix in the count so that ranges that differ only by multiplicity still differ.
    out[..4].iter_mut().zip((keys.len() as u32).to_le_bytes()).for_each(|(o, c)| *o ^= c);
    out
}

/// The first message for `[lo, hi)`.
pub fn initiate(src: &dyn KeySource, lo: Key, hi: Key) -> Result<Vec<Range>> {
    let mine = src.keys(lo, hi)?;
    let item = if mine.len() <= SMALL {
        Item::Ids { ids: mine.iter().map(|k| k.1).collect(), last: false }
    } else {
        Item::Fp { fp: fingerprint(&mine), count: mine.len() as u32 }
    };
    Ok(vec![Range { upper: hi, item }])
}

#[derive(Debug, Default)]
pub struct Reply {
    /// Empty when this range set is fully reconciled.
    pub ranges: Vec<Range>,
    /// Ids the peer has that this side lacks.
    pub need: Vec<EventId>,
}

pub fn respond(src: &dyn KeySource, lo: Key, incoming: &[Range]) -> Result<Reply> {
    let mut out: Vec<Range> = Vec::new();
    let mut need = Vec::new();
    let mut prev = lo;
    for r in incoming {
        let upper = r.upper;
        match &r.item {
            Item::Skip => push(&mut out, upper, Item::Skip),
            Item::Fp { fp, count } => {
                let mine = src.keys(prev, upper)?;
                if fingerprint(&mine) == *fp && mine.len() as u32 == *count {
                    push(&mut out, upper, Item::Skip);
                } else if *count == 0 && mine.len() <= MAX_IDS {
                    push(&mut out, upper, Item::Ids { ids: mine.iter().map(|k| k.1).collect(), last: true });
                } else if mine.len() <= SMALL {
                    push(&mut out, upper, Item::Ids { ids: mine.iter().map(|k| k.1).collect(), last: false });
                } else {
                    split(&mut out, &mine, upper);
                }
            }
            Item::Ids { ids, last } => {
                let mine = src.keys(prev, upper)?;
                let have: std::collections::HashSet<EventId> = mine.iter().map(|k| k.1).collect();
                need.extend(ids.iter().filter(|i| !have.contains(i)).copied());
                if *last {
                    push(&mut out, upper, Item::Skip);
                } else {
                    let theirs: std::collections::HashSet<&EventId> = ids.iter().collect();
                    if mine.iter().all(|k| theirs.contains(&k.1)) {
                        push(&mut out, upper, Item::Skip);
                    } else if mine.len() <= MAX_IDS {
                        push(&mut out, upper, Item::Ids { ids: mine.iter().map(|k| k.1).collect(), last: true });
                    } else {
                        split(&mut out, &mine, upper);
                    }
                }
            }
        }
        prev = upper;
    }
    if out.iter().all(|r| r.item == Item::Skip) {
        out.clear();
    }
    Ok(Reply { ranges: out, need })
}

fn push(out: &mut Vec<Range>, upper: Key, item: Item) {
    if item == Item::Skip
        && let Some(last) = out.last_mut()
        && last.item == Item::Skip
    {
        last.upper = upper;
        return;
    }
    out.push(Range { upper, item });
}

fn split(out: &mut Vec<Range>, mine: &[Key], upper: Key) {
    let chunk = mine.len().div_ceil(BUCKETS);
    let chunks: Vec<&[Key]> = mine.chunks(chunk).collect();
    for (i, c) in chunks.iter().enumerate() {
        let u = if i + 1 < chunks.len() { chunks[i + 1][0] } else { upper };
        push(out, u, Item::Fp { fp: fingerprint(c), count: c.len() as u32 });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn key(n: u64) -> Key {
        let mut id = [0u8; 16];
        id[..8].copy_from_slice(&n.wrapping_mul(0x9E37_79B9_7F4A_7C15).to_be_bytes());
        (n / 3, EventId(id))
    }

    /// Runs a full exchange; returns what each side learned it needs and the round count.
    fn run(a: &Vec<Key>, b: &Vec<Key>) -> (Vec<EventId>, Vec<EventId>, usize) {
        let mut msg = initiate(a, MIN_KEY, MAX_KEY).unwrap();
        let (mut need_a, mut need_b) = (vec![], vec![]);
        let mut rounds = 0;
        let mut a_turn = false;
        while !msg.is_empty() {
            rounds += 1;
            assert!(rounds < 64, "did not converge");
            let r = if a_turn { respond(a, MIN_KEY, &msg).unwrap() } else { respond(b, MIN_KEY, &msg).unwrap() };
            if a_turn {
                need_a.extend(r.need)
            } else {
                need_b.extend(r.need)
            }
            msg = r.ranges;
            a_turn = !a_turn;
        }
        need_a.sort();
        need_b.sort();
        (need_a, need_b, rounds)
    }

    fn diff(x: &[Key], y: &[Key]) -> Vec<EventId> {
        let mut v: Vec<EventId> = x.iter().filter(|k| !y.contains(k)).map(|k| k.1).collect();
        v.sort();
        v
    }

    #[test]
    fn identical_sets_finish_in_one_round() {
        let a: Vec<Key> = (0..10_000).map(key).collect();
        let mut s = a.clone();
        s.sort();
        let (na, nb, rounds) = run(&s, &s);
        assert!(na.is_empty() && nb.is_empty());
        assert_eq!(rounds, 1);
    }

    #[test]
    fn large_sets_with_few_differences() {
        let mut a: Vec<Key> = (0..20_000).map(key).collect();
        let mut b = a.clone();
        b.retain(|k| k.0 % 997 != 0);
        a.retain(|k| k.0 % 1009 != 5);
        a.sort();
        b.sort();
        let (na, nb, rounds) = run(&a, &b);
        assert_eq!(na, diff(&b, &a));
        assert_eq!(nb, diff(&a, &b));
        assert!(rounds < 12, "{rounds} rounds");
    }

    #[test]
    fn one_side_empty() {
        let mut a: Vec<Key> = (0..5_000).map(key).collect();
        a.sort();
        let (na, nb, _) = run(&a, &vec![]);
        assert!(na.is_empty());
        assert_eq!(nb.len(), 5_000);
        let (na, nb, _) = run(&vec![], &a);
        assert_eq!(na.len(), 5_000);
        assert!(nb.is_empty());
    }

    proptest! {
        #[test]
        fn reconciles_any_sets(xs in proptest::collection::btree_set(0u64..3000, 0..400), ys in proptest::collection::btree_set(0u64..3000, 0..400)) {
            let mut a: Vec<Key> = xs.iter().map(|n| key(*n)).collect();
            let mut b: Vec<Key> = ys.iter().map(|n| key(*n)).collect();
            a.sort();
            b.sort();
            let (na, nb, _) = run(&a, &b);
            prop_assert_eq!(na, diff(&b, &a));
            prop_assert_eq!(nb, diff(&a, &b));
        }
    }
}
