//! Each entry the Source has sent in the manifest and the Destination has
//! not compared yet, by ID: whether it is a file, and its path, to answer
//! the Destination's needs. Kept a batch of the manifest at a time, each
//! with its directory's path once and its names in one buffer, and let go a
//! batch at a time, in order, once the Destination says it has compared all
//! of it, and all before it: what is kept is what is on its way, not the
//! whole tree.
//!
//! It is held within a bound: past it, the batches held go to disk together,
//! a file of them, let go of whole once the Destination has compared all
//! of it, so that what a directory the Destination holds back until its
//! last keeps is on disk, however large it is; see [`crate::sorted`]. So
//! it is not the owner's, which does no I/O: the scan keeps each batch as
//! the owner lets it go, before it is written, and the thread that reads
//! the answers finds what each need is, and lets batches go.

use std::collections::{BTreeMap, VecDeque};
use std::io;

use crate::manifest::{self, FileId, Kind};
use crate::sorted::{Keyed, Spill};

/// See the module.
pub(super) struct Known {
    spill: Spill,
    /// The batches on disk, the oldest: each a file of whole batches, by
    /// their first IDs, and the IDs it holds.
    written: VecDeque<Written>,
    /// The batches after those, in memory, and what they cost.
    parts: VecDeque<KnownPart>,
    cost: usize,
    /// How many entries were ever recorded.
    recorded: u64,
    /// The ranges of the IDs kept that the Destination said it compared,
    /// merged, by their first, to their last.
    compared: BTreeMap<FileId, FileId>,
    /// The batch last read from disk, for the entries next to one another
    /// the Destination needs.
    cached: Option<KnownPart>,
}

/// Batches on disk: by their first IDs, from `first`, to before `end`.
struct Written {
    batches: Keyed,
    first: FileId,
    end: FileId,
}

/// A batch of the manifest, as it is kept.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct KnownPart {
    /// The ID of its first entry; the others follow it.
    first: FileId,
    directory: Vec<u8>,
    names: Vec<u8>,
    /// For each entry, where its name ends in `names`, and whether it is a
    /// file.
    entries: Vec<(u32, bool)>,
}

impl KnownPart {
    /// The `entries` of the directory at `path`, in ID order, as they are
    /// kept; none of none. The root is the one entry of the directory `` with
    /// the name ``.
    pub(super) fn of(path: &[u8], entries: &[manifest::Entry]) -> Option<KnownPart> {
        let first = entries.first()?;
        let mut part = KnownPart {
            first: first.id,
            directory: path.to_vec(),
            names: Vec::new(),
            entries: Vec::with_capacity(entries.len()),
        };
        for entry in entries {
            part.names.extend_from_slice(&entry.name);
            let end = u32::try_from(part.names.len()).expect("a part's names under 4 GiB");
            part.entries.push((end, entry.kind == Kind::File));
        }
        Some(part)
    }

    /// The ID after its last entry's.
    fn end(&self) -> FileId {
        self.first + self.entries.len() as FileId
    }

    /// What it costs in memory, about.
    fn cost(&self) -> usize {
        self.directory.len() + self.names.len() + self.entries.len() * 8 + 96
    }

    /// Whether the entry `id` is a file, and its path, if it is in it.
    fn entry(&self, id: FileId) -> Option<(bool, Vec<u8>)> {
        let offset = usize::try_from(id.checked_sub(self.first)?).ok()?;
        let &(end, is_file) = self.entries.get(offset)?;
        let start = match offset {
            0 => 0,
            _ => self.entries[offset - 1].0 as usize,
        };
        let name = &self.names[start..end as usize];
        Some((is_file, manifest::join(&self.directory, name)))
    }
}

impl Known {
    pub(super) fn new(spill: Spill) -> Self {
        Self {
            spill,
            written: VecDeque::new(),
            parts: VecDeque::new(),
            cost: 0,
            recorded: 0,
            compared: BTreeMap::new(),
            cached: None,
        }
    }

    /// How many entries were ever recorded: the manifest's.
    pub(super) fn len(&self) -> u64 {
        self.recorded
    }

    /// The ID of the first entry kept, if any.
    fn front(&self) -> Option<FileId> {
        match self.written.front() {
            Some(written) => Some(written.first),
            None => self.parts.front().map(|part| part.first),
        }
    }

