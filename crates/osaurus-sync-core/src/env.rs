use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Source of time and randomness. The simulation substitutes a seeded, virtual implementation
/// so that a failing seed replays exactly.
pub trait Env: Send + Sync {
    fn now_ms(&self) -> u64;
    fn fill_random(&self, buf: &mut [u8]);
}

pub fn random_array<const N: usize>(env: &dyn Env) -> [u8; N] {
    let mut b = [0u8; N];
    env.fill_random(&mut b);
    b
}

pub fn random_u64(env: &dyn Env) -> u64 {
    u64::from_le_bytes(random_array(env))
}

/// Wall clock and OS randomness.
pub struct SystemEnv;

impl Env for SystemEnv {
    fn now_ms(&self) -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
    }

    fn fill_random(&self, buf: &mut [u8]) {
        getrandom::fill(buf).expect("OS randomness unavailable");
    }
}

/// Deterministic environment for tests: a manually advanced clock and a xorshift stream.
pub struct TestEnv {
    state: Mutex<(u64, u64)>,
}

impl TestEnv {
    pub fn new(seed: u64, start_ms: u64) -> Self {
        Self { state: Mutex::new((start_ms, seed | 1)) }
    }

    pub fn advance(&self, ms: u64) {
        self.state.lock().unwrap().0 += ms;
    }

    pub fn set_now(&self, ms: u64) {
        self.state.lock().unwrap().0 = ms;
    }
}

impl Env for TestEnv {
    fn now_ms(&self) -> u64 {
        self.state.lock().unwrap().0
    }

    fn fill_random(&self, buf: &mut [u8]) {
        let mut s = self.state.lock().unwrap();
        for b in buf.iter_mut() {
            let mut x = s.1;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            s.1 = x;
            *b = (x >> 24) as u8;
        }
    }
}
