# R9 / G3: rule order and precedence across nodes and layers

Work order WO-036 (pile AE). Tree: branch `w2/ae-r9-g3-rule-order`, base `9523587e1`.
Line numbers are for this branch. Follows `g0-reverification-2026-11-07.md`, which closed the
single-node case (G0) and left rule distribution and merge order (G3, card `bb4dff9d`) open.

## Summary

- **One node.** The verdict of `GovernanceEngine::evaluate` is a function of the *set* of matching
  rules, not their order. Order changes only the reason text. Strictness order is
  Deny > Escalate > PermitWithWarning > Permit.
- **Across nodes** the verdict also depends on three things that are not rules: the risk threshold, the
  human-approval flag, and operator exemptions. Two nodes with equal rule sets can still differ.
- **One real bug found and fixed:** `RuleDistribution::merge` did not converge on an exact version
  tie (F4 below).
- **Two owner decisions** (F1 exemption beats overlay deny; F2 last-writer-wins can loosen) and
  **one inert feature** (F3 cross-node escalation never forwards).
- No way was found for a Warning or Advisory rule, a rule position, or a gossip delivery order to turn a
  Deny or Escalate into a Permit. The ways a permit can beat a deny are the ones listed in
  "Every way a permit can beat a deny or escalate".

## How order is decided on one node

`evaluate` (`crates/clawft-kernel/src/governance.rs:1256`) iterates `active_rules()` (`:1217`, insertion
order, no sort). Every decision input starts false and is only ever set true (`:1285-1296`):
`has_blocking`, `has_warning`, `hard_deny`, `needs_approval`, `other_blocking`. The verdict is computed
once after the loop (`:1388`):

| Condition | Decision |
|---|---|
| any blocking match, and not `hard_deny`, and (`human_approval_required` or (`needs_approval` and not `other_blocking`)) | EscalateToHuman |
| any blocking match otherwise | Deny |
| no blocking, magnitude over threshold, a warning matched | PermitWithWarning |
| otherwise | Permit |

Consequences:

- A rule's position cannot matter to the kind of verdict. Tested for every order of the merged
  parent + overlay set in `governance_g3_order_tests.rs` (120 orders per case).
- **Deny beats Escalate.** An overlay approval rule (`OVERLAY_APPROVAL_TAG`) only escalates when no
  other blocking rule matched (`needs_approval && !other_blocking`). `other_blocking` is set by any
  blocking general rule without the approval tag (`:1363`), by blocking browser-policy rules (`:1317`)
  and by blocking binding-thread rules (`:1335`).
- **Overlay deny is never escalated.** `hard_deny` is set from `OVERLAY_DENY_TAG` on general,
  browser-policy and binding-thread rules (`:1318`, `:1336`, general at `:1361`), and the escalation
  branch requires `!hard_deny`.
- **Warning and Advisory never touch the blocking flags.** The non-blocking browser-policy and
  binding-thread branches only set `has_warning` (`:1323`, `:1346`). That was G0.
- The reason text comes from the first blocking match (`note_blocking_reason`, `:1730`), except that a
  binding-thread block always supplies it. This is the only place order shows.
- `evaluate_in_environment` (`:1449`) runs `evaluate` and can only replace the result with a Deny
  (`:1483`).
- Selectors: a rule whose selectors do not match is skipped (`:1303`). An empty selector string
  matches everything (`selector_matches`, `:246-249`), so a gossiped rule with `action_selector = ""`
  is universal, which is the strict direction. `tool_selector` reads the request's own `tool` or
  `gate_action` context value (`selectors_match`, `:158-176`). A rule that selects on tool is only as
  good as the caller's honesty about the tool name; Tapestry sets it from its own tool call, not from
  model output.
- A blocking rule with no `force_on_match` applies only when the effect magnitude is over the
  threshold (`:1355`). A caller that sends no effect (Tapestry sends none, see
  `docs/tapestry/r9-policy-layer.md`, "What is not done") has magnitude 0, so such a rule never
  fires. Rules meant for tool calls need `force_on_match`.

## Merged layers: parent policy + overlay (one node)

`governance_overlay::merge` (`governance_overlay.rs:415`) is tighten-only:

- The parent's rules are carried over verbatim (`:446`), then the overlay's deny and approval rules are
  appended (`rules.extend(added)`, `:501`). The overlay can add, never remove or deactivate.
