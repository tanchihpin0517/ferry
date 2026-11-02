//! What a transfer leaves out: rsync's include and exclude rules, as
//! `--exclude`, `--include`, and the files `--exclude-from` and
//! `--include-from` name give them, matched as rsync matches them.
//!
//! Each entry is matched by its path from the transfer's root, as a dry
//! run lists it, against the rules in the order they were given: the first
//! that matches says whether the entry is left out, and none, that it is
//! not. A directory left out is not gone into. How a pattern matches is
//! rsync's, from its `exclude.c` and `wildmatch.c`, and checked against
//! rsync itself, in the tests and in `tests/rsync.rs`:
//!
//! - a pattern without a `/` matches the last part of a path, at any depth;
//! - one that starts with `/` matches from the transfer's root;
//! - one with a `/` elsewhere, or a `**`, the end of the path, from the
//!   start of a part, at any depth;
//! - one that ends with `/` only a directory;
//! - `*` matches anything but `/`, `**` anything, `?` one byte but `/`,
//!   `[...]` one of a set, and `\` takes the next byte as it is; `dir/***`
//!   matches `dir` and everything under it.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// A rule, as given: whether it includes or excludes, and its pattern.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub include: bool,
    pub pattern: Vec<u8>,
}

impl Rule {
    /// A rule as an option gives it, `include` for `--include`: a pattern
    /// that starts with `- ` or `+ ` is an exclude or an include whatever
    /// the option, as rsync takes it. Refused: `!`, which clears the rules
    /// given so far in rsync, and is not built; and a pattern no path can
    /// match, as one with an empty part, or `.` or `..` for one, which
    /// rsync takes and matches nothing with, so that what it was to keep
    /// from a `sync` would be deleted.
    pub fn given(text: &[u8], include: bool) -> Result<Rule> {
        if text == b"!" {
            bail!("the rule `!`, which clears the rules given before it, is not built yet");
        }
        let (include, pattern) = match text {
            [b'-', b' ', pattern @ ..] => (false, pattern),
            [b'+', b' ', pattern @ ..] => (true, pattern),
            pattern => (include, pattern),
        };
        if !can_match(pattern) {
            bail!(
                "the rule `{}` can match nothing: no path has an empty part, nor `.` or `..`",
                crate::printable::path(pattern)
            );
        }
        Ok(Rule {
            include,
            pattern: pattern.to_vec(),
        })
    }
}

/// The rules a file holds, as rsync reads the file `--exclude-from` or
/// `--include-from` names, `include` for the latter: a line each, without
/// the carriage return before its newline, if any; blank lines, and lines
/// that start with `#` or `;`, left out. Each line is a rule as
/// [`Rule::given`] takes it, its spaces and all.
pub fn from_file(contents: &[u8], include: bool) -> Result<Vec<Rule>> {
    let mut rules = Vec::new();
    for (index, line) in contents.split(|&byte| byte == b'\n').enumerate() {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() || line.starts_with(b"#") || line.starts_with(b";") {
            continue;
        }
        let rule = Rule::given(line, include)
            .map_err(|error| error.context(format!("line {}", index + 1)))?;
        rules.push(rule);
    }
    Ok(rules)
}

/// Whether some path can match `pattern`: none has an empty part, nor `.`
/// or `..`, which a pattern has, past its leading `/` and the trailing one
/// that makes it a directory's, as `./build` or `a//b` do.
fn can_match(pattern: &[u8]) -> bool {
    let inner = pattern.strip_prefix(b"/").unwrap_or(pattern);
    let inner = match pattern.len() > 1 {
        true => inner.strip_suffix(b"/").unwrap_or(inner),
        false => inner,
    };
    !inner
        .split(|&byte| byte == b'/')
        .any(|part| matches!(part, b"" | b"." | b".."))
}

/// The rules, ready to match.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    rules: Vec<Compiled>,
    /// What each path a server gives follows, from the transfer's root:
    /// the name of the root its own paths are from, where that is an entry
    /// the transfer names, as SOURCE without a trailing slash is; empty
    /// where the root is the transfer's own.
    under: Vec<u8>,
}

impl Filter {
    pub fn new(rules: &[Rule]) -> Self {
        Self {
            rules: rules.iter().map(Compiled::new).collect(),
            under: Vec::new(),
        }
    }

