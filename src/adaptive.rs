//! The compression level, adapted to whichever is the bottleneck: the
//! network or the CPU.
//!
//! The signal is the queue of data packets waiting to be written to the
//! connection. Kept mostly full, the network is slower than compression, and
//! a higher level spends idle CPU on sending fewer bytes. Kept mostly empty,
//! compression is what the network waits for, and a lower level speeds it.
//! The level moves one step at a time, and only after several frames in a row
//! agree, so that it does not swing back and forth.

use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, Ordering};

/// The level adaptation starts at.
pub const START: i32 = 3;
/// The levels it stays between. zstd goes higher, but at a cost in memory and
/// speed not worth it for a transfer.
pub const LOWEST: i32 = 1;
pub const HIGHEST: i32 = 19;

/// How many frames in a row must agree before the level moves.
const AGREEMENT: u32 = 4;

/// A queue fuller than this says the network is the bottleneck.
const FULL: f64 = 0.75;
/// A queue emptier than this says the CPU is.
const EMPTY: f64 = 0.25;

pub struct Level {
    level: AtomicI32,
    /// Fixed by the user: never adapted.
    fixed: bool,
    /// Which way the last frames pointed, and how many in a row.
    streak: Mutex<(i32, u32)>,
}

impl Level {
    /// Adapted from [`START`], or fixed at `fixed`.
    pub fn new(fixed: Option<i32>) -> Self {
        Self {
            level: AtomicI32::new(fixed.unwrap_or(START)),
            fixed: fixed.is_some(),
            streak: Mutex::new((0, 0)),
        }
    }

    pub fn get(&self) -> i32 {
        self.level.load(Ordering::Relaxed)
    }

    /// Takes a reading as a frame is sent: how full the queue to the network
    /// is, from 0 to 1.
    pub fn observe(&self, backlog: f64) {
        if self.fixed {
            return;
        }
        let direction = if backlog >= FULL {
            1
        } else if backlog <= EMPTY {
            -1
        } else {
            0
        };
        let mut streak = self.streak.lock().expect("not poisoned");
        if direction == 0 || direction != streak.0 {
            *streak = (direction, u32::from(direction != 0));
            return;
        }
        streak.1 += 1;
        if streak.1 >= AGREEMENT {
            streak.1 = 0;
            let level = (self.get() + direction).clamp(LOWEST, HIGHEST);
            self.level.store(level, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_queue_raises_the_level_after_agreement() {
        let level = Level::new(None);
        for _ in 0..AGREEMENT - 1 {
            level.observe(1.0);
        }
        assert_eq!(level.get(), START);
        level.observe(1.0);
        assert_eq!(level.get(), START + 1);
    }

    #[test]
    fn an_empty_queue_lowers_it_and_bounds_hold() {
        let level = Level::new(None);
        for _ in 0..100 {
            level.observe(0.0);
        }
        assert_eq!(level.get(), LOWEST);
        for _ in 0..1_000 {
            level.observe(1.0);
        }
        assert_eq!(level.get(), HIGHEST);
    }

    #[test]
    fn disagreement_resets_the_streak() {
        let level = Level::new(None);
        for _ in 0..10 {
            level.observe(1.0);
            level.observe(1.0);
            level.observe(0.5);
        }
        assert_eq!(level.get(), START);
    }

    #[test]
    fn a_fixed_level_never_moves() {
        let level = Level::new(Some(7));
        for _ in 0..100 {
            level.observe(1.0);
        }
        assert_eq!(level.get(), 7);
    }
}