    /// The ID after the last entry kept, if any.
    fn back(&self) -> Option<FileId> {
        match self.parts.back() {
            Some(part) => Some(part.end()),
            None => self.written.back().map(|written| written.end),
        }
    }

    /// Records a part, whose IDs follow the last recorded's; past the bound,
    /// the parts held go to disk.
    pub(super) fn record(&mut self, part: KnownPart) -> io::Result<()> {
        debug_assert!(
            self.back().is_none_or(|end| end == part.first),
            "IDs follow manifest order"
        );
        self.recorded += part.entries.len() as u64;
        self.cost += part.cost();
        self.parts.push_back(part);
        if self.cost >= self.spill.limits.held {
            let mut batches = Keyed::new(&self.spill);
            let first = self.parts.front().expect("one").first;
            let end = self.parts.back().expect("one").end();
            for part in self.parts.drain(..) {
                let value = postcard::to_stdvec(&part).map_err(io::Error::other)?;
                batches.push(&part.first.to_be_bytes(), &value)?;
            }
            self.cost = 0;
            self.written.push_back(Written {
                batches,
                first,
                end,
            });
        }
        Ok(())
    }

    /// Whether the entry `id` is a file, and its path, if it is kept.
    pub(super) fn entry(&mut self, id: FileId) -> io::Result<Option<(bool, Vec<u8>)>> {
        if let Some(front) = self.parts.front()
            && id >= front.first
        {
            let index = self.parts.partition_point(|part| part.end() <= id);
            return Ok(self.parts.get(index).and_then(|part| part.entry(id)));
        }
        if let Some(cached) = &self.cached
            && let Some(entry) = cached.entry(id)
        {
            return Ok(Some(entry));
        }
        let index = self.written.partition_point(|written| written.end <= id);
        let Some(written) = self.written.get_mut(index) else {
            return Ok(None);
        };
        if id < written.first {
            return Ok(None);
        }
        let Some((_, value)) = written.batches.floor(&id.to_be_bytes())? else {
            return Ok(None);
        };
        let part: KnownPart = postcard::from_bytes(&value).map_err(io::Error::other)?;
        let entry = part.entry(id);
        self.cached = Some(part);
        Ok(entry)
    }

