# SPARC Completion — weftos.weavelogic.ai Improvements

## Phase: C (Completion)

### Execution Checklist

Each sprint has a clear "done" definition. Check off as completed.

---

## Sprint 1: Foundation (Landing Page + Branding)

### Prerequisites
- [ ] Read current `docs/src/app/layout.tsx`
- [ ] Read current `docs/src/app/page.tsx`
- [ ] Read current `docs/src/app/global.css`
- [ ] Verify `npm run build` passes before changes

### Tasks
- [ ] **1.1** Fix `<title>` in layout.tsx: "clawft" → "WeftOS"
- [ ] **1.1** Update meta description to benefit-driven copy
- [ ] **1.1** Add Open Graph meta tags
- [ ] **1.2** Rewrite hero headline to "The AI framework that remembers everything."
- [ ] **1.2** Rewrite hero description to benefit-driven copy
- [ ] **1.2** Add primary CTA button: "Get Started in 5 Minutes"
- [ ] **1.2** Add secondary CTA button: "View on GitHub"
- [ ] **1.3** Rewrite all three layer cards (benefits, not features)
- [ ] **1.3** Add "Learn more →" text to cards
- [ ] **1.4** Add footer with GitHub, license, WeaveLogic, support links
- [ ] **1.5** Rewrite feature highlights (Run Anywhere, Trust, Prove, Scale)
- [ ] **1.5** Move shields.io badges below the fold

### Verification
- [ ] `npm run build` succeeds
- [ ] Landing page renders correctly in light mode
- [ ] Landing page renders correctly in dark mode
- [ ] `<title>` tag reads "WeftOS" (not "clawft")
- [ ] No acronyms visible above the fold (ECC, HNSW, DEMOCRITUS, ExoChain)
- [ ] Both CTA buttons link to correct destinations
- [ ] Footer renders on landing page
- [ ] Push to master, verify Vercel deploy succeeds
- [ ] Check live site: https://weftos.weavelogic.ai/

---

## Sprint 2: Docs Structure

### Prerequisites
- [ ] Sprint 1 deployed and verified
- [ ] Read `docs/src/source.config.ts`
- [ ] Read `docs/src/content/docs/meta.json`
- [ ] Understand Fumadocs pagination and editOnGithub config

### Tasks
- [ ] **2.1** Create `content/docs/index.mdx` (docs landing page)
- [ ] **2.1** Update `meta.json` to include index page
- [ ] **2.2** Enable previous/next page navigation in Fumadocs config
- [ ] **2.3** Configure editOnGithub (repo, dir, branch)
- [ ] **2.4** Create `content/docs/glossary.mdx` with all term definitions

### Verification
- [ ] `npm run build` succeeds (page count increases)
- [ ] /docs returns 200 (not 404)
- [ ] /docs/glossary returns 200
- [ ] Previous/Next buttons appear at bottom of doc pages
- [ ] "Edit on GitHub" link appears and resolves correctly
- [ ] Push and verify Vercel deploy

---

## Sprint 3: Quickstart Overhaul

### Prerequisites
- [ ] Sprint 2 deployed (so /docs index links work)
- [ ] Verify Docker image works: `docker run --rm ghcr.io/weftos/weftos:0.3.1 weft --help`
- [ ] Verify curl installer works on a clean machine

### Tasks
- [ ] **3.1** Restructure getting-started.mdx into three tiers
- [ ] **3.1** Tier 1: Docker one-liner with expected output
- [ ] **3.1** Tier 2: curl install → API key → first agent with expected output
- [ ] **3.1** Tier 3: Full config walkthrough with expected output
- [ ] **3.1** Add prerequisites section at top
- [ ] **3.1** Add troubleshooting section at bottom
- [ ] **3.2** Add expected output blocks to all other command examples

### Verification
- [ ] Tier 1 works in < 60 seconds with Docker
- [ ] Tier 2 works in < 5 minutes with curl
- [ ] Every command example has an expected output block
- [ ] Prerequisites are listed before first command
- [ ] Troubleshooting covers at least 3 common errors
- [ ] `npm run build` succeeds
- [ ] Push and verify

---

## Sprint 4: Content Improvements

### Prerequisites
- [ ] Core docs structure is stable (Sprints 1-3 done)
- [ ] Run benchmarks locally to get real numbers

### Tasks
- [ ] **4.1** Create examples/devops-agent.mdx
- [ ] **4.1** Create examples/code-review-agent.mdx
- [ ] **4.1** Create examples/research-swarm.mdx
- [ ] **4.1** Create examples/compliance-bot.mdx
- [ ] **4.1** Create examples/meta.json
- [ ] **4.2** Run benchmarks, create benchmarks.mdx with real data
- [ ] **4.3** Add compliance mapping to security.mdx

### Verification
- [ ] All 4 example pages render correctly
- [ ] Example configs are valid and copy-pasteable
- [ ] Benchmark numbers are from actual measurements
- [ ] Security page has compliance section
- [ ] `npm run build` succeeds
- [ ] Push and verify

---

## Sprint 5: Polish & Cross-Site

### Prerequisites
- [ ] Content is stable (Sprints 1-4 done)

### Tasks
- [ ] **5.1** Add "By WeaveLogic" header link
- [ ] **5.1** Add "Need enterprise support?" CTA
- [ ] **5.1** Align primary brand color with weavelogic.ai
- [ ] **5.2** Add skip-to-content link
- [ ] **5.2** Fix heading hierarchy
- [ ] **5.2** Add copy confirmation toast
- [ ] **5.3** Add dynamic GitHub stars badge
- [ ] **5.3** Add crates.io downloads badge

### Verification
- [ ] Can navigate to weavelogic.ai from any docs page
- [ ] Skip-to-content link works with keyboard
- [ ] No heading hierarchy violations (H1 > H2 > H3)
- [ ] Code block copy shows confirmation
- [ ] All badges display current data
- [ ] Light and dark mode both work
- [ ] `npm run build` succeeds
- [ ] Final push and full site review

---

## Post-Completion Review

After all 5 sprints are deployed:

1. **Re-run 5-second test**: Does a visitor understand WeftOS's value in 5 seconds?
2. **Time the quickstart**: Can a new developer go from zero to running in < 5 minutes?
3. **Cross-site check**: Can a visitor navigate between weavelogic.ai and weftos.weavelogic.ai seamlessly?
4. **Competitive comparison**: Does the site hold up against LangChain/CrewAI landing pages?
5. **Messaging consistency**: Are the same terms and positioning used across both sites?

### Success Criteria (from Specification)
- [ ] 5-second comprehension: 4/5 (was 2/5)
- [ ] Time to first hello world: < 2 min (was 15+ min)
- [ ] Landing page CTAs: 2 (was 0)
- [ ] /docs returns 200 (was 404)
- [ ] HTML title: "WeftOS" (was "clawft")
- [ ] Cross-site navigation: bidirectional (was none)
- [ ] Messaging score: 7+/10 (was 3.0/10)
- [ ] UX heuristic average: 4.0/5 (was 3.5/5)
