# Governance rule compilation spike (ISOP)

**Date**: 2026-10-19
**Status**: Plan. Cards filed 2026-10-19 (IDs below). No code beyond the probe test.
**Research**: [`docs/research/isop-governance-compilation.md`](../research/isop-governance-compilation.md)
**Governing ADRs**:
- ADR-033 (three-branch governance) and ADR-092 (rule distribution) are
  Accepted. ADR-092's gossip loop is not yet connected.
- The spike adds no ADR. G4 below decides whether one is needed.

## Idea

The governance engine's decision is a finite Boolean function of a few
predicates:
- selector matches,
- the magnitude-over-threshold bit,
- browser and binding-thread predicates.

Compiling that function once gives us:
- a lint that names dead and shadowed rules;
- an exact test that two mesh nodes decide alike;
- a short Minato–Morreale ISOP cover that a person, or a leaf device, can
  read.

## Phases

### G0 — Fix the rule-order defect (prerequisite)

The browser-policy branch resets `has_blocking` (research note, finding 1).
A test confirmed it on 2026-10-19: a blocking rule followed by a matching
Warning-severity browser rule returns `Permit`, and the reverse order returns
`Deny`. Fix this on its own merits, whether or not the spike goes ahead.
The compiled function has to equal `evaluate()`, so `evaluate()` must not
depend on rule order first.

- Change: a non-blocking browser match sets `has_warning` only. It never
  clears flags.
- Test: the probe `isop_probe_warning_browser_rule_does_not_clear_earlier_block`
  becomes a permanent regression test. Add one more test showing that every
  permutation of the default genesis rules gives the same decision.
- Done: both tests pass, `scripts/build.sh test clawft-kernel` is green, and
  clippy is clean.

### G1 — Compile a rule set to a function

New module `clawft-kernel/src/governance_compile.rs`. It is not
feature-gated, matching `governance.rs`. clawft-kernel has no `governance`
feature, despite what the module docs say (research note, finding 4).

- Collect variables: distinct selectors, the `T` bit, and one bit per browser
  and binding rule.
- Derive the don't-care set from selector implications (prefix containment,
  mutually exclusive exact selectors, browser predicates requiring
  `platform=browser`).
- Produce one truth table (bitset) per decision value. Refuse rule sets above
  a variable cap (start at 20) with a typed error.
- Done when: an exhaustive test over every assignment, for the default
  genesis rules plus generated rule sets (proptest), shows that the compiled
  decision equals `evaluate()` on a synthesized request for that assignment.

### G2 — ISOP and lint

- Implement truth-table ISOP (about 150 lines, our own code, no new
  dependency). Test it against hand-checked covers and these properties:
  every cube is prime, the cover is irredundant, and `L ≤ f ≤ U`.
- Lint: for each rule *r*, compare `F` with `F_{-r}`, and report it as dead
  (its selectors never match an allowed assignment) or shadowed.
- Add a `weaver governance lint` subcommand that prints the ISOP per decision
  value and the dead and shadowed rules for the running kernel's active set.
- Done: the lint runs against the live rule set on the Mac kernel, and its
  output is recorded in the card.

### G3 — Equality across nodes

- `compile(rules_a) == compile(rules_b)`, and when they differ, the ISOP of
  `F_a ⊕ F_b` over the shared variables.
- Wire this into `RuleDistribution` as a check that runs after a merge. It
  reports a semantic difference even when versions are equal, but changes no
  `EscalationOutcome`. Changing the escalation behaviour is a separate ADR-092
  amendment, decided after the spike.
- Done: a test with two rule sets that differ only by a D8 overlay reports
  exactly the requests they disagree on.

### G4 — Spike review (decision point)

Write the results into the research note:
- the lint findings on the real rule set,
- the variable counts,
- compile time.

Then decide:
- whether G5 and G6 go ahead;
- whether ADR-092 needs an amendment, so that a semantic difference triggers
  escalation;
- whether a new ADR is needed for compiled tables on the wire.

### Later (not in the spike)

- **G5: explanations.** Return the matching `Deny`/`Escalate` cube in
  `GovernanceResult` next to `evaluated_rules`. The CoS and the audit trail
  show the cube. This replaces the last-match-wins reason text (finding 2).
- **G6: leaf rule tables.** Gossip the compiled covers alongside the source
  rules. A leaf device evaluates a selector table plus the cubes. Blocked on
  the ADR-092 gossip loop.
- **AclPolicy compiler.** Its semantics are first match from the leaf upward,
  so it needs a separate compiler.

## Side card

- **Remove the K0 `check_permission` stub.** `exo_resource_tree::permission::check`
  always returns `Allow`. On 2026-10-19 it had no caller other than its
  re-export. Remove the function and the `check as check_permission`
  re-export, or mark them `#[deprecated]` if an external crate turns out to
  use them.

## Cards

| Phase | Card | State |
|---|---|---|
| G0 | `85f29f99` | todo |
| G1 | `ef5785d6` | todo |
| G2 | `16c8c22c` | todo |
| G3 | `bb4dff9d` | todo |
| G4 | `b58b8384` | todo |
| Side: K0 stub | `eb1d6a2d` | todo |
