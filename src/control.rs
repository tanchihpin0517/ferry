//! The client's control of a transfer: it follows where the threads of each
//! side spend their time, as each server sends its [`Metrics`] every second
//! on its control connection and the client takes its own, and hands what
//! it sees to a [`Policy`].
//!
//! The client is where both sides and the network between them are seen at
//! once, so it is where a transfer will be steered: the servers report, and
//! later do as they are told. Telling them is left for the policies to come;
//! for now, [`Watch`] only watches.

use std::collections::VecDeque;

use crate::profile::Metrics;

/// How many of each side's latest intervals are kept.
pub const WINDOW: usize = 10;

/// Whose threads metrics are of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Client,
    Source,
    Destination,
}

impl Side {
    pub const ALL: [Side; 3] = [Side::Client, Side::Source, Side::Destination];
}

/// What the client sees of a transfer: each side's latest intervals, the
/// oldest first.
#[derive(Debug, Default)]
pub struct View {
    sides: [VecDeque<Metrics>; 3],
}

impl View {
    /// Adds `side`'s next interval, letting go of the oldest beyond
    /// [`WINDOW`].
    pub fn add(&mut self, side: Side, metrics: Metrics) {
        let intervals = &mut self.sides[side as usize];
        intervals.push_back(metrics);
        if intervals.len() > WINDOW {
            intervals.pop_front();
        }
    }

    /// `side`'s latest intervals, the oldest first.
    pub fn intervals(&self, side: Side) -> impl Iterator<Item = &Metrics> {
        self.sides[side as usize].iter()
    }

    /// `side`'s latest interval, if one came.
    pub fn latest(&self, side: Side) -> Option<&Metrics> {
        self.sides[side as usize].back()
    }
}

/// What steers a transfer, given what the client sees, each time a side's
/// interval comes.
pub trait Policy: Send {
    fn observe(&mut self, view: &View);
}

/// The policy that steers nothing yet.
pub struct Watch;

impl Policy for Watch {
    fn observe(&mut self, _view: &View) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics(sequence: u64) -> Metrics {
        Metrics {
            sequence,
            nanos: 1_000_000_000,
            roles: Vec::new(),
        }
    }

    #[test]
    fn a_view_keeps_each_side_s_latest_intervals() {
        let mut view = View::default();
        for sequence in 1..=WINDOW as u64 + 3 {
            view.add(Side::Source, metrics(sequence));
        }
        view.add(Side::Destination, metrics(1));
        let kept: Vec<u64> = view
            .intervals(Side::Source)
            .map(|metrics| metrics.sequence)
            .collect();
        assert_eq!(kept, (4..=WINDOW as u64 + 3).collect::<Vec<_>>());
        assert_eq!(view.latest(Side::Destination).unwrap().sequence, 1);
        assert!(view.latest(Side::Client).is_none());
    }
}