    /// The Destination has compared every entry from `first` to `last`, and
    /// asked for those it needs: the batches at the front all of whose
    /// entries it compared go, and a file of them once all its batches would.
    /// Only what is kept counts: of IDs before or past it, there is nothing
    /// to let go of. A batch is never empty, so its last entry is the one
    /// before its end.
    pub(super) fn compared(&mut self, first: FileId, last: FileId) {
        let (Some(front), Some(back)) = (self.front(), self.back()) else {
            return;
        };
        let (mut first, mut last) = (first.max(front), last.min(back - 1));
        if first > last {
            return;
        }
        // Merged with the ranges it touches.
        if let Some((&start, &end)) = self.compared.range(..=first).next_back()
            && end.saturating_add(1) >= first
        {
            self.compared.remove(&start);
            (first, last) = (start, last.max(end));
        }
        while let Some((&start, &end)) = self.compared.range(first..).next()
            && start <= last.saturating_add(1)
        {
            self.compared.remove(&start);
            last = last.max(end);
        }
        self.compared.insert(first, last);
        let covered = |compared: &BTreeMap<FileId, FileId>, from: FileId, end: FileId| {
            compared
                .range(..=from)
                .next_back()
                .is_some_and(|(_, &last)| last >= end - 1)
        };
        while let Some(written) = self.written.front() {
            if !covered(&self.compared, written.first, written.end) {
                break;
            }
            self.written.pop_front();
            self.cached = None;
        }
        if self.written.is_empty() {
            while let Some(part) = self.parts.front() {
                if !covered(&self.compared, part.first, part.end()) {
                    break;
                }
                self.cost -= part.cost();
                self.parts.pop_front();
            }
        }
        match self.front() {
            Some(kept) => self.compared.retain(|_, end| *end >= kept),
            None => self.compared.clear(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Timestamp;
    use crate::sorted::Limits;

    fn entry(id: FileId, name: &[u8], kind: Kind) -> manifest::Entry {
        manifest::Entry {
            id,
            name: name.to_vec(),
            kind,
            size: 0,
            mtime: Timestamp {
                seconds: 0,
                nanoseconds: 0,
            },
            mode: 0o644,
            uid: 0,
            gid: 0,
        }
    }

    /// The tree of the root, `a`, `f`, and in `a`, `x`, `y`, and in `a/z`,
    /// `deep`, kept a batch each: of the root, of the root's entries, of
    /// `a`'s, of `a/z`'s.
    fn known(spill: Spill) -> Known {
        let mut known = Known::new(spill);
        for (path, entries) in [
            (&b""[..], vec![entry(0, b"", Kind::Directory)]),
            (
                b"",
                vec![entry(1, b"a", Kind::Directory), entry(2, b"f", Kind::File)],
            ),
            (b"a", vec![entry(3, b"x", Kind::File)]),
            (b"a", vec![entry(4, b"y", Kind::File)]),
            (b"a/z", vec![entry(5, b"deep", Kind::File)]),
        ] {
            known
                .record(KnownPart::of(path, &entries).unwrap())
                .unwrap();
        }
        known
    }

    fn path(known: &mut Known, id: FileId) -> Option<Vec<u8>> {
        known.entry(id).unwrap().map(|(_, path)| path)
    }

    /// Kept in memory, or on disk past a bound reached by every batch: each
    /// entry's path and whether it is a file, until the batches all of whose
    /// entries were compared go, in order.
    #[test]
    fn entries_are_kept_until_compared_in_memory_or_on_disk() {
        let temp = tempfile::tempdir().unwrap();
        for held in [crate::sorted::HELD, 1] {
            let mut known = known(Spill {
                limits: Limits { held, fan_in: 4 },
                temp: temp.path().to_owned(),
            });
            assert_eq!(known.len(), 6);
            let paths: Vec<Vec<u8>> = (0..6).map(|id| path(&mut known, id).unwrap()).collect();
            assert_eq!(paths, [&b""[..], b"a", b"f", b"a/x", b"a/y", b"a/z/deep"]);
            let files: Vec<bool> = (0..6)
                .map(|id| known.entry(id).unwrap().unwrap().0)
                .collect();
            assert_eq!(files, [false, false, true, true, true, true]);
            assert_eq!(known.entry(6).unwrap(), None);
            // Compared out of order: nothing goes until the front has.
            known.compared(2, 3);
            assert_eq!(path(&mut known, 0).unwrap(), b"");
            known.compared(0, 1);
            for id in 0..4 {
                assert_eq!(path(&mut known, id), None, "{id}");
            }
            assert_eq!(path(&mut known, 4).unwrap(), b"a/y");
            known.compared(5, 5);
            assert_eq!(path(&mut known, 5).unwrap(), b"a/z/deep");
            known.compared(4, 4);
            assert_eq!(path(&mut known, 5), None);
            assert!(known.compared.is_empty());
            assert_eq!(known.len(), 6);
        }
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
    }

    /// Past what is kept, nothing changes.
    #[test]
    fn ranges_past_what_is_kept_change_nothing() {
        let mut known = Known::new(Spill::default());
        known.compared(0, FileId::MAX);
        known
            .record(KnownPart::of(b"", &[entry(0, b"", Kind::Directory)]).unwrap())
            .unwrap();
        known
            .record(KnownPart::of(b"", &[entry(1, b"f", Kind::File)]).unwrap())
            .unwrap();
        known.compared(FileId::MAX, FileId::MAX);
        assert_eq!(known.parts.len(), 2);
        // To the last ID there could be, as a Destination gone wrong may
        // say: all go, nothing overflows.
        known.compared(0, FileId::MAX);
        assert!(known.parts.is_empty());
    }

    /// A batch compared in part stays, until all of it is.
    #[test]
    fn a_batch_compared_in_part_stays() {
        let mut known = Known::new(Spill::default());
        let batch = [
            entry(0, b"p", Kind::File),
            entry(1, b"q", Kind::File),
            entry(2, b"r", Kind::File),
        ];
        known.record(KnownPart::of(b"b", &batch).unwrap()).unwrap();
        known.compared(1, 1);
        known.compared(0, 0);
        assert_eq!(path(&mut known, 2).unwrap(), b"b/r");
        known.compared(2, 2);
        assert!(known.parts.is_empty());
    }
}
