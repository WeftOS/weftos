# SPARC Pseudocode — weftos.weavelogic.ai Improvements

## Phase: P (Pseudocode)

### Task Decomposition & Sequencing

Tasks are ordered by dependency and impact. Each task has clear inputs, outputs, and acceptance criteria.

---

## Sprint 1: Foundation (Landing Page + Branding) — Est. 4-6 hours

### Task 1.1: Fix Metadata & Title
```
INPUT: docs/src/app/layout.tsx
CHANGE:
  - title.default: "clawft" → "WeftOS"
  - description: update to benefit-driven copy
  - Add Open Graph meta tags (title, description, image)
OUTPUT: Correct <title> and meta tags across all pages
ACCEPTANCE: document.title === "WeftOS" on landing page
```

### Task 1.2: Rewrite Landing Page Hero
```
INPUT: docs/src/app/page.tsx (120 lines)
CHANGE:
  - Replace H1 subtitle: "Full-stack AI operating system built in Rust"
    → "The AI framework that remembers everything."
  - Replace description paragraph with benefit-driven copy
  - Add primary CTA button: "Get Started in 5 Minutes" → /docs/clawft/getting-started
  - Add secondary CTA: "View on GitHub" with dynamic star count
  - Move shields.io badges to a "Status" section below the fold
OUTPUT: Benefit-driven hero with clear CTAs
ACCEPTANCE: 5-second test passes (visitor understands value prop)
```

### Task 1.3: Rewrite Layer Cards
```
INPUT: docs/src/app/page.tsx — layer cards section
CHANGE:
  - Card 1: "7-stage processing pipeline..." → "Connect to any LLM, deploy on any channel..."
  - Card 2: "Process management with PID tracking..." → "Every agent gets a PID, audit trail, governance..."
  - Card 3: "Causal knowledge graph, HNSW..." → "A knowledge graph that grows with every interaction..."
  - Add hover states and "Learn more →" text to indicate clickability
OUTPUT: Benefit-driven cards that communicate value
ACCEPTANCE: No acronyms visible on cards (ECC, HNSW, DEMOCRITUS)
```

### Task 1.4: Add Footer
```
INPUT: docs/src/app/layout.tsx or page.tsx
CHANGE:
  - Add footer component with:
    - GitHub repository link
    - License (MIT/Apache-2.0)
    - "By WeaveLogic" with link to weavelogic.ai
    - "Need enterprise support?" → weavelogic.ai/contact
    - Community link (GitHub Discussions or Discord if available)
OUTPUT: Professional footer on all pages
ACCEPTANCE: Footer renders on landing page and doc pages
```

### Task 1.5: Add Feature Highlights Section
```
INPUT: docs/src/app/page.tsx — feature cards section
CHANGE:
  - Replace abstract labels with benefit-driven descriptions
  - "Agent Loop" → "Run Anywhere" / "7-stage pipeline, 9 providers, native + WASM + Docker"
  - "Governance" → "Trust Your Agents" / "Constitutional AI that stops misbehavior before it happens"
  - "Provenance" → "Prove Every Decision" / "Tamper-evident audit trail for every agent action"
  - "Mesh" → "Scale Across Machines" / "Encrypted P2P coordination without central servers"
OUTPUT: Feature section that communicates practical benefits
ACCEPTANCE: No jargon visible ("effect vectors", "ExoChain")
```

---

## Sprint 2: Docs Structure (Index + Navigation) — Est. 3-4 hours

### Task 2.1: Create /docs Index Page
```
INPUT: Create docs/src/content/docs/index.mdx (or equivalent Fumadocs route)
CHANGE:
  - Title: "WeftOS Documentation"
  - One-paragraph explanation of WeftOS and its layers
  - "Start Here" section with links:
    - Quickstart (getting-started)
    - Architecture Overview
    - Installation Options
  - "By Topic" section:
    - "Build agents" → /docs/clawft
    - "Deploy infrastructure" → /docs/weftos
    - "Add cognition" → /docs/weftos/ecc
  - Search bar
OUTPUT: /docs returns a useful page instead of 404
ACCEPTANCE: HTTP 200 on /docs with navigation links
```

### Task 2.2: Add Previous/Next Navigation
```
INPUT: Fumadocs configuration (source.config.ts or layout config)
CHANGE:
  - Enable previousPage/nextPage in Fumadocs page layout
  - Define page ordering in meta.json files
OUTPUT: Previous/Next buttons at bottom of every doc page
ACCEPTANCE: Every doc page has navigation to adjacent pages
```

### Task 2.3: Add "Edit this page on GitHub" Links
```
INPUT: Fumadocs layout configuration
CHANGE:
  - Set editOnGithub.repo = "WeftOS/weftos"
  - Set editOnGithub.dir = "docs/src/content/docs"
  - Set editOnGithub.branch = "master"
OUTPUT: "Edit on GitHub" link on every doc page
ACCEPTANCE: Link resolves to correct file on GitHub
```