    /// The same, for a server whose root is an entry named `name` in the
    /// transfer's: each path it gives then follows `name`, and its root is
    /// `name`, matched as any other entry, as rsync matches the SOURCE it is
    /// given without a trailing slash.
    pub fn under(self, name: Option<&[u8]>) -> Self {
        Self {
            under: name.map(<[u8]>::to_vec).unwrap_or_default(),
            ..self
        }
    }

    /// Whether there are no rules, and nothing is left out.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Whether the entry at `path`, from the server's root, without a
    /// leading or trailing `/`, is left out: the first rule that matches
    /// it, from the transfer's root, says so. The transfer's root itself
    /// never is.
    pub fn excludes(&self, path: &[u8], is_directory: bool) -> bool {
        if self.rules.is_empty() {
            return false;
        }
        let full;
        let path = if self.under.is_empty() {
            path
        } else {
            let mut joined = self.under.clone();
            if !path.is_empty() {
                joined.push(b'/');
                joined.extend_from_slice(path);
            }
            full = joined;
            &full[..]
        };
        self.rules
            .iter()
            .find(|rule| rule.matches(path, is_directory))
            .is_some_and(|rule| !rule.include)
    }
}

/// A rule as rsync keeps it to match, from its `add_rule`.
#[derive(Debug, Clone)]
struct Compiled {
    include: bool,
    /// The pattern, without the trailing `/` that makes it a directory's,
    /// its leading `/` kept.
    pattern: Vec<u8>,
    /// It matches only a directory.
    directory: bool,
    /// How many `/` it has, its leading one counted.
    slashes: usize,
    /// It has a wildcard: `*`, `?`, or `[`.
    wild: bool,
    /// It has `**`, which matches across `/`.
    wild2: bool,
    /// It starts with `**`.
    wild2_prefix: bool,
    /// It ends with `***`, which matches the directory before it too.
    wild3_suffix: bool,
}

impl Compiled {
    fn new(rule: &Rule) -> Self {
        let mut pattern = rule.pattern.clone();
        let directory = pattern.len() > 1 && pattern.ends_with(b"/");
        if directory {
            pattern.pop();
        }
        let slashes = pattern.iter().filter(|&&byte| byte == b'/').count();
        let wild = pattern
            .iter()
            .any(|byte| matches!(byte, b'*' | b'?' | b'['));
        let double = pattern.windows(2).position(|pair| pair == b"**");
        let wild2 = wild && double.is_some();
        Self {
            include: rule.include,
            directory,
            slashes,
            wild,
            wild2,
            wild2_prefix: wild2 && double == Some(0),
            wild3_suffix: wild2 && pattern.ends_with(b"***"),
            pattern,
        }
    }

    /// Whether it matches the entry at `path`, as rsync's `rule_matches`
    /// says.
    fn matches(&self, path: &[u8], is_directory: bool) -> bool {
        if path.is_empty() {
            return false;
        }
        let mut name = path;
        let mut slash_before = false;
        if self.slashes == 0 && !self.wild2 {
            // Neither a `/` nor a `**`: the last part alone.
            if let Some(slash) = path.iter().rposition(|&byte| byte == b'/') {
                name = &path[slash + 1..];
            }
        } else if self.wild2_prefix {
            // So that `**/` matches at the start.
            slash_before = true;
        }
        let mut slash_after = false;
        if is_directory {
            // So that a trailing `/***` matches the directory.
            slash_after = self.wild3_suffix;
        } else if self.directory {
            return false;
        }
        let (anchored, pattern) = match self.pattern.strip_prefix(b"/") {
            Some(pattern) => (true, pattern),
            None => (false, &self.pattern[..]),
        };
        let at = if !anchored && self.slashes > 0 && !self.wild2 {
            // A `/` within, and no `**`: the last parts, as many as it has.
            At::Last(self.slashes + 1)
        } else if !anchored && !self.wild2_prefix && self.wild2 {
            // A `**` within or at the end: from after any `/`.
            At::AnySlash
        } else {
            At::Start
        };
        if self.wild {
            let mut text = Vec::with_capacity(name.len() + 2);
            if slash_before {
                text.push(b'/');
            }
            text.extend_from_slice(name);
            if slash_after {
                text.push(b'/');
            }
            return wildmatch_at(pattern, &text, at);
        }
        if anchored {
            return name == pattern;
        }
        // The end of the path, from the start of a part.
        name.ends_with(pattern)
            && (name.len() == pattern.len() || name[name.len() - pattern.len() - 1] == b'/')
    }
}

