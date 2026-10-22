# Security

## Reporting a vulnerability

Report it privately, through GitHub: on the repository's **Security** tab,
choose **Report a vulnerability**
([here](https://github.com/tanchihpin0517/ferry/security/advisories/new)).
Please do not open a public issue for it. You will get an answer there, and
the fix, once released, says what it was.

## Supported versions

Only the latest release is fixed. `ferry update` installs it.

## What Ferry trusts

Ferry can run as root, with `--remote-sudo`. It installs a binary on a host,
and writes what another machine sends. So, what each part trusts:

- **The client trusts the servers it starts.** Each is the client's own
  build: a server of another build is refused. Each runs over the system's
  `ssh`, as the user that `ssh` logs in as, or as root through `sudo`.
  Host keys, and everything else in `~/.ssh/config`, are `ssh`'s to check.
- **The Destination does not trust the Source.** It takes a name only if it
  is one name: not `.` or `..`, with no `/` and no NUL. It takes a
  directory only in the order a scan sends one. It writes only under its
  root, and never follows a symlink that is already in the destination. A
  file takes its name only once its content matches the BLAKE3 hash the
  Source sent with it. What a Source may send is fuzzed: see `fuzz/`.
- **The Source sends only what is under its root,** and only the files of
  its own manifest that the Destination asks for.
- **A release installed on a host is checked against the release's
  `SHA256SUMS`.** The client downloads both from GitHub over HTTPS, and the
  host installs the upload only if its SHA-256 matches. Releases are not
  signed, so this trusts GitHub, and the account that publishes them.
- **The roots never overlap.** A DESTINATION that is SOURCE, is inside it,
  or holds it is refused, and the transfer stops if a mount puts one inside
  the other while it runs.