### Task 2.4: Create Glossary Page
```
INPUT: Create docs/src/content/docs/glossary.mdx
CHANGE:
  - Define all project-specific terms:
    - clawft, WeftOS, ECC, ExoChain, DEMOCRITUS
    - HNSW, SHAKE-256, BLAKE3
    - Symposium, Causal DAG, Effect Vectors
    - Constitutional Governance, Provenance Chain
  - Each entry: one-sentence definition + link to relevant doc page
OUTPUT: /docs/glossary with all terms defined
ACCEPTANCE: Every acronym on the site has a glossary entry
```

---

## Sprint 3: Quickstart Overhaul — Est. 3-4 hours

### Task 3.1: Three-Tier Quickstart
```
INPUT: docs/src/content/docs/clawft/getting-started.mdx
CHANGE:
  - Restructure into three paths:
    Path 1 — "Try it" (30 seconds):
      docker run --rm -it ghcr.io/weftos/weftos:0.3.1 weft --help
      Show expected output
    Path 2 — "Build something" (5 minutes):
      curl install → set API key → weft agent -m "Hello"
      Show expected output for each step
      10-line example with explanation
    Path 3 — "Go deep" (30 minutes):
      Full config file walkthrough
      Multi-provider setup
      Custom tool creation
  - Add prerequisites section at top
  - Add troubleshooting section at bottom
OUTPUT: Three clear paths to getting started
ACCEPTANCE: Path 1 works in < 60 seconds on a clean machine with Docker
```

### Task 3.2: Add Expected Output Blocks
```
INPUT: All getting-started and quickstart MDX files
CHANGE:
  - After every command example, add a "You should see:" block
  - Use a visually distinct style (gray background or different code block type)
OUTPUT: Every command has its expected output shown
ACCEPTANCE: No command example exists without a corresponding output block
```

---

## Sprint 4: Content Improvements — Est. 4-6 hours

### Task 4.1: Add Use Case Examples
```
INPUT: Create docs/src/content/docs/clawft/examples/ directory
CHANGE:
  - Create 3-4 example pages:
    1. devops-agent.mdx — DevOps automation with governance
    2. code-review-agent.mdx — Code review with audit trail
    3. research-swarm.mdx — Multi-agent research with mesh
    4. compliance-bot.mdx — Constitutional governance in action
  - Each example: problem statement, 20-line config, expected output
OUTPUT: /docs/clawft/examples/* with working examples
ACCEPTANCE: Each example can be copy-pasted and run
```

### Task 4.2: Add Performance Benchmarks Section
```
INPUT: Create docs/src/content/docs/weftos/benchmarks.mdx
CHANGE:
  - Agent spawn time (ms)
  - Memory footprint per agent vs Python frameworks
  - Binary size comparison
  - WASM cold-start latency
  - Run actual benchmarks using scripts/build.sh or cargo bench
  - Present as terminal output blocks (Tailwind style)
OUTPUT: /docs/weftos/benchmarks with real numbers
ACCEPTANCE: All numbers are from actual measurements, not estimates
```

### Task 4.3: Improve Security Page
```
INPUT: docs/src/content/docs/clawft/security.mdx
CHANGE:
  - Add compliance mapping section (SOC2, HIPAA, GDPR alignment)
  - Add note: "designed to support" compliance requirements
  - Add simplified threat model (attack surfaces + mitigations)
OUTPUT: Enterprise-friendly security page
ACCEPTANCE: Page addresses compliance concerns without making false claims
```

---

## Sprint 5: Polish & Cross-Site — Est. 2-3 hours

### Task 5.1: Brand Continuity
```
INPUT: docs/src/app/layout.tsx, page.tsx, global.css
CHANGE:
  - Add "By WeaveLogic" link in header
  - Add "Need enterprise support?" banner or footer CTA
  - Align primary brand color with weavelogic.ai
  - Ensure dark/light mode both work
OUTPUT: Visual and navigational connection to weavelogic.ai
ACCEPTANCE: User can navigate to weavelogic.ai from any page
```

### Task 5.2: Accessibility Fixes
```
INPUT: docs/src/app/layout.tsx, page.tsx
CHANGE:
  - Add skip-to-content link as first focusable element
  - Fix heading hierarchy (H1 > H2 > H3 strict order)
  - Add aria-labels to code blocks
  - Add copy confirmation toast on code block copy
OUTPUT: WCAG 2.1 AA baseline compliance
ACCEPTANCE: No heading hierarchy violations, skip link works
```

### Task 5.3: Add Social Proof
```
INPUT: docs/src/app/page.tsx
CHANGE:
  - Replace static badges with dynamic ones where possible
  - Add GitHub stars count (API or shields.io dynamic badge)
  - Add crates.io total downloads badge
  - Add "Built on WeftOS" section if any external users exist
OUTPUT: Social proof signals on landing page
ACCEPTANCE: At least 3 dynamic proof points visible
```

---

## Execution Order

```
Sprint 1 (Foundation)     → Can ship independently, highest impact
Sprint 2 (Docs Structure) → Unblocks Sprint 3
Sprint 3 (Quickstart)     → Depends on Sprint 2 for /docs index
Sprint 4 (Content)        → Independent of Sprints 1-3
Sprint 5 (Polish)         → Final pass after content is stable
```

## Total Estimated Effort: 16-23 hours across 5 sprints
