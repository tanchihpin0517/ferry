#!/usr/bin/env python3
"""What rsync copies of one tree under each list of include and exclude
rules, written as Rust for Ferry's unit tests in src/filter/rsync_cases.rs:
each case, its rules as (include, pattern), and the paths rsync copied, a
directory's ending in /. Run it with rsync on the PATH, from anywhere:

    python3 -I scripts/rsync-filter-cases.py > src/filter/rsync_cases.rs
    cargo fmt

The trees are made in a temporary directory, and removed."""
import os, shutil, subprocess, tempfile

HERE = tempfile.mkdtemp()
DIRS = ['build/in', 'a/build', 'a/sub/foo', 'sub/foo', 'xsub/foo', 'a/b/c', 'x/a/b', 'c', 'd', 'e/build', 'sp ace', '[x]']
FILES = ['buildfile', 'a/build2', 'top.o', 'a/deep.o', 'a/deep.c', 'x/build', 'c/x', 'd/c', 'x/a/b/d', 'x/a/b/e',
         'build/top', 'build/in/1', 'a/build/3', 'q1', 'qa', 'f*', 'f[1]', 'sp ace/z', '[x]/y', 'a/b/c/z', '.hidden',
         'a/.hidden', 'e/build/f']
LINKS = [('e/link', 'c')]

def tree(root):
    shutil.rmtree(root, ignore_errors=True)
    for d in DIRS:
        os.makedirs(os.path.join(root, d), exist_ok=True)
    for f in FILES:
        open(os.path.join(root, f), 'w').close()
    for path, target in LINKS:
        os.symlink(target, os.path.join(root, path))

def listing(root):
    out = []
    for dirpath, dirnames, filenames in os.walk(root):
        for name in dirnames + filenames:
            path = os.path.join(dirpath, name)
            rel = os.path.relpath(path, root)
            out.append(rel + '/' if os.path.isdir(path) and not os.path.islink(path) else rel)
    return sorted(out)

def case(rules):
    s, d = os.path.join(HERE, 's'), os.path.join(HERE, 'd')
    tree(s)
    shutil.rmtree(d, ignore_errors=True)
    os.makedirs(d)
    args = [f'--filter={"+" if inc else "-"} {pat}' for inc, pat in rules]
    subprocess.run(['rsync', '-a', *args, s + '/', d + '/'], check=True, stderr=subprocess.DEVNULL)
    return listing(d)

SINGLE = ['build', '/build', 'build/', '/build/', 'sub/foo', 'a/*', 'a/**', '*.o', '*/', '*', 'c/***', 'build/*',
          'build/**', 'a**d', '**/foo', 'a/**/c', 'a/**/b', 'q?', 'q[0-9]', 'q[!0-9]', 'q[[:digit:]]', 'q[^0-9]',
          'f\\*', 'f\\[1\\]', '[[]x]', 'sp ace', '.*', '*/c', 'x/*/b', '*/*/b/*', 'link', 'link/', 'b/c/', '**/b/c',
          '*a*', 'e/*/f', 'build*', '**build', '***', 'x/a/b/', 'a/b/c/z', '/a/b', 'q[a-c]', 'q[]a]']
CASES = [[(False, p)] for p in SINGLE] + [
    [(True, '/a/deep.o'), (False, '*.o')],
    [(True, '*/'), (True, '*.c'), (False, '*')],
    [(True, '*.c'), (False, '*')],
    [(True, 'a/'), (True, 'a/build/***'), (False, '*')],
    [(False, 'build'), (True, 'build')],
]

def raw(text):
    return 'r#"' + text + '"#'

version = subprocess.run(['rsync', '--version'], capture_output=True, text=True, check=True).stdout.split()[2]
print(f'// Made by scripts/rsync-filter-cases.py, from rsync {version}: each case, its')
print('// rules, and what rsync copied of the tree.')
print('pub const TREE_DIRS: &[&str] = &[' + ', '.join(raw(d) for d in DIRS) + '];')
print('pub const TREE_FILES: &[&str] = &[' + ', '.join(raw(f) for f in FILES) + '];')
print('pub const TREE_LINKS: &[(&str, &str)] = &[' + ', '.join(f'({raw(p)}, {raw(t)})' for p, t in LINKS) + '];')
print('/// A case: its rules, each whether it includes, and its pattern; and what')
print('/// rsync copied.')
print("pub type Case = (&'static [(bool, &'static str)], &'static [&'static str]);")
print('pub const RSYNC: &[Case] = &[')
for rules in CASES:
    copied = case(rules)
    print('    (')
    print('        &[' + ', '.join(f'({str(i).lower()}, {raw(p)})' for i, p in rules) + '],')
    print('        &[' + ', '.join(raw(p) for p in copied) + '],')
    print('    ),')
print('];')
shutil.rmtree(HERE)