/// Where in a path a wildcard pattern may start to match.
#[derive(Debug, Clone, Copy)]
enum At {
    Start,
    /// At the start, or just after any `/`.
    AnySlash,
    /// At the start of the last this many parts, which the path must have.
    Last(usize),
}

/// What one try at matching came to, as rsync's `dowild` says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wild {
    NoMatch,
    Match,
    /// No later start can match either.
    AbortAll,
    /// No later start can match, but one after a `**` before may.
    AbortToStarStar,
}

/// Whether `pattern` matches `text` starting `at` where it may, as rsync's
/// `wildmatch_array` does.
fn wildmatch_at(pattern: &[u8], text: &[u8], at: At) -> bool {
    let start = match at {
        At::Last(count) => match last_parts(text, count) {
            Some(start) => start,
            None => return false,
        },
        At::Start | At::AnySlash => 0,
    };
    let mut matched = dowild(pattern, &text[start..]);
    if matched != Wild::Match && matches!(at, At::AnySlash) && matched != Wild::AbortAll {
        let mut at = start;
        loop {
            let Some(&byte) = text.get(at) else {
                return false;
            };
            at += 1;
            if byte == b'/' {
                matched = dowild(pattern, &text[at..]);
                if matched != Wild::NoMatch && matched != Wild::AbortToStarStar {
                    break;
                }
            }
        }
    }
    matched == Wild::Match
}

/// Where the last `count` parts of `text` start, if it has as many.
fn last_parts(text: &[u8], mut count: usize) -> Option<usize> {
    for (index, &byte) in text.iter().enumerate().rev() {
        if byte == b'/' {
            count -= 1;
            if count == 0 {
                return Some(index + 1);
            }
        }
    }
    (count == 1).then_some(0)
}

