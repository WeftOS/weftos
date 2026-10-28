# Tells by surface: what reads as generated, and what to write instead

This catalogs the concrete, checkable signs that a piece of writing came from
an LLM rather than a person, broken out by the surface it appears on — because
a tell that matters in a Slack message (a "Great question!" opener) doesn't
even apply to a slide title, and a tell that matters in a README (symmetric
"Overview / Features / Conclusion" scaffolding) doesn't apply to a one-line
commit subject. Each surface gets: the lexical tells, the structural tells,
a replacement principle, and a before/after I wrote myself to show the fix.

Grounding: the external lists are Wikipedia's [Signs of AI writing](https://en.wikipedia.org/wiki/Wikipedia:Signs_of_AI_writing)
guide (used by editors to flag likely-AI edits) and two lexical-frequency
studies — Kobak et al., ["Delving into ChatGPT usage in academic writing
through excess vocabulary"](https://arxiv.org/abs/2406.07016) (arXiv:2406.07016,
found *delve* +1500%, *underscore* +1000%, *intricate* +700% in PubMed abstracts
2022→2024) and Juzek & Ward, ["Why Does ChatGPT 'Delve' So Much? Exploring the
Sources of Lexical Overrepresentation in Large Language Models"](https://aclanthology.org/2025.coling-main.426/)
(COLING 2025), which traces the overrepresentation to RLHF annotator pools
rather than pretraining data. The in-repo evidence is this project's own git
history and docs — real commits, real authors, quoted directly below, not
paraphrased.

## The severity model

Not every tell is equally bad, and a few things treated as tells are actually
just how some people write. Three tiers:

1. **Always-fix.** Reads as generated regardless of who wrote it, because it
   carries no information and exists only as a verbal tic: "Great question!",
   "Here's the thing", closing summaries that just restate what was already
   said, "Let me know if you'd like me to adjust anything!" tacked onto a
   finished deliverable, "It's not just X, it's Y" as a rhetorical reflex
   rather than a real distinction, fake enthusiasm ("I'd be happy to help
   with that!"), restating the question back before answering it.
2. **Fix unless the person's own voice profile says otherwise.** Tricolons,
   headers on a 5-line message, bullet-with-bold-label spam where prose would
   read fine, uniform sentence length, hedging stacks ("might potentially
   somewhat"). A specific person can legitimately write this way; absent a
   profile that says so, default to fixing it.
3. **Stylistic preference, not a tell.** Em dashes, Oxford comma, sentence
   fragments, contractions, occasional passive voice, starting a sentence
   with "And" or "But". These vary by person and register and should never be
   auto-corrected by a base rule — see "tells that are not reliable" below.

## a. Harness / terminal output a developer reads

This repo is an unusually good corpus for this surface because the same
person (Mathew Beane) both writes terse commits by hand and reviews long
commits that an agent drafted under `Co-Authored-By: claude-flow` or
`Co-Authored-By: Claude Fable 5` — so the contrast between his own voice and
agent output that passed his bar is directly visible in one `git log`.

His own terse style, subject-only, no body (`git log --format='%s'`):

```
Expose dashboard goals to WeftOS harness
Document client board export decision gate
Use host-local node identity for dashboard heartbeat
```

No adjectives, no "this commit," verb-first, done.

Agent-drafted commits he kept (`858c6bcb1`, real, full text in git log)
open with the mechanism, not a summary of intent:

> rvf-runtime 0.2.0's locking.rs declared `extern "C" fn __errno_location()`
> unconditionally for Unix. That is a glibc name; macOS exports `__error`, so
> anything linking rvf-runtime on macOS failed with an undefined symbol.

— and close with a "Not fixed here:" section that admits scope limits instead
of implying the change was complete:

> Not fixed here: scripts/build.sh clippy is red on this branch with 19
> pre-existing errors ... Confirmed pre-existing by reproducing them with
> these changes stashed.

That admission is the opposite of a generated-sounding tell. Generated
terminal output tends to *claim* completeness ("All tests passing, the
implementation is now robust and production-ready!") without the reproduction
step that would let a reader trust the claim.

**Top tells in this surface:**
- Lexical: "successfully", "seamlessly", "robust", "comprehensive solution",
  "production-ready" used as a vibe rather than a checked fact.
- Preamble before the point: "I've made the following changes:" followed by
  a list that was about to appear anyway.
- Closing offers on a finished, already-run task: "Let me know if you'd like
  me to run the tests!" — if you can run them, run them; report the result.
- Cheerleading punctuation: exclamation points on status lines, ✅/🎉 emoji as
  the only signal of pass/fail where a number would do.
- Silent scope-inflation: describing what was *attempted* as if it were
  *verified* ("this fixes the leak" vs. "this should fix the leak; not yet
  reproduced under load").

**Structural tells:** a wall of headers and bullets for a two-line status
update; restating the user's request back before answering it; a "Summary"
section at the end of a short reply that just repeats the reply.

**Replacement principle:** front-load the outcome and the number, state the
verification method in the same breath as the claim, and say what wasn't
done as plainly as what was.

BEFORE (generated-sounding):
> Great, I've successfully implemented the fix! I made the following changes
> to resolve the memory leak issue:
> - Fixed the buffer allocation logic
> - Added proper cleanup handling
> - Improved error handling throughout
>
> The implementation is now robust and the leak should be fully resolved.
> Let me know if you'd like me to run any additional tests!

AFTER (matches `858c6bcb1`'s actual register):
> Buffer wasn't freed on the early-return error path in `read_chunk`; added
> the `drop(buf)` there. `cargo nextest -p clawft-io`: 41/41 pass. Did not
> load-test under sustained allocation pressure — that's the scenario the
> original report came from, so treat this as fixed-in-the-obvious-place,
> not fixed-and-verified-at-scale.

## b. Markdown files / READMEs / technical docs / ADRs

The failure mode here isn't lone words, it's *templating the whole document*
regardless of what the content actually needs. This repo's own
`docs/research/ruv-ecosystem-synergy-flywheel.md` is a real, otherwise
well-informed example: every subsection ends with a bolded "**Synergy play:**"
sentence, whether or not that section's content actually resolves into a
"play." That's a repeated scaffold applied for symmetry, not because the
content called for it in every case.

**Top tells:**
- Lexical: *delve*, *robust*, *seamless*, *leverage*, *comprehensive*,
  *holistic*, *realm*, *tapestry* (as abstraction), *bespoke*, *pivotal*,
  *underscore* (verb), *showcase*, *testament to*, *paradigm shift*,
  *game-changer* — see the Wikipedia list and the Kobak/Juzek studies cited
  above for the frequency evidence behind these specific words.
- "In this document we will explore..." / "In conclusion..." bookending —
  redundant in a doc a reader can already see the length of.
- Bullet lists where every item starts with a **Bold Label:** even when the
  content is a paragraph of connected reasoning, not an enumerable set.
- A table manufactured for two rows that would read faster as a sentence.
- Symmetric headers imposed on asymmetric content — "Overview / Motivation /
  Approach / Results / Future Work" on a two-paragraph note.

**What's actually fine, and present in this same doc:** tables that hold
real multi-dimensional comparisons (rUv concept, status, WeftOS alignment —
three genuinely different axes) are not a tell; they're the right tool.

**Replacement principle:** let structure follow the content's actual shape.
If three of five sections resolve into an action, name it there; don't force
the other two into the same box. Vary the closing move per section instead
of repeating a template sentence.

BEFORE:
> ## Section 3: Memory Architecture
> WeftOS leverages a comprehensive memory architecture that seamlessly
> integrates multiple tiers.
> **Synergy play:** align the memory tapestry with upstream patterns for a
> holistic developer experience.

AFTER:
> ## Memory
> Three tiers: hot (VectorStore/HNSW), warm (COW memory crate), cold
> (AgentDB via Ruflo). Pattern keys stay compatible with Ruflo's
> `memory_store` namespace so Claude and Grok sessions can share memory
> without a second pattern store.

## c. Commit messages and PR descriptions

Already covered in depth in (a) with real examples, since in this repo the
two surfaces share one voice. One thing specific to PR descriptions: the
generated version narrates its own virtue ("This PR represents a significant
improvement to..."); the human/good-agent version states the delta and the
proof. Compare the real `a5ac89dfe` commit body's numeric before/after crate
version table against a generic alternative:

BEFORE (generic):
> This PR upgrades our dependencies to the latest versions, bringing
> significant improvements and bug fixes. This ensures our codebase stays
> up to date with the latest best practices.

AFTER (what's actually in the repo):
> rvf-runtime had no 0.2.1: upstream published 0.2.0 (2026-02-16) then 0.3.0
> (2026-06-11), so 0.2.0 was the terminal release of that line and the minor
> bump was the only path forward. ... Net effect: zero source changes to
> WeftOS.

The second one is falsifiable — a reviewer can check the version numbers and
the "zero source changes" claim. The first one is not falsifiable at all,
which is itself a tell: generated PR prose tends toward claims that can't be
checked against anything.

**Severity note specific to this surface:** a `Co-Authored-By:` trailer or
session link is not itself a tell — it's disclosure, and this repo's own
convention keeps it. The tell is in the prose above the trailer, not the
trailer's existence.

## d. Long-form drafts (proposals, reports, blog posts)

**Top tells:**
- Tricolons everywhere: "faster, safer, and more reliable" as a reflex closer
  on nearly every paragraph, regardless of whether there really are three
  parallel things to say.
- "It's not just a database — it's a platform" antithesis structure used as
  a rhetorical trick rather than a real correction of a reader's assumption.
- Rhetorical-question openers: "But what does this really mean for
  developers?"
- "Here's the thing" / "Here's why that matters" as a transition filler.
- Hedge-stacking: "it could potentially, in some cases, arguably help."
- A closing "In summary" paragraph that adds no new information and exists
  only to have a closing paragraph.

**Structural tells:** every section the same length regardless of how much
the topic actually needs; a five-part structure imposed on a two-part
argument; headers that restate the thesis instead of previewing the section.

**Replacement principle:** let paragraph length track argument complexity —
a point that takes one sentence gets one sentence. Cut the rhetorical
question opener and start with the claim. If there really are three parallel
things, say three; if there are two, say two.

BEFORE:
> It's not just a performance optimization — it's a fundamental rethinking
> of how the system handles memory. This change delivers faster, safer, and
> more maintainable code across the board. Here's why that matters: in
> today's fast-paced development landscape, robust memory management is more
> critical than ever.

AFTER:
> The old allocator copied the buffer on every read; the new one borrows it.
> That's the whole change. It cuts p99 read latency from 40ms to 6ms on the
> WEFT-577 benchmark and removes about 200 lines of manual lifetime
> bookkeeping that existed only to work around the copy.

## e. Presentations / slide decks

**Top tells:**
- Slide titles that are vague noun phrases ("Key Considerations", "Our
  Approach") instead of the one claim the slide is arguing — the opposite of
  assertion-evidence titling, which is a real, good practice this tell is
  often confused with. The fix isn't "no claims in titles," it's "a specific,
  checkable claim in the title" ("Gz bundle: 3390KB → 1576KB after font
  trim", not "Bundle Size Improvements").
- Bullets that are sentence fragments restating the title in different words
  rather than adding a new fact per bullet.
- Speaker notes that are the bullets copy-pasted into full sentences, adding
  nothing a reader couldn't infer from the slide itself.
- Exclamation points and "!" energy on data slides.
- A closing "Questions?" slide with no actual open question named on it.

**Replacement principle:** one slide, one claim, in the title; each bullet
is evidence for that claim, not a rephrasing of it; speaker notes carry the
context that isn't on the slide (why this number, what was ruled out), not a
restatement of what's already visible.

BEFORE:
> **Title: Key Considerations for Bundle Optimization**
> - Bundle size is important
> - We made several improvements
> - Results were positive
>
> *Speaker notes: On this slide we discuss how bundle size is important and
> the improvements we made were positive.*

AFTER:
> **Title: Panel WASM: 7.28MB → 4.49MB raw (gate: 4.5MB)**
> - Dropped `egui_extras/all_loaders` → `image` + `datepicker` only
> - Splash asset 636KB → 60KB (pngquant), fonts subset to Latin-only
> - Gz still ~76KB over the 1500KB long-term goal — multi-module load is
>   the next lever, not yet built
>
> *Speaker notes: the gz gap is real and we're naming it here on purpose —
> the gate was moved to 1600 to ship this wave; don't read the slide as
> "done."*

## f. Email and chat (Slack), including replies and short messages

**Top tells:**
- Openers: "I hope this email finds you well", "I wanted to reach out
  regarding...", "Just circling back on this."
- "Great question!" / "Happy to help!" as reflexive openers regardless of
  whether the question was actually notable.
- A short factual answer wrapped in unnecessary throat-clearing paragraph
  before and a "Let me know if you have any other questions!" after.
- Over-formality in a channel where the person's own history is casual, or
  the reverse — under-reading the register of a message thread that's
  clearly formal.
- Bulleted lists in a 3-sentence Slack message where the sentences would
  read faster inline.

**Replacement principle:** match the register already established in the
thread; answer the question in the first line; skip the opener and the
closer unless there's a real reason for either (e.g., you're introducing a
genuinely new ask, or there's a real follow-up you need, not a generic
offer).

BEFORE (Slack DM):
> Hi! I hope you're doing well. I wanted to reach out regarding the WEFT-577
> ticket. Great question about the bundle size! So here's the thing — we
> managed to get the raw size down to 4487 KB, which is really fantastic
> progress. Let me know if you have any questions or concerns!

AFTER:
> Raw's down to 4487KB (gate's 4500), gz is 1576 vs the 1500 goal — still
> ~76KB over. Multi-module load is the next lever if we want to close that.

## g. Social posts (LinkedIn, X)

**Top tells:**
- Hook-question openers: "Ever wondered why your bundle size keeps
  creeping up? 🧵"
- Numbered-thread scaffolding ("1/12") applied to content that doesn't need
  twelve parts.
- Emoji-as-bullet ("🚀 Faster. 🔒 Safer. 🎯 Smarter.") replacing actual bullet
  punctuation.
- Fake-contrarian openers: "Unpopular opinion:" preceding an opinion that is
  not unpopular.
- "Game-changer", "This is huge", "Here's what nobody tells you about..."
- Hashtag stacking unrelated to the platform's actual discovery mechanics.

**Replacement principle:** state the concrete result first, in one sentence,
with the number; let the platform's own audience decide if it's a big deal
instead of asserting that it is.

BEFORE:
> 🚀 Huge win this week! Ever wondered how to slash your WASM bundle size?
> Here's what nobody tells you about dependency trimming 🧵 1/8
> 🎯 Faster load times
> 🔒 Better UX
> 💪 Cleaner codebase
> #WebDev #Performance #GameChanger

AFTER:
> Trimmed our VSCode panel's WASM bundle from 7.28MB to 4.49MB raw by
> dropping unused egui_extras loaders and subsetting fonts to Latin-only.
> Gz is still ~76KB over our long-term goal — multi-module loading is next.

## Register calibration: one result, four surfaces

Same underlying fact (the WEFT-577 bundle trim), calibrated per surface,
showing that calibration — not tone-flattening to one "professional AI
voice" — is the actual skill:

- **Terminal/status:** `Panel WASM (post-opt): 4.38 MB (4594853 bytes). Raw:
  4487 KB Gzipped: 1576 KB. Budget: 4500/1600 → PASS`
- **Slack to a teammate:** "Got the panel bundle under the raw gate (4487 vs
  4500). Gz's still ~76KB over the *original* 1500 goal though — didn't want
  to just quietly move the goalpost, so I bumped the gate to 1600 and left a
  note in the doc."
- **Email to a stakeholder who doesn't read code:** "The VSCode extension's
  install size dropped by about 40% this week (from 7.3MB to 4.5MB) after we
  removed unused image-processing code and shrank the bundled fonts. There's
  a further ~5% we're leaving for a follow-up that requires restructuring how
  the panel loads its views."
- **LinkedIn:** the "AFTER" example under (g) above — concrete number, no
  hook question, no emoji bullets.

The content is identical across all four. What changes is what's assumed
known (bytes vs. percentages), what's owed (proof vs. headline), and what's
left out (the internal goalpost-move story belongs in Slack, not in a
stakeholder email or a public post).

## Tells that are NOT reliable

These are commonly flagged but should not be auto-corrected by a base rule,
because real people — including the human author of this repo's own commits
— use them constantly:

- **Em dashes.** Mathew Beane's own commit subjects use them routinely:
  "`rvf-runtime 0.2.0 → 0.3.2, ruvector 2.2.x/2.3.x`", "`— fixed upstream in
  rvf-runtime 0.3.2`". Em-dash density is a per-person feature, not a tell.
  A voice profile should be free to say "this person em-dashes constantly,
  keep it."
- **Bullet lists.** Fine when the content genuinely enumerates independent
  items (see the WEFT-577 result doc's asset table). The tell is bulleting
  *prose that isn't list-shaped*, not bullets themselves.
- **Tables.** Same logic — a table with three real, independently-varying
  columns (crate / pre-version / post-version) is the right tool. Manufacturing
  a two-row table to look structured is the tell.
- **Structured verification sections** ("Verified: ...", "Not fixed here:
  ..."). This repo's convention of naming what wasn't checked is exactly
  the opposite of generated-sounding — it's evidence of a real check having
  happened. Don't strip this thinking it looks "too formatted."
- **Passive voice, occasionally.** A single passive sentence isn't a tell;
  a document where every sentence dodges naming an actor might be.
- **Contractions or their absence, Oxford comma, sentence fragments,
  starting a sentence with "And" or "But."** All person-specific register
  choices a voice profile should be able to pin in either direction.

The rule of thumb: a tell is reliable when it appears *regardless of what
the content needs* (a template applied for its own sake, a word chosen for
its vibe rather than its meaning, an emotional register that doesn't track
the actual stakes). It's not reliable when it's a mechanical feature
(punctuation mark, list format) that plenty of real writers also use by
habit — those should be governed by the per-person voice profile, not by a
base "sounds like AI" rule.
