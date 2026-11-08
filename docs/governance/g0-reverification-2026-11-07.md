# G0 re-verification: WeftOS Claude step

Status: **signed off by Mathew, 2026-11-07 (~10:06 PM ET).** He overrode the WeftOS Cogs Bot review step
because the Cogs Bot is offline until Monday. The R9 policy layer may merge once its own tests pass.

Date: 2026-11-07. Tree: `target-0.8.5` at `fe8e81104` (local; same tree as `16f21c646`). Branch with the
probe and this note: `test/g0-reverify-20261107`.

## What G0 was

Card `85f29f99`, found 2026-10-19. In `GovernanceEngine::evaluate`, a Warning-severity `browser_policy`
match set `has_blocking = false`. "Spawn approval, then a Warning BP-002" gave Permit; the reverse order
gave Deny. Fixed in `fe59c46ab` (2026-10-23).

## What I checked

1. **The fix is on the branch.** `fe59c46ab` is an ancestor of `fe8e81104`. No later commit touches
   `crates/clawft-kernel/src/governance.rs`.
2. **Code reading** (`governance.rs`, `evaluate`, lines 1256-1410):
   - Every decision input (`has_blocking`, `has_warning`, `hard_deny`, `needs_approval`,
     `other_blocking`) starts false and is only ever set to true. No branch clears a flag. The verdict is
     computed from the flags after the loop, so the verdict is a function of the set of matching rules,
     not their order.
   - The non-blocking browser branch only sets `has_warning`. Its comment says it "never touches another
     rule's flags", and the code agrees.
   - `active_rules()` does not sort, so tests that permute insertion order really permute evaluation order.
   - Order still affects **text only**: the first blocking rule supplies the Deny/Escalate reason
     (`note_blocking_reason`), except a binding-thread block, whose canonical reason always wins. This is
     documented and does not change the decision.
   - `evaluate_in_environment` runs `evaluate` and can only replace the decision with a Deny when the
     environment-adjusted magnitude is exceeded. It cannot loosen a verdict.
3. **Shipped tests:** `cargo test --locked -p clawft-kernel --lib governance` gives 197 passed, 0 failed,
   including `isop_probe_warning_browser_rule_does_not_clear_earlier_block` and
   `rule_order_does_not_change_decision` (720 orders).
4. **A wider probe** (new: `crates/clawft-kernel/tests/g0_rule_order_reverify.rs`):
   - Four rule sets that mix every severity and rule kind the evaluator branches on: general
     Blocking/Warning/Advisory, overlay approval, overlay deny, browser_policy at Blocking and Warning,
     binding-thread at Blocking and Warning, and force-on-match.
   - Five request shapes: browser spawn at low and high effect, native high and low effect, and a
     binding-thread mismatch.
   - Both `human_approval_required` settings.
   - Every permutation of each set: 40 cells, 554,400 evaluations, a fresh engine per order.
   - Result: **every cell has exactly one decision kind.** All four kinds were reached (Permit,
     PermitWithWarning, EscalateToHuman, Deny), so the probe isn't vacuous.
   - The original G0 pair denies in both orders.
   - Run: `cargo test --locked -p clawft-kernel --test g0_rule_order_reverify -- --nocapture`, which
     gives 2 passed.

## Verdict (WeftOS Claude)

**G0 is fixed on `target-0.8.5`.** Rule order cannot change a governance decision in `evaluate` or
`evaluate_in_environment`.

## Open items for the reviewer, not G0 itself

- **K0 stub still present.** `exo_resource_tree::permission::check`, re-exported as `check_permission`
  from `exo-resource-tree/src/lib.rs:63`, returns `Allow` for everything. It has no callers in the
  workspace today (card `eb1d6a2d`, still todo). The R9 policy layer must call
  `GovernanceEngine`/`GateBackend` and never this function. Removing or deprecating it before R9 ships
  would close the trap.
- **Stale plan:** `docs/plans/governance-isop-spike.md` still lists G0 (`85f29f99`) as `todo`.
- **AclPolicy** is first-match by design (leaf upward). That is order-dependent on purpose and outside
  G0.
- Not covered here: rule distribution and gossip merge order across nodes (G3, `bb4dff9d`), and
  overlay file merging beyond the existing `governance_overlay_tests` hash-order test.

## Sign-off

- [x] WeftOS Claude: verified, 2026-11-07 (this note)
- [~] WeftOS Cogs Bot: review overridden by Mathew (Cogs Bot offline until Monday)
- [x] Mathew: signed off, 2026-11-07 ~10:06 PM ET