/// Matches `pattern` against all of `text`, as rsync's `dowild` does: a
/// byte past either's end reads as 0, as its C strings end.
fn dowild(pattern: &[u8], text: &[u8]) -> Wild {
    let p_at = |index: usize| pattern.get(index).copied().unwrap_or(0);
    let (mut p, mut t) = (0, 0);
    while p_at(p) != 0 {
        let mut p_ch = p_at(p);
        let t_ch = text.get(t).copied().unwrap_or(0);
        if t_ch == 0 && p_ch != b'*' {
            return Wild::AbortAll;
        }
        match p_ch {
            b'?' => {
                if t_ch == b'/' {
                    return Wild::NoMatch;
                }
            }
            b'*' => {
                p += 1;
                let special = p_at(p) == b'*';
                while p_at(p) == b'*' {
                    p += 1;
                }
                if p_at(p) == 0 {
                    // A trailing `**` matches everything; a trailing `*`, what
                    // has no `/` left.
                    if !special && text[t..].contains(&b'/') {
                        return Wild::NoMatch;
                    }
                    return Wild::Match;
                }
                loop {
                    let Some(&t_ch) = text.get(t) else {
                        return Wild::AbortAll;
                    };
                    let matched = dowild(&pattern[p..], &text[t..]);
                    if matched != Wild::NoMatch {
                        if !special || matched != Wild::AbortToStarStar {
                            return matched;
                        }
                    } else if !special && t_ch == b'/' {
                        return Wild::AbortToStarStar;
                    }
                    t += 1;
                }
            }
            b'[' => {
                p += 1;
                p_ch = p_at(p);
                let negated = p_ch == b'!' || p_ch == b'^';
                if negated {
                    p += 1;
                    p_ch = p_at(p);
                }
                let mut previous = 0;
                let mut matched = false;
                loop {
                    if p_ch == 0 {
                        return Wild::AbortAll;
                    }
                    if p_ch == b'\\' {
                        p += 1;
                        p_ch = p_at(p);
                        if p_ch == 0 {
                            return Wild::AbortAll;
                        }
                        matched |= t_ch == p_ch;
                    } else if p_ch == b'-'
                        && previous != 0
                        && p_at(p + 1) != 0
                        && p_at(p + 1) != b']'
                    {
                        p += 1;
                        p_ch = p_at(p);
                        if p_ch == b'\\' {
                            p += 1;
                            p_ch = p_at(p);
                            if p_ch == 0 {
                                return Wild::AbortAll;
                            }
                        }
                        matched |= previous <= t_ch && t_ch <= p_ch;
                        p_ch = 0;
                    } else if p_ch == b'[' && p_at(p + 1) == b':' {
                        let name = p + 2;
                        let mut end = name;
                        while p_at(end) != 0 && p_at(end) != b']' {
                            end += 1;
                        }
                        if p_at(end) == 0 {
                            return Wild::AbortAll;
                        }
                        if end == name || p_at(end - 1) != b':' {
                            // No `:]`: a `[` as any other byte.
                            p_ch = b'[';
                            matched |= t_ch == p_ch;
                        } else {
                            matched |= match &pattern[name..end - 1] {
                                b"alnum" => t_ch.is_ascii_alphanumeric(),
                                b"alpha" => t_ch.is_ascii_alphabetic(),
                                b"blank" => t_ch == b' ' || t_ch == b'\t',
                                b"cntrl" => t_ch.is_ascii_control(),
                                b"digit" => t_ch.is_ascii_digit(),
                                b"graph" => t_ch.is_ascii_graphic(),
                                b"lower" => t_ch.is_ascii_lowercase(),
                                b"print" => t_ch.is_ascii_graphic() || t_ch == b' ',
                                b"punct" => t_ch.is_ascii_punctuation(),
                                b"space" => matches!(t_ch, b' ' | b'\t'..=b'\r'),
                                b"upper" => t_ch.is_ascii_uppercase(),
                                b"xdigit" => t_ch.is_ascii_hexdigit(),
                                _ => return Wild::AbortAll,
                            };
                            p = end;
                            p_ch = 0;
                        }
                    } else {
                        matched |= t_ch == p_ch;
                    }
                    previous = p_ch;
                    p += 1;
                    p_ch = p_at(p);
                    if p_ch == b']' {
                        break;
                    }
                }
                if matched == negated || t_ch == b'/' {
                    return Wild::NoMatch;
                }
            }
            _ => {
                if p_ch == b'\\' {
                    // The next byte, as it is.
                    p += 1;
                    p_ch = p_at(p);
                }
                if t_ch != p_ch {
                    return Wild::NoMatch;
                }
            }
        }
        p += 1;
        t += 1;
    }
    if t < text.len() {
        Wild::NoMatch
    } else {
        Wild::Match
    }
}

#[cfg(test)]
mod rsync_cases;

#[cfg(test)]
mod tests {
    use super::rsync_cases::{RSYNC, TREE_DIRS, TREE_FILES, TREE_LINKS};
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    /// The tree rsync's cases were made on: each path, and whether it is a
    /// directory; a symlink is not.
    fn tree() -> BTreeMap<String, bool> {
        let mut tree = BTreeMap::new();
        for directory in TREE_DIRS {
            let mut at = String::new();
            for part in directory.split('/') {
                if !at.is_empty() {
                    at.push('/');
                }
                at.push_str(part);
                tree.insert(at.clone(), true);
            }
        }
        for path in TREE_FILES
            .iter()
            .chain(TREE_LINKS.iter().map(|(path, _)| path))
        {
            tree.insert((*path).to_owned(), false);
        }
        tree
    }

    /// What a walk of the tree copies with `filter`, as rsync lists it: an
    /// entry is copied where nothing above it was left out, and it is not.
    fn copied(filter: &Filter) -> BTreeSet<String> {
        let tree = tree();
        let mut left_out: Vec<&str> = Vec::new();
        let mut copied = BTreeSet::new();
        for (path, &is_directory) in &tree {
            let under_one_left_out = left_out.iter().any(|out| {
                path.strip_prefix(out)
                    .is_some_and(|rest| rest.starts_with('/'))
            });
            if under_one_left_out {
                continue;
            }
            if filter.excludes(path.as_bytes(), is_directory) {
                left_out.push(path);
                continue;
            }
            copied.insert(if is_directory {
                format!("{path}/")
            } else {
                path.clone()
            });
        }
        copied
    }

