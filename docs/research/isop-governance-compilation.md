# ISOP (Minato–Morreale) for governance rule compilation

**Date**: 2026-10-19
**Status**: Research. No production code. The spike plan is
[`docs/plans/governance-isop-spike.md`](../plans/governance-isop-spike.md).
**Scope**: `clawft-kernel::governance` (`GovernanceEngine`, `GovernanceRule`),
`exo-resource-tree::permission` (`AclPolicy`, `CapabilityChecker`),
ADR-033, ADR-034, ADR-066, ADR-092, ADR-103 D8.

## Summary

The Minato–Morreale algorithm computes an *irredundant sum of products*
(ISOP). That is a short OR-of-ANDs form of a Boolean function, and each AND
term in it is as general as the input allows. It takes the function as an
interval `[L, U]` and is free to use any don't-care in between.

The decision our governance engine makes is a finite Boolean function of a
small number of predicates. Once that function is written out, ISOP and the
decision diagrams under it give us four things we lack today:

1. A lint that finds dead and shadowed rules.
2. An exact equality check between two nodes' rule sets after an ADR-092 merge.
3. Short, flat rule tables that leaf devices can enforce.
4. Minimal explanations for denials.

Writing out the function also forces a precise statement of how the engine
combines rules. Reading `evaluate()` for this note turned up one rule-order
dependence. It is recorded under [Findings](#findings).

## The algorithm

Input: two Boolean functions with `L ≤ U`.
- `L` is the ON-set: inputs the cover must include.
- `U` is the ON-set plus the don't-care set: inputs the cover may include.

Output: a set of cubes (AND terms) whose OR, `f`, satisfies `L ≤ f ≤ U`. Every
cube is a prime implicant of `U`, and no cube can be removed without dropping
part of `L`.

```
ISOP(L, U):
  if L = 0: return ∅                         // nothing to cover
  if U = 1: return {⊤}                       // the empty cube covers all
  x   = top variable of L, U
  L0, L1 = cofactors of L on x = 0 / x = 1
  U0, U1 = cofactors of U on x = 0 / x = 1

  C0 = ISOP(L0 ∧ ¬U1, U0)                    // must carry ¬x
  C1 = ISOP(L1 ∧ ¬U0, U1)                    // must carry x
  f0 = OR(C0); f1 = OR(C1)

  L* = (L0 ∧ ¬f0) ∨ (L1 ∧ ¬f1)               // still uncovered
  U* = U0 ∧ U1                               // safe without x
  C* = ISOP(L*, U*)                          // independent of x

  return {¬x·c | c ∈ C0} ∪ {x·c | c ∈ C1} ∪ C*
```

Properties that matter here:

- **Prime and irredundant.** Each cube is as general as the interval allows,
  and no cube is redundant.
- **It uses the don't-cares.** Input combinations that cannot occur widen `U`,
  and that shortens the cubes.
- **It returns the function too.** On BDDs, the recursion also returns the
  BDD of `f`, so equality checks come free.
- **Good, not minimal.** The LIRMM analysis (see Sources) gives functions
  where it returns the worst-case ISOP. Its size ratio to a minimum SOP is unbounded as variable
  count grows. "Short and irredundant" is a safe claim; "optimal" is not.
- **Cost.** This is polynomial in BDD size. For the variable counts we have
  (see below), a truth-table (bitset) version is simpler than a BDD and is
  fast enough.

Implementations to read: Spot `spot::minato_isop` (a non-recursive version
that takes a BDD or a min/max pair), CUDD `Cudd_zddIsop`, and the
mockturtle/kitty `isop` used by ABC-family tools (truth-table based).

## How the engine combines rules today

These notes come from reading `GovernanceEngine::evaluate`
(`crates/clawft-kernel/src/governance.rs`), not from documentation. The earlier
description in chat ("first match wins") was wrong for this engine. First
match wins applies only to `AclPolicy` in the resource tree.

1. **Rate limit.** This happens outside the rule function: a `Deny` before
   any rule is checked.
2. **Magnitude.** `T = effect.score() > risk_threshold`. This is one bit per
   engine threshold.
3. **Each active rule, in insertion order:**
   - `selectors_match`: an action selector and a tool selector (exact, or a
     prefix when it ends in `*`), combined with AND. The tool selector reads
     `context["tool"]`, falling back to `context["gate_action"]`.
   - `BrowserPolicy`: a predicate on `platform == "browser"`, action and topic
     (`apply_browser_policy_rule`).
   - `BindingThread`: a predicate on the binding-thread context
     (`apply_binding_thread_rule`).
   - `General`: applies if `T ∨ force_on_match`. Severity and the ADR-103 D8
     overlay tag set the flags `has_blocking`, `hard_deny`, `needs_approval`
     and `other_blocking`.
4. **Decision** from the flags:
   - `has_blocking` and not `hard_deny`, with
     `human_approval_required ∨ (needs_approval ∧ ¬other_blocking)`:
     `EscalateToHuman`.
   - `has_blocking` otherwise: `Deny`.
   - `T ∧ has_warning`: `PermitWithWarning`.
   - Otherwise: `Permit`.

Apart from the browser-policy reset described below, the flags only go from
false to true. So the decision is meant to be an order-independent Boolean
function of these inputs:

| Variable | Source | Count |
|---|---|---|
| `s_k`: request matches selector *k* | distinct action and tool selectors in the active rules | one per distinct selector |
| `T`: magnitude over threshold | `effect.score()` vs `risk_threshold` | 1 per threshold |
| `b_j`: browser predicate *j* fires | `apply_browser_policy_rule` per rule | one per browser rule |
| `t_j`: binding-thread predicate *j* fires | `apply_binding_thread_rule` | one per binding rule |

`human_approval_required` is engine configuration. It is a constant per
compiled function, not an input. The default genesis set has 3 browser rules,
1 binding rule and the spawn-approval rule, so a realistic rule set is well
under 20 variables. A 2²⁰-bit truth table is 128 KiB, which is fine for a
lint.

### Don't-cares come from selector structure

Selectors are not independent. The ones that matter:

- `tool.agent_*` matching implies `tool.*` matches.
- Two different exact selectors cannot both match the same action.
- Browser predicates are false unless `platform == "browser"`.

Every combination ruled out by these implications is a don't-care. That is
the part of `U` outside `L`, and it is why ISOP terms come out shorter than
the source rules.

### Multi-valued output

The decision has four values. Compile one indicator function per value
(`Deny`, `Escalate`, `Warn`; `Permit` is the rest), each with the same
don't-care set, and run ISOP on each. The three covers together are the
compiled rule table.

## What it buys us

### 1. Dead and shadowed rule lint

Compile the full rule set to `F`. For each rule *r*, compile the set without
it to `F_{-r}`. If `F_{-r} = F`, then *r* never changes a decision. It may be
dead (its selectors can't match) or shadowed (a stronger rule always fires
with it).

Today nothing tells an operator this. The ISOP of `F` is the readable
counterpart: the shortest flat statement of what the rule set actually does.

### 2. Cross-node equality after an ADR-092 merge

ADR-092 merges by last-writer-wins on `(version, updated_at, origin_node)`.
Version equality does not prove decision equality. One node can hold an extra
local overlay (ADR-103 D8), or a different `risk_threshold`.

Compile each node's merged set. If the functions are equal, the nodes decide
identically. If they differ, `F_a ⊕ F_b` is exactly the set of requests they
disagree on, and its ISOP describes that set in a few terms. This is a
precise drift alarm, and it fits ADR-092's escalation path. Its
`EscalationOutcome::Forward` currently fires on version alone.

### 3. Rule tables for leaf devices

The covers are flat lists of cubes over selector and predicate bits. A leaf
device (edge pad, `weft-cog-host`) can check them by matching strings
against the selector table and then ANDing bits. It needs no
`GovernanceEngine`, effect scorer or serde rule model.

This depends on the ADR-092 gossip loop being live. The tables would be
gossiped alongside the source rules. They would not replace the source
rules, because the chain anchors the source.

### 4. Minimal explanations

Each cube in the `Deny` cover is a prime implicant. The cube that matched a
request is the shortest sufficient reason for the decision. For example:
`selector(tool.agent_spawn) ∧ ¬approval-overlay`, where today the operator
gets the reason text of the last matching rule.

The CoS and the audit trail could show the cube. The rule IDs would stay in
`evaluated_rules` for provenance.

### 5. Possibly: narrower capability tokens (ADR-066)

Take the permissions to grant as `L` and what the issuer can tolerate as `U`.
ISOP then gives the smallest set of caveats. This is speculative, and it is
not part of the spike.

## What it does not do

- **Numbers stay outside.** The effect score (ADR-034) is reduced to the
  threshold bit `T`. ISOP says nothing about scoring.
- **It needs an exact model of the engine.** The compiler must reproduce
  `evaluate()` exactly. The spike's first acceptance test is exhaustive
  agreement between the compiled function and `evaluate()` over every
  assignment, including the rule-order permutations.
- **AclPolicy has its own semantics.** It is first match from the leaf
  upward, which is ordered. Its function is still finite, but it needs a
  separate compiler. It is not in the first spike.
- **Reason text is not covered.** `blocking_reason` is last-assignment-wins
  and depends on order. The compiled function covers the decision only. The
  explanation work (item 4) is what would replace the reason text.

## Findings

These came from reading the code for this note.

1. **Rule order can clear a block.** In the `BrowserPolicy` branch, a match at
   a severity other than `Blocking`/`Critical` sets `has_blocking = false`.
   That also clears a block set by any earlier rule. A blocking rule followed
   by a matching Warning-severity browser rule can therefore turn a `Deny`
   into a permit. In the reverse order, the request is denied.
   Probe test: `isop_probe_warning_browser_rule_does_not_clear_earlier_block`.
   **Confirmed by test**; see the [probe result](#probe-result).
2. **Reason-text comments don't match the code.** The browser branch says
   "the first denial reason is retained". The binding-thread branch says its
   canonical reason wins "even if a later magnitude rule would overwrite".
   But every blocking match, including later General rules, assigns
   `blocking_reason`. The last match wins. This was found by reading the
   code and has not been tested.
3. **`exo_resource_tree::check_permission` is still the K0 stub.** It always
   returns `Allow` (`permission.rs:73`) and is re-exported from `lib.rs`. A
   search of `crates/` on 2026-10-19 found no caller other than the
   re-export. New code uses `CapabilityChecker::check_permission`. The stub
   can be removed, so that nothing new binds to it.
4. **Module docs name a feature that doesn't exist.** The `governance.rs`
   module docs say the CGR engine needs "the `governance` or `ruvector-apps`
   feature", and that without them `evaluate()` always permits.
   `crates/clawft-kernel/Cargo.toml` defines neither feature, and `evaluate()`
   is not gated. The docs are out of date. The behaviour is the safer of the
   two readings.

### Probe result

**Confirmed 2026-10-19** on `target-0.8.4` (`2613b6360`) with
`scripts/build.sh test clawft-kernel --filter isop_probe`.

Setup: `GovernanceEngine::new(0.7, false)` with two rules, then one request.
- Rules: `spawn_requires_approval()` (Blocking, `force_on_match`) and a
  `BP-002` browser rule with its severity set to Warning.
- Request: action `tool.agent_spawn`, `tool=agent_spawn`,
  `platform=browser`.

| Rule order | Decision |
|---|---|
| Warning browser rule, then spawn-approval | `Deny` |
| Spawn-approval, then Warning browser rule | **`Permit`** |

The default genesis browser rules are all `Blocking`, so the default set
can't trigger this. Any rule set that contains a non-blocking browser rule
can. That includes operator rules, ADR-103 overlays and rules gossiped under
ADR-092, where insertion order depends on merge order. The test is attached
to card G0 and is not committed, because it fails until the fix lands.

## Sources

- S. Minato, "Fast Generation of Prime-Irredundant Covers from Binary
  Decision Diagrams", IEICE Trans. Fundamentals E76-A(6), 1993 —
  <https://eprints.lib.hokudai.ac.jp/repo/huscap/all/47468/59_IEICE76_967.pdf>
- Hokkaido University record —
  <https://eprints.lib.hokudai.ac.jp/dspace/handle/2115/47468>
- Spot `minato_isop` —
  <https://spot.lre.epita.fr/doxygen/classspot_1_1minato__isop.html>
- Worst-case ISOP analysis (LIRMM) —
  <https://hal-lirmm.ccsd.cnrs.fr/lirmm-01712044v1/document>
- Minato & De Micheli, ICCAD '98 (ISOP for disjunctive decomposition) —
  <https://cecs.uci.edu/~papers/compendium94-03/papers/1998/iccad98/pdffiles/02b_2.pdf>
