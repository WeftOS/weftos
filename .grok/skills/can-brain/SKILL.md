---
name: can-brain
description: |
  Graded knowledge base for the CAN family and the WeftOS buses that use it.
  Read the references before answering. Cite the section. Grade every claim
  documented, inferred, or unknown. The Desktop CAN-STANDARDS draft is a
  proposal layer, not an accepted map.
  Use when: a frame, profile, PGN, transceiver, bitrate, DBC, TWAI, FDCAN, or
  WeftOS sensor-on-CAN question needs a sourced answer.
  NOT for: pasting paywalled clause text; inventing scales; accepting the draft.
argument-hint: "[question]"
allowed-tools: Read, Grep, Glob, Bash, WebSearch, WebFetch
---

# CAN brain

Answer from `references/`, not from memory.

## Read order

1. `references/00-index.md` — layers and grades.
2. The one topic file the question names.
3. `references/04-weftos.md` whenever the question mentions a cog, SenML, a
   node id, time, the Civic, VENTUNO, or an ESP32.
4. `references/05-open.md` before you declare a gap new.

## Grades

- `documented` — the reference quotes a page that was opened, or a file in this repo.
- `inferred` — a reading of two documented facts. Show both.
- `unknown` — no clause in hand. Draft a question. Do not fill in a number.

The draft's own marks mean: `[S]` the draft named a URL; `[U]` the draft left
it unverified. Re-fetch a `[S]` URL before you repeat its number in a DBC or
a cog. A `[U]` item stays `unknown` until a spec is open. Do not treat a `[U]`
line as good enough to ship.

Operator errata in `references/04-weftos.md` override a contradictory draft
cell. Recorded: IMU accel is 0.002 m/s² per bit (not 1/1024), and GNSS
latitude and longitude are 32-bit (not 16-bit).

## Output

Lead with the grade and the citation. Then the consequence for the bus.
If the caller asked for a design, separate "repo rule", "draft proposal", and
"not yet known". Do not merge those three into one recommendation.

## Reference docs

- `references/00-index.md`
- `references/01-physical.md`
- `references/02-higher-layers.md`
- `references/03-tooling.md`
- `references/04-weftos.md`
- `references/05-open.md`