    /// Each case comes out as rsync's did, on the same tree: what each rule
    /// matches, in which order, and what is never gone into.
    #[test]
    fn rules_leave_out_what_rsyncs_do() {
        let mut failed = Vec::new();
        for (rules, expected) in RSYNC {
            let rules: Vec<Rule> = rules
                .iter()
                .map(|(include, pattern)| Rule {
                    include: *include,
                    pattern: pattern.as_bytes().to_vec(),
                })
                .collect();
            let expected: BTreeSet<String> =
                expected.iter().map(|path| (*path).to_owned()).collect();
            let got = copied(&Filter::new(&rules));
            if got != expected {
                let rules: Vec<(bool, String)> = rules
                    .iter()
                    .map(|rule| {
                        (
                            rule.include,
                            String::from_utf8_lossy(&rule.pattern).into_owned(),
                        )
                    })
                    .collect();
                failed.push(format!(
                    "{rules:?}: rsync alone {:?}, Ferry alone {:?}",
                    expected.difference(&got).collect::<Vec<_>>(),
                    got.difference(&expected).collect::<Vec<_>>()
                ));
            }
        }
        assert!(failed.is_empty(), "{}", failed.join("\n"));
    }

    /// A rule given with `- ` or `+ ` is that, whatever the option; `!`, and
    /// a pattern no path can match, are refused.
    #[test]
    fn a_rule_is_given_as_rsync_takes_it() {
        let rule = |text: &str, include| Rule::given(text.as_bytes(), include);
        assert_eq!(
            rule("+ top.o", false).unwrap(),
            Rule {
                include: true,
                pattern: b"top.o".to_vec()
            }
        );
        assert!(!rule("- x", true).unwrap().include);
        assert_eq!(rule("-x", true).unwrap().pattern, b"-x");
        assert!(rule("#not a comment here", false).is_ok());
        for fine in [
            "build", "/build", "build/", "a/***", "**/x", "..x", "x.", "/a/b/",
        ] {
            assert!(rule(fine, false).is_ok(), "{fine}");
        }
        for never in [
            "", "/", "./build", "build//", "a//b", "a/./b", "a/../b", "..", "/.", "x/.",
        ] {
            let refused = rule(never, false).unwrap_err().to_string();
            assert!(refused.contains("can match nothing"), "{never}: {refused}");
        }
        assert!(
            rule("!", false)
                .unwrap_err()
                .to_string()
                .contains("not built")
        );
        assert!(rule("- !", false).is_ok());
    }

    /// A server whose root is an entry the transfer names matches each path
    /// after its name, and its root as the name: a rule from the
    /// transfer's root still says.
    #[test]
    fn a_root_named_is_matched_by_its_name() {
        let rules = [
            Rule::given(b"/data/sub/x", false).unwrap(),
            Rule::given(b"/other", false).unwrap(),
        ];
        let inside = Filter::new(&rules);
        let named = Filter::new(&rules).under(Some(b"data"));
        assert!(!inside.excludes(b"sub/x", false));
        assert!(named.excludes(b"sub/x", false));
        assert!(!named.excludes(b"", true));
        assert!(!inside.excludes(b"", true));
        let other = Filter::new(&rules).under(Some(b"other"));
        assert!(other.excludes(b"", true));
        assert!(!Filter::default().excludes(b"x", false));
    }

    /// A file's rules, a line each: a carriage return before the newline
    /// dropped, spaces kept, blank and comment lines left out; a bad line
    /// says which it is.
    #[test]
    fn a_files_rules_are_read_as_rsync_reads_them() {
        let rules = from_file(
            b"# comment\n; comment\n\na.o \r\n c.o\n+ keep\n #x\n",
            false,
        )
        .unwrap();
        let patterns: Vec<(bool, &[u8])> = rules
            .iter()
            .map(|rule| (rule.include, &rule.pattern[..]))
            .collect();
        assert_eq!(
            patterns,
            [
                (false, &b"a.o "[..]),
                (false, b" c.o"),
                (true, b"keep"),
                (false, b" #x")
            ]
        );
        let refused = from_file(b"fine\n./bad\n", true).unwrap_err();
        assert!(
            format!("{refused:#}").starts_with("line 2: "),
            "{refused:#}"
        );
    }
}
