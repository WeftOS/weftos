# SPARC Architecture — weftos.weavelogic.ai Improvements

## Phase: A (Architecture)

### 1. System Context

```
┌──────────────────────────────────────────────────────────┐
│                    Vercel (CDN + Edge)                     │
│                                                           │
│  ┌─────────────────────┐    ┌──────────────────────────┐ │
│  │  weftos.weavelogic.ai│    │   weavelogic.ai          │ │
│  │  (Next.js 16 / SSG) │◄──►│   (Next.js / SSR)        │ │
│  │  Root: docs/src/     │    │   Separate repo           │ │
│  │  Project: clawft     │    │   Project: weavelogic.ai  │ │
│  └────────┬────────────┘    └──────────────────────────┘ │
│           │                                               │
│           │ Git push triggers                             │
│           ▼                                               │
│  ┌─────────────────────┐                                 │
│  │  GitHub: weave-      │                                 │
│  │  logic-ai/weftos     │                                 │
│  │  (master branch)     │                                 │
│  └─────────────────────┘                                 │
└──────────────────────────────────────────────────────────┘
```

### 2. File Architecture

All changes are within `docs/src/` in the clawft repo.

```
docs/src/
├── app/
│   ├── layout.tsx          ← FIX: title, meta, footer, brand header
│   ├── page.tsx            ← REWRITE: hero, cards, CTAs, footer
│   ├── global.css          ← EXTEND: custom brand tokens
│   └── docs/
│       └── [[...slug]]/
│           └── page.tsx    ← Fumadocs routing (no changes needed)
├── content/
│   └── docs/
│       ├── meta.json       ← UPDATE: add docs index, glossary to nav
│       ├── index.mdx       ← NEW: docs landing page
│       ├── glossary.mdx    ← NEW: term definitions
│       ├── clawft/
│       │   ├── getting-started.mdx  ← REWRITE: three-tier quickstart
│       │   ├── security.mdx        ← EXTEND: compliance mapping
│       │   └── examples/           ← NEW: use case examples
│       │       ├── meta.json
│       │       ├── devops-agent.mdx
│       │       ├── code-review-agent.mdx
│       │       ├── research-swarm.mdx
│       │       └── compliance-bot.mdx
│       └── weftos/
│           ├── benchmarks.mdx      ← NEW: performance data
│           └── (existing 52 pages, no changes)
├── source.config.ts        ← CHECK: edit-on-github, pagination config
├── next.config.mjs         ← DONE: turbopack.root already fixed
└── package.json            ← NO CHANGES
```

### 3. Component Architecture

#### Landing Page Structure (page.tsx rewrite)
```
<main>
  <HeroSection>
    <h1>WeftOS</h1>
    <p class="subtitle">benefit-driven tagline</p>
    <p class="description">2-sentence value prop</p>
    <CTAButtons>
      <PrimaryButton href="/docs/clawft/getting-started">Get Started</PrimaryButton>
      <SecondaryButton href="https://github.com/WeftOS/weftos">GitHub ★</SecondaryButton>
    </CTAButtons>
  </HeroSection>

  <LayerCards>  <!-- benefit-driven, no jargon -->
    <Card href="/docs/clawft" title="Run agents anywhere" />
    <Card href="/docs/weftos" title="Manage agents like processes" />
    <Card href="/docs/weftos/ecc" title="Give agents a brain" />
  </LayerCards>

  <InstallSection>  <!-- simplified, show output -->
    <CodeBlock>curl install command</CodeBlock>
    <AlternativeLinks>crates.io | Docker | npm</AlternativeLinks>
  </InstallSection>

  <StatusBadges>  <!-- moved below fold -->
    <shields.io badges />
  </StatusBadges>

  <FeatureHighlights>  <!-- benefit-driven -->
    <Feature title="Run Anywhere" />
    <Feature title="Trust Your Agents" />
    <Feature title="Prove Every Decision" />
    <Feature title="Scale Across Machines" />
  </FeatureHighlights>

  <Footer>
    <GitHubLink /> <License /> <WeaveLogicLink /> <SupportCTA />
  </Footer>
</main>
```

### 4. Fumadocs Configuration

Key Fumadocs features to enable:
- `editOnGithub`: links to source files on GitHub
- `previousPage` / `nextPage`: bottom-of-page pagination
- `toc`: table of contents (already enabled)
- `breadcrumb`: location indicator (verify enabled)

Configuration lives in `source.config.ts` and layout files.

### 5. Deployment Pipeline

```
Developer pushes to master
    ↓
Vercel detects change in docs/src/
    ↓
Vercel builds: cd docs/src && npm install && npm run build
    ↓
Next.js 16 + Turbopack SSG generates 72+ static pages
    ↓
Deploy to weftos.weavelogic.ai
    ↓
CDN invalidation (automatic)
```

No CI changes needed. Existing Vercel Git integration handles everything.

### 6. Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Keep Fumadocs Ocean theme | Yes | Professional baseline, low effort to customize |
| Custom CSS vs theme override | Theme override via CSS vars | Maintain upgradability |
| Landing page in page.tsx vs MDX | Keep as TSX | More layout control for marketing page |
| Docs index as MDX vs TSX | MDX | Consistent with other doc pages |
| Benchmarks: static vs dynamic | Static (measured once, published) | Reproducibility, no runtime dependency |
| Footer: component vs inline | Inline in layout.tsx | Simple, one location |

### 7. Testing Strategy

- `npm run build` must succeed (72+ pages generated)
- No TypeScript errors
- Manual check: landing page renders in light and dark mode
- Manual check: /docs returns 200
- Manual check: all new links resolve (no 404s)
- Verify `<title>` tag on landing page
- Verify footer renders on doc pages
