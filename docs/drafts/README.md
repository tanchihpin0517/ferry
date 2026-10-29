# Design drafts

[中文](README.zh-TW.md)

Changes to the design that are being worked out, before they are decided.
`docs/design/` says what Ferry is and is decided to be; this directory holds
what it may become, while that is still being discussed or built.

## How a draft lives

1. **Draft.** A new file here, `docs/drafts/<topic>.md`, from the template
   below. One language is enough: a draft has no twin until it is accepted.
2. **Under discussion.** Open questions are listed in the draft, and answered
   in it, so that the draft always says where the discussion stands.
3. **Accepted.** The decision is written into `docs/design/`, in English and
   in its `.zh-TW.md` twin, in the same commit as the code that first needs
   it, or before.
4. **Merged or dropped.** The draft is then deleted, with its row below; git
   keeps its history. A dropped draft goes the same way, and its reason goes
   into the design document's **Not chosen**, if it is worth keeping.

A draft never says how Ferry behaves today: for that, read `docs/design/` and
the code.

## Drafts

| Draft | Status | About | Related |
|---|---|---|---|
| [preparation.md](preparation.md) | accepted: steps 1 to 5 done | preparing a transfer as an explicit state machine: the steps, the order, and the review items each holds (in Chinese) | target architecture §6; WP-01, WP-09, WP-10; D11 |
| [architecture-redesign.md](architecture-redesign.md) | accepted: B1 done, the manifest compressed as one stream, and A1's six steps: data in blocks, the Source's owner, the data credit, and errors said by the decider; large files read slowly from a spinning disk, found after, are now read in order; B1's completion done: the Destination's decider does no I/O, and follows the finishing, the files in segments, and the batches, the writing thread gone, the Wants sent as each part is compared; A2's first two steps: the session's core, tested against another over made-up streams, and its driver and streams' executors, over socketpairs, not wired yet; its third: reports on the servers' control connections; and its fourth: the client makes the streams and hands them to the servers, touching no data, the peers going over the session, and the relay, the global reordering, and the heartbeats gone; and its fifth: the client makes a stream that broke again, logging its SSH connection in again, asking on the terminal where it must; and its sixth, the acceptance: the design documents and figures as built, copies whose streams break again and again tested, a file of 4 GiB about a tenth faster; the servers' control connections are not judged by the session, so the first SSH connection dropping still fails a transfer; then, on 2026-10-25, decided again: as nano4's host name leads to several login nodes, and each SSH connection may land on another, each host gets one SSH connection, a drop fails the transfer, there is no reconnecting, `--ssh-connections` goes, and the session becomes a thin layer over one stream; A2's reconnecting taken back out the same day, in five steps: one SSH connection, a thin layer over one stream, no reconnecting, the client judging each server by its control connection's silence, and the acceptance, which found two faults, and CI four more after the push, all fixed; nano4 tested after the push: `ferry test` three ways, 39 passing each, and 1 GiB copied and synced each way; on 2026-10-28, B2 decided on: one lister by default, the same K on both sides, `--listers`, its plan written, not started | the architecture from a high level: reliability end to end, in a session at both ends, in blocks that can be sent again (taken back on 2026-10-25: one SSH connection, failing when it drops); only the order that is needed; the client still makes every connection, and hands both ends to the servers without touching the data; credits from the Destination; in each peer, one owner of its state and parallel executors, with only the Destination deciding; connection health judged in one place; what was measured, and improvement options (in Chinese) | target architecture; ARC-04, D16; Reconnecting in TODO.md |
| [fsync.md](fsync.md) | parked: fsync taken out on 2026-10-24, `--fsync` refused; to be looked at again | why Ferry synced every file, how, what it cost as measured on four filesystems, what rsync does, why it was taken out, what is lost without it, and the ways to bring it back (in Chinese) | c06ff61; ARC-01d, WP-19a, WP-15b |
| [protocol.md](protocol.md) | draft: 2026-10-28, with the pipes, now in the design | an overview: every connection a transfer uses, on pipes or as they are; each one's flow, to the byte; how those on pipes send: layers, states, settings, pipes, the packets as a draft, compression, windows, ending and failing; and what differs from now (in Chinese) | the pipes, in [architecture](../design/architecture.md#transport) |
| [large-directories.md](large-directories.md) | draft: 2026-10-28 | directories too large: names past 64 MiB sorted a run at a time into temporary files, then merged, so that memory has a bound; the files in `std::env::temp_dir()`, nameless from the start; the Destination's `sync` merged as it reads (in Chinese) | the redesign's B2, the pipes |

Proposals written as part of a review, kept where the review is:

| Proposal | Status | About |
|---|---|---|
| [ARC-01 to ARC-05](../review/2026-10-17/done/architecture.md) | ARC-01, 02, 03 and 05 done; [ARC-04](../review/2026-10-17/architecture.md) declined (D16: no segments until resuming a file is wanted), then taken up by the redesign (2026-10-23: data in blocks) | the write stage, how threads are put together and stopped, who owns the Destination's state, segmented records, smaller changes |
| [Target architecture](../review/2026-10-17/target-architecture.md) | long-term direction | how Ferry would be designed from scratch, and the steps from here to there |

A proposal that starts being worked on gets a draft here, which links back
to it.

Figures for drafts and proposals are in [assets/](assets/):

- [stages-and-stop.svg](assets/stages-and-stop.svg): the supervisor of ARC-02
  in one server, its stages, the stop, and what happens when a writer fails.
- `redesign-*.svg`: the 17 figures of
  [architecture-redesign.md](architecture-redesign.md), each where the draft
  uses it.
- `protocol-*.svg`: the 11 figures of [protocol.md](protocol.md): every
  connection, the flow of each (7), the layers, a connection's states, and the
  packets once ready.

## Template

```markdown
# <Topic>

- Status: draft | under discussion | accepted | dropped
- Started: YYYY-MM-DD; last changed: YYYY-MM-DD
- Changes: which sections of docs/design/ this would change
- Related: review items, issues, commits

## Problem

What is wrong or missing today, with evidence: measurements, failures, code.

## Proposal

What changes, and how, in enough detail to build it.

## Alternatives

What else was considered, and why not.

## Open questions

- [ ] Each question, and who decides it.

## Plan

The steps, each ending with something that runs and is tested.
```