- The overlay's `human_approval_required` is OR-ed. When the overlay raises it and the parent did not
  have it, the parent's blocking rules are re-tagged `OVERLAY_DENY_TAG` (`:456-466`) so the engine-wide
  flag cannot turn the parent's denies into approvable prompts (review M1).
- An approval glob that overlaps a *visible* parent deny is refused (`:439`). "Visible" means
  `parent_view` (`:374`): active, blocking or critical, `force_on_match`, general, no `tool_selector`.
  A parent deny outside that view is accepted next to an overlay approval; the engine still answers
  Deny (test `an_approval_overlapping_a_deny_the_merge_cannot_see_is_still_a_deny`).
- `effective_hash` sorts rules by normalized id then content (`sorted_rule_values`, `:239`) and covers
  the limits (`limits_value`, `:224`). Two nodes with the same parent and overlay get the same hash
  whatever order the parent lists its rules (test
  `nodes_with_the_same_parent_and_overlay_agree_whatever_the_rule_listing_order`).
- Rules reach the engine in `effective.rules` order (`overlay_runtime.rs:585`), which is parent order
  then overlay order. Harmless, per the section above.

## Across nodes

### Rule distribution (`rule_distribution.rs`)

- `merge` (`:165`) is last-writer-wins per rule id: higher `version`, then higher `updated_at_unix`,
  then greater `origin_node` (`remote_wins`, `:192`). Ids are exact strings; the overlay's
  case-insensitive collision check (`norm_id`) does not apply here.
- Two rules with the same id from different origins are not merged field by field; one body replaces
  the other entirely. Strictness plays no part.
- `active_rules()` (`:140`) iterates a `HashMap`, so each node feeds its engine the rules in a different
  order. Decisions are the same (see above); the reason text can differ between nodes.
- `set_version` (`:146`) is a sum of versions. Two different sets can have the same sum, so it is only
  a cheap "might differ" hint, never an equality check. G3's compile-and-compare is what would replace
  it.
- Nothing in this module signs or authenticates an envelope. In this tree `RuleDistribution` is
  exported (`lib.rs:642`) and used by `workload_governance` tests and docs only; the daemon builds its
  gate from the overlay runtime's effective rules (`overlay_runtime.rs:571-590`), not from gossip.
  So the hazards below are not reachable from a running daemon today.

### What else decides the verdict on a node

| Input | Where | Effect across nodes |
|---|---|---|
| risk threshold | `GovernanceEngine::new`, `Effective::risk_threshold` | same rules, effect 0.5: Deny on one node, Permit on another |
| `human_approval_required` | same | same blocking match: Deny on one node, Escalate on another |
| `exempt_actions` | `gate.rs:446`, applied at `:583` and `:610` | Deny or Escalate becomes Permit for that exact action |
| Production environment factor | `evaluate_in_environment:1449` | threshold halved; only adds Denies |
| per-principal rate limit | `evaluate:1262` | Deny when exhausted; per node |

The overlay hash covers the limits, so overlay-merged nodes with different limits never look equal
(test `rule_sets_are_not_the_whole_verdict_limits_are_hashed_too`). `RuleGossipEnvelope` carries no
limits, no exemptions, and no hash at all.

### R9 (Tapestry) side

Tapestry asks `governance.check` and maps Deny to refusal and Escalate to a forced prompt; it never
answers allow (`docs/tapestry/r9-policy-layer.md`, "Decision mapping"). Layer order there is
strictest-wins, so Tapestry cannot loosen a WeftOS verdict. Two inputs it does not control are the
ones in the table above: the node's threshold and flag, and `grant_applied`, which Tapestry ignores in
the reply and which is `true` exactly when F1 applied. No fork change was needed.

## Every way a permit can beat a deny or escalate

1. **F1, operator exemption** (`gate.rs:583`, `:610`). `exempt_action(name)` flips Deny and Escalate to
   Permit for that exact action, including an overlay's hard deny. It is the documented operator grant
   (`gate.rs:385-395`) and is witnessed as `governance.grant`, but it means the project overlay
   (ADR-103 D8, "an overlay deny also stops tool dispatch") does not stop an action the operator
   exempted. The swap keeps exemptions on purpose (`overlay_runtime.rs:170`, test
   `a_swap_keeps_the_per_action_exemptions_of_the_running_gate`). **Owner decision:** should an
   `OVERLAY_DENY_TAG` deny outrank an operator exemption? Pinned by
   `known_hazard_an_operator_exemption_overrides_an_overlay_hard_deny`, which fails if the precedence
   changes.
