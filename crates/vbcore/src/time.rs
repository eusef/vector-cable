//! Local monotonic time as plain seconds, passed in by the caller.
//!
//! The logic modules never read a clock. A caller feeds them `LocalTime`
//! values taken from one monotonic source (`std::time::Instant` in the
//! runner, a simulated clock in tests).

use std::ops::{Add, Sub};

/// Seconds on the caller's monotonic clock. Only differences carry meaning.
#[derive(Clone, Copy, Debug, Default, PartialEq, PartialOrd)]
pub struct LocalTime(pub f64);

impl LocalTime {
    pub const ZERO: LocalTime = LocalTime(0.0);

    pub fn seconds(self) -> f64 {
        self.0
    }
}

impl Add<f64> for LocalTime {
    type Output = LocalTime;
    fn add(self, rhs: f64) -> LocalTime {
        LocalTime(self.0 + rhs)
    }
}

impl Sub<f64> for LocalTime {
    type Output = LocalTime;
    fn sub(self, rhs: f64) -> LocalTime {
        LocalTime(self.0 - rhs)
    }
}

impl Sub for LocalTime {
    type Output = f64;
    /// Seconds from `rhs` to `self`.
    fn sub(self, rhs: LocalTime) -> f64 {
        self.0 - rhs.0
    }
}

/// A monotonic clock backed by `std::time::Instant`, for the I/O layer.
#[derive(Clone, Copy, Debug)]
pub struct Clock {
    epoch: std::time::Instant,
}

impl Default for Clock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock {
    pub fn new() -> Self {
        Clock {
            epoch: std::time::Instant::now(),
        }
    }

    pub fn now(&self) -> LocalTime {
        LocalTime(self.epoch.elapsed().as_secs_f64())
    }
}
