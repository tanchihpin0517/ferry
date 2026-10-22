//! What the Destination could not do, by kind, reported once at the end.

use std::collections::BTreeMap;

use crate::progress;

/// What the Destination could not do, by kind, with a few of the paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Warning {
    Special,
    /// An owner this process may not set, or the filesystem does not keep.
    Owner,
    /// Setuid or setgid taken away, as the owner they run as could not be
    /// set.
    Setid,
    Mode,
    Times,
}

impl Warning {
    pub(super) fn describe(self) -> &'static str {
        match self {
            Warning::Special => "were not created: creating sockets and devices needs privilege",
            Warning::Owner => "kept another owner: it could not be set here",
            Warning::Setid => {
                "lost setuid or setgid: their owner could not be set here, and they would run as this one"
            }
            Warning::Mode => "kept other permissions: they could not be set here",
            Warning::Times => "kept another modification time: it could not be set here",
        }
    }
}

#[derive(Default)]
pub(super) struct Warnings {
    pub(super) kinds: BTreeMap<Warning, (u64, Vec<Vec<u8>>)>,
}

impl Warnings {
    pub(super) const EXAMPLES: usize = 3;

    pub(super) fn add(&mut self, warning: Warning, path: &[u8]) {
        let (count, examples) = self.kinds.entry(warning).or_default();
        *count += 1;
        if examples.len() < Self::EXAMPLES {
            examples.push(path.to_vec());
        }
    }

    pub(super) fn messages(&self) -> Vec<String> {
        self.kinds
            .iter()
            .map(|(warning, (count, examples))| {
                let examples: Vec<String> = examples
                    .iter()
                    .map(|path| String::from_utf8_lossy(path).into_owned())
                    .collect();
                let more = if *count as usize > examples.len() {
                    ", …"
                } else {
                    ""
                };
                format!(
                    "{} {} ({}{more})",
                    progress::counted(*count, "entry", "entries"),
                    warning.describe(),
                    examples.join(", ")
                )
            })
            .collect()
    }
}
