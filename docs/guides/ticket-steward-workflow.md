# Ticket steward workflow

The steward runs beside each authoritative project board. It examines every
ticket on a bounded sweep, collects evidence, decides whether the current
status and assignee still match the work, and records a durable receipt. The
portfolio dashboard receives the source board's published snapshot only after
the source change is confirmed. A subscribed item is never a write target.

## Authority by board

| Board | Steward write path | Portfolio update |
|---|---|---|
| WeftOS and other native dashboard boards | Project-scoped harness credential and conditional ticket action | Already native; no snapshot |
| BakeOS OS board | Native machine transition command for allowed transitions; staff approves reassignment | Host-local snapshot publisher |
| Client product board | Existing product-board steward lane, respecting protected cards and machine-write policy | Host-local approved publisher |
| FlipsOS native board | Native ticket store with a dedicated machine actor and revision check | Project snapshot publisher |
| Shasta restoration board | Its own `/board` writer after an adapter exists | Restoration subscription publisher |

Other projects use the same contract when a board is enrolled. A project
registration must name exactly one source board and one authorized writer.
The dashboard's subscription token is publish-only and is not a source-board
credential.

## Per-ticket state machine

Persist a run keyed by `(project, ticket ID, source revision, evidence digest)`.
The ledger belongs to the source project and survives process restarts.

1. **Observed:** read one source ticket, its events, comments, linked work, and
   available receipts. Record the source revision and last observed time.
2. **Assessed:** save an evidence bundle with source URLs/IDs, timestamps,
   digest, proposed status and assignee, reason, and policy outcome. An absent
   or stale receipt is not proof of completion. A merged PR alone does not
   establish delivery. Never invent an owner from ticket text.
3. **Queued or ready:** send ambiguous or policy-restricted changes to the
   responsible person. Deterministic changes may advance only if the project's
   transition policy explicitly allows the machine action. An unchanged ticket
   gets a checked receipt so every ticket has sweep coverage.
4. **Applying:** re-read the source revision, then use its authorized writer
   with a compare-and-swap precondition and a stable action key. If the source
   changed, discard the proposal and reassess. A local mutex alone cannot
   protect two WeftOS nodes.
5. **Source confirmed:** read the ticket again and verify the target state and
   source event ID. If a crash occurs after the write, this read lets the next
   run recognize the committed action instead of repeating it.
6. **Publishing:** issue a full source-board snapshot via the project's
   existing publisher; verify the dashboard's acknowledged snapshot/count and
   record its ID. A failed publish stays pending and retries without rewriting
   the source ticket.
7. **Complete:** store the final receipt with actor, policy version, evidence
   digest, source revision/event, and dashboard snapshot. Subsequent sweeps
   can reassess when new evidence or a new source revision arrives.

The ledger's transitions are monotonic. Failed steps retain an error, attempt
count, and next retry time. Use exponential backoff with a cap, dead-letter
after repeated authorization or schema failures, and show pending publication
separately from pending source action. Sweep cursors are durable and paginate
the entire board; deletion or status filters must not silently skip tickets.

## Decision policy

- Move `todo` to `in_progress` only with an accepted claim or source work-start
  receipt. Move `in_progress` to `review` only when implementation and required
  local checks have a concrete receipt. Move to `done` only with accepted review
  and the project-specific delivery evidence. Reopen when an explicit source
  regression or human decision supersedes the prior completion.
- Reassignment requires an explicit accepted owner or handoff event and a
  project writer permitted to change the assignee. BakeOS machine workers do
  not perform human-only assignee edits.
- Cite exact evidence in each proposal. A model may summarize and rank
  evidence, but cannot authorize a transition. When evidence conflicts, queue
  a review and leave source status and assignee unchanged.
- Protected or client-confidential cards follow source export policy. A
  dashboard receipt may contain only the source-approved projection.

## Operational checks

Schedule a bounded sweep in each project, and allow manual `--once` replay.
Track total scanned, unchanged, queued, applied, conflicts, source errors,
pending publication, last successful sweep, and oldest unreviewed ticket.
Alert on a stuck cursor, repeated write conflicts, or snapshot lag. Test
crashes at the source-write and publish boundaries, two-node races, revoked
credentials, and a ticket edited by a person between assessment and apply.

The first implementation pass establishes the ledger and conservative
decision path on native dashboard, BakeOS, a client project, FlipsOS, and Shasta boards.
Shasta's conditional source writer is staged in a project branch; its steward
is not active until that branch is released and a project schedule runs it.
No steward should report a project as active merely because its snapshot
subscription exists.

## Native dashboard runner

`node scripts/ticket-steward.mjs --decisions <file>` scans every ticket visible
to the selected `wfb_` credential and writes a mode-600 checkpoint at
`~/.local/state/weftos/ticket-steward.json`. Add `--apply` to send approved
changes to the conditional source API. The credential remains in the usual
mode-600 board token file. Run one instance per project credential and set
`WEFTOS_STEWARD_STATE_FILE` to a different host-local path for each project.

The decisions file has `schema: 1` and a `decisions` array. Each decision names
`ticket_id`, `expected_status`, `expected_updated_at`, `action` (`status` or
`assign`), `value`, `authorization: "human"`, `approved_by`, and `evidence` with
a reason and typed HTTPS references. The current rules require a claim for
work start, change plus checks for review, review plus delivery for done, a
regression for reopening, or an accepted handoff for reassignment. A decision
without those receipts is queued. This first runner treats supplied references
as attestations and requires a named human approval; it cannot yet verify the
linked artifacts independently. Its source action key is stable across crash
replay, while the dashboard RPC checks the source revision and deduplicates a
successful action. The next step is source-specific evidence verifiers and a
schedule under each project's process manager.