2. **F2, last-writer-wins can loosen.** A higher-version rule with `active = false`, a lower severity,
   or no `force_on_match` replaces a blocking rule on every node it reaches (`merge`,
   `remote_wins`). There is no authority check on the envelope, and the version counter saturates at
   `u64::MAX` (`:53`), after which that rule can no longer be edited by a higher version. Not reachable
   from the daemon today (see above). **Owner decision** before gossip is wired in: sign envelopes by
   origin, and decide whether a remote edit may loosen at all (tighten-only like the overlay, or loosen
   only from an authorised origin). Pinned by `known_hazard_a_newer_edit_can_loosen_a_rule_everywhere`.
3. **Zero effect.** A blocking rule without `force_on_match` does not fire for a caller that sends no
   effect (`:1355`). Not a bug; a rule-authoring rule. Use `force_on_match` for tool-call rules.
4. **Selector evasion.** `tool_selector` trusts the request's `tool` or `gate_action` context key
   (`:164-174`). A caller that can choose its own context can leave the tool out. The daemon's
   `governance.check` takes `tool` from the caller too (`governance_check_rpc.rs`, `parse`), so the
   trust sits with the client (Tapestry sets it from the real tool call).
5. **Different node parameters** (table above): a node with a higher threshold or `human_approval`
   off permits or denies where another escalates.
6. **Not through the rule set:** a rule's position, a Warning or Advisory rule next to a block, a
   delivery order of gossip, and the `HashMap` load order. All tested to change nothing.

## Findings and closure

| # | Finding | Closure |
|---|---|---|
| F4 | `merge` did not converge on an exact version tie (same version, time and origin, different body): each node kept whichever body it saw first, so the cluster stayed split for good. | **Fixed** in `rule_distribution.rs` (`body_wins`): the stricter body wins (active, higher severity, `force_on_match`), then the serialized text, so the result is the same in every order. Test `exact_version_tie_converges_and_the_stricter_body_wins` fails without the fix. |
| F1 | Exemption beats overlay deny. | **Owner decision**, pinned by test. |
| F2 | LWW can loosen; envelopes unauthenticated. | **Owner decision**, pinned by test. |
| F3 | `escalate` can never forward after a real merge: `merge` accepts any higher version, so no remote hint stays above the local version, and `Forward` only occurs in the unit test that writes `remote_hints` directly. Local deny always stands, which is the safe side. | **Owner decision** (ADR-092 amendment, per `docs/plans/governance-isop-spike.md` G3). Pinned by `known_gap_escalation_cannot_forward_after_a_real_merge`. |

## Tests added

In `rule_distribution.rs` (`mod tests`):

- `exact_version_tie_converges_and_the_stricter_body_wins` (the fix)
- `merge_converges_in_every_delivery_order` (6 orders of 3 envelopes, plus re-delivery is a no-op)
- `converged_nodes_decide_alike_whatever_order_their_rules_load_in` (both orders of the load, both
  human-approval settings, six requests)
- `known_hazard_a_newer_edit_can_loosen_a_rule_everywhere` (F2)
- `known_gap_escalation_cannot_forward_after_a_real_merge` (F3)

In `governance_g3_order_tests.rs`:

- `deny_beats_escalate_beats_permit_in_every_position`
- `an_overlay_that_sets_human_approval_never_softens_a_parent_deny`
- `an_approval_overlapping_a_deny_the_merge_cannot_see_is_still_a_deny`
- `nodes_with_the_same_parent_and_overlay_agree_whatever_the_rule_listing_order`
- `rule_sets_are_not_the_whole_verdict_limits_are_hashed_too`
- `known_hazard_an_operator_exemption_overrides_an_overlay_hard_deny` (F1)

## Not covered

- The ISOP compile-and-compare (G1 to G3 as planned); this review is the order analysis only.
- Gossip transport, peer authentication and chain-replicated rule changes: no code in this tree
  distributes rules to a running engine.
- Rule-set size or timing limits.
