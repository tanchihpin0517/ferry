//! What the Destination could not do, by kind, reported once at the end.

use std::collections::BTreeMap;

use crate::progress;

/// What the Destination could not do, by kind, with a few of the paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Warning {
    Special,
}

impl Warning {
    pub(super) fn describe(self) -> &'static str {
        match self {
            Warning::Special => "were not created: creating sockets and devices needs privilege",
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
