import Link from 'next/link';

export default function HomePage() {
  return (
    <main className="flex min-h-screen flex-col items-center justify-center px-4">
      <div className="mx-auto max-w-4xl text-center">
        <h1 className="mb-4 text-5xl font-bold tracking-tight text-fd-foreground sm:text-6xl">
          WeftOS
        </h1>
        <p className="mb-4 text-xl text-fd-foreground/80 sm:text-2xl">
          The AI framework that remembers everything.
        </p>
        <p className="mb-8 text-base text-fd-muted-foreground sm:text-lg max-w-2xl mx-auto">
          Build agents that persist knowledge across sessions, prove why they made every decision, and coordinate across machines — all in a single Rust runtime. Open source, production-tested with 3,900+ tests.
        </p>

        <div className="mb-12 flex flex-wrap justify-center gap-3">
          <a href="/docs/clawft/getting-started" className="rounded-md bg-fd-primary px-6 py-3 font-medium text-fd-primary-foreground hover:opacity-90 transition-opacity">
            Get Started in 5 Minutes
          </a>
          <a href="https://github.com/WeftOS/weftos" className="rounded-md border border-fd-border px-6 py-3 font-medium text-fd-foreground hover:bg-fd-accent transition-colors">
            View on GitHub
          </a>
          <a href="/clawft" className="rounded-md border border-fd-border px-6 py-3 font-medium text-fd-foreground hover:bg-fd-accent transition-colors">
            Try in Browser
          </a>
        </div>

        {/* Three-layer architecture */}
        <div className="grid gap-4 sm:grid-cols-3 mb-12">
          <Link
            href="/docs/clawft"
            className="group rounded-xl border border-fd-border bg-fd-card p-6 text-left transition-colors hover:border-fd-primary hover:bg-fd-accent"
          >
            <div className="mb-2 text-xs font-medium uppercase tracking-wider text-fd-primary">
              Layer 1 — Agent Runtime
            </div>
            <h2 className="mb-2 text-xl font-semibold text-fd-card-foreground">
              Run agents anywhere
            </h2>
            <p className="text-sm text-fd-muted-foreground">
              Connect to any LLM, deploy on any channel (Slack, Teams, web, CLI), and let agents learn new skills automatically. Native binaries, browser WASM, or Docker.
            </p>
          </Link>

          <Link
            href="/docs/weftos"
            className="group rounded-xl border border-fd-border bg-fd-card p-6 text-left transition-colors hover:border-fd-primary hover:bg-fd-accent"
          >
            <div className="mb-2 text-xs font-medium uppercase tracking-wider text-fd-primary">
              Layer 2 — Kernel
            </div>
            <h2 className="mb-2 text-xl font-semibold text-fd-card-foreground">
              Manage agents like processes
            </h2>
            <p className="text-sm text-fd-muted-foreground">
              Every agent gets a PID, a cryptographic audit trail, and governance rules. When agents fail, the supervisor restarts them. When they misbehave, constitutional checks stop them.
            </p>
          </Link>

          <Link
            href="/docs/weftos/ecc"
            className="group rounded-xl border border-fd-border bg-fd-card p-6 text-left transition-colors hover:border-fd-primary hover:bg-fd-accent"
          >
            <div className="mb-2 text-xs font-medium uppercase tracking-wider text-fd-primary">
              Layer 3 — Cognitive
            </div>
            <h2 className="mb-2 text-xl font-semibold text-fd-card-foreground">
              Give agents a brain
            </h2>
            <p className="text-sm text-fd-muted-foreground">
              A knowledge graph that grows with every interaction. Your agents remember what they learned last week, trace cause and effect, and get smarter over time.
            </p>
          </Link>
        </div>

        {/* Install CTA */}
        <div className="mb-12 rounded-xl border border-fd-border bg-fd-card p-6">
          {/* One command. Any platform. */}
          <h3 className="mb-4 text-lg font-semibold text-fd-card-foreground">Install</h3>
          <div className="mb-4 rounded-lg bg-fd-background p-4 text-left font-mono text-sm text-fd-foreground">
            <span className="text-fd-muted-foreground">$</span> curl -fsSL https://github.com/WeftOS/weftos/releases/latest/download/clawft-cli-installer.sh | sh
          </div>
          <p className="mb-4 text-sm text-fd-muted-foreground">
            Also available via Homebrew, Docker, cargo, and PowerShell.{' '}
            <a href="/docs/weftos/getting-started/installation" className="underline hover:text-fd-foreground transition-colors">See all install options</a>.
          </p>
          <div className="flex flex-wrap justify-center gap-3 text-sm">
            <a href="https://github.com/WeftOS/weftos/releases" className="rounded-md bg-fd-primary px-4 py-2 font-medium text-fd-primary-foreground hover:opacity-90 transition-opacity">
              GitHub Releases
            </a>
            <a href="https://crates.io/crates/weftos" className="rounded-md border border-fd-border px-4 py-2 font-medium text-fd-foreground hover:bg-fd-accent transition-colors">
              crates.io
            </a>
            <a href="https://www.npmjs.com/package/@weftos/core" className="rounded-md border border-fd-border px-4 py-2 font-medium text-fd-foreground hover:bg-fd-accent transition-colors">
              npm
            </a>
            <a href="https://github.com/WeftOS/weftos/pkgs/container/weftos" className="rounded-md border border-fd-border px-4 py-2 font-medium text-fd-foreground hover:bg-fd-accent transition-colors">
              Docker
            </a>
            <a href="/docs/weftos/getting-started/installation" className="rounded-md border border-fd-border px-4 py-2 font-medium text-fd-foreground hover:bg-fd-accent transition-colors">
              All install options
            </a>
          </div>
        </div>

        <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4 mb-8">
          {[
            { label: 'Run Anywhere', desc: '7 platforms — Linux, macOS, Windows, Docker, WASM, browser' },
            { label: 'Trust Your Agents', desc: 'Constitutional AI that stops misbehavior before it happens' },
            { label: 'Prove Every Decision', desc: 'Tamper-evident audit trail for every agent action' },
            { label: 'Scale Across Machines', desc: 'Encrypted P2P coordination without central servers' },
          ].map((item) => (
            <div key={item.label} className="rounded-lg border border-fd-border p-4">
              <div className="mb-1 font-semibold text-fd-foreground">{item.label}</div>
              <div className="text-sm text-fd-muted-foreground">{item.desc}</div>
            </div>
          ))}
        </div>

        <div className="mb-8 flex flex-wrap justify-center gap-2">
          <img src="https://img.shields.io/github/v/release/WeftOS/weftos?label=release&color=blue" alt="Release" />
          <img src="https://img.shields.io/crates/v/weftos?color=orange" alt="crates.io" />
          <img src="https://img.shields.io/npm/v/@weftos/core?color=red" alt="npm" />
          <img src="https://img.shields.io/badge/targets-7_platforms-green" alt="Platforms" />
          <img src="https://img.shields.io/badge/tests-3,900+-brightgreen" alt="Tests" />
        </div>

        <footer className="mt-16 border-t border-fd-border pt-8 pb-8 text-center text-sm text-fd-muted-foreground">
          <div className="flex flex-wrap justify-center gap-6">
            <a href="https://github.com/WeftOS/weftos" className="hover:text-fd-foreground transition-colors">GitHub</a>
            <a href="/docs" className="hover:text-fd-foreground transition-colors">Documentation</a>
            <a href="https://weavelogic.ai" className="hover:text-fd-foreground transition-colors">WeaveLogic</a>
            <a href="https://weavelogic.ai/contact" className="hover:text-fd-foreground transition-colors">Enterprise Support</a>
          </div>
          <p className="mt-4">MIT / Apache-2.0 — Built by <a href="https://weavelogic.ai" className="underline hover:text-fd-foreground">WeaveLogic</a></p>
        </footer>
      </div>
    </main>
  );
}
