//! Per-second rates from the kernel's cumulative counters.
//!
//! Every live sample goes through one `Rates`, so a rate is always over the
//! interval since the previous sample, whoever asked for it. A counter seen
//! for the first time, or one that went backwards (a reset, a re-created
//! interface), has no rate: None, never 0.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::sysfs::round;

#[derive(Default)]
struct State {
    prev: HashMap<String, f64>,
    next: HashMap<String, f64>,
    prev_t: Option<f64>,
    now: f64,
    dt: Option<f64>,
}

/// Shared by the collectors of one sample, which may run on several threads
/// (the GPUs are sampled in parallel).
#[derive(Default)]
pub struct Rates {
    state: Mutex<State>,
}

impl Rates {
    pub fn begin(&self, now: f64) {
        let mut s = self.state.lock().unwrap();
        s.now = now;
        s.dt = match s.prev_t {
            Some(p) if now > p => Some(now - p),
            _ => None,
        };
        s.next = HashMap::new();
    }

    pub fn end(&self) {
        let mut s = self.state.lock().unwrap();
        s.prev = std::mem::take(&mut s.next);
        s.prev_t = Some(s.now);
    }

    pub fn dt(&self) -> Option<f64> {
        self.state.lock().unwrap().dt
    }

    pub fn delta(&self, key: &str, value: Option<f64>) -> Option<f64> {
        let value = value?;
        let mut s = self.state.lock().unwrap();
        s.next.insert(key.to_string(), value);
        let prev = *s.prev.get(key)?;
        s.dt?;
        if value < prev {
            return None;
        }
        Some(value - prev)
    }

    pub fn rate(&self, key: &str, value: Option<f64>, digits: i32) -> Option<f64> {
        let d = self.delta(key, value)?;
        let dt = self.dt()?;
        if dt == 0.0 {
            return None;
        }
        Some(round(d / dt, digits))
    }

    /// rate() for an integer counter.
    pub fn rate_i(&self, key: &str, value: Option<i64>, digits: i32) -> Option<f64> {
        self.rate(key, value.map(|v| v as f64), digits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_are_per_second_and_never_from_a_reset() {
        let r = Rates::default();
        r.begin(100.0);
        assert_eq!(r.rate("x", Some(10.0), 3), None);
        r.end();
        r.begin(102.0);
        assert_eq!(r.rate("x", Some(30.0), 3), Some(10.0));
        assert_eq!(r.rate("new", Some(5.0), 3), None);
        r.end();
        r.begin(103.0);
        assert_eq!(r.rate("x", Some(1.0), 3), None);
        assert_eq!(r.rate("missing", None, 3), None);
        r.end();
        r.begin(104.0);
        assert_eq!(r.rate("x", Some(3.0), 3), Some(2.0));
    }
}
