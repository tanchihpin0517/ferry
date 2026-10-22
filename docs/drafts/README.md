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
| [rsync-parity.md](rsync-parity.md) | draft | tests that run rsync and Ferry on the same trees and compare the results, and the differences that are deliberate (in Chinese) | DST-06, DST-13, SRC-09, SRC-13; D9, D10 |

Proposals written as part of a review, kept where the review is:

| Proposal | Status | About |
|---|---|---|
| [ARC-01 to ARC-05](../review/2026-10-17/architecture.md) | proposed, not decided | the write stage, how threads are put together and stopped, who owns the Destination's state, segmented records, smaller changes |
| [Target architecture](../review/2026-10-17/target-architecture.md) | long-term direction | how Ferry would be designed from scratch, and the steps from here to there |

A proposal that starts being worked on gets a draft here, which links back
to it.

Figures for drafts and proposals are in [assets/](assets/):

- [stages-and-stop.svg](assets/stages-and-stop.svg): the supervisor of ARC-02
  in one server, its stages, the stop, and what happens when a writer fails.

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
