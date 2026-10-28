import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import Link from 'next/link';

// docs/src/public/facts.json is written by scripts/gen-facts.mjs (build.sh gen-facts)
// and read here at build time, so the numbers on this page are never typed by hand.
interface Facts {
  version: string;
  crates: number;
  published: number;
  adrs: number;
  cogs: number;
  cogs_release: string;
  catalog: { modules: number; chips: number };
  weaver_commands: number;
  weft_commands: number;
}

const facts: Facts = JSON.parse(readFileSync(join(process.cwd(), 'public', 'facts.json'), 'utf8'));

const REPO = 'https://github.com/WeftOS/weftos';
const COGS = 'https://github.com/WeftOS/weftos-cogs';

// Each status is the decision-record status stated on /docs/weftos/platform.
const features = [
  {
    href: '/docs/weftos/platform/cogs',
    name: 'Cogs',
    body: 'Small native sensor and inference programs, run as signed, governed workloads by the Cog Host.',
    status: 'Accepted',
  },
  {
    href: '/docs/weftos/platform/cogbus',
    name: 'Cogbus',
    body: 'The data plane in the node daemon that moves readings, streams and blobs between cogs.',
    status: 'Accepted, built',
  },
  {
    href: '/docs/weftos/platform/placement',
    name: 'Governed placement',
    body: 'Which node runs a workload, how the decision is gated and recorded, driven by weaver workload.',
    status: 'Accepted',
  },
  {
    href: '/docs/weftos/mesh-networking',
    name: 'Mesh',
    body: 'Encrypted peer-to-peer links over TCP, WebSocket and QUIC, with peer admission and cross-node IPC.',
    status: 'Built in the 0.8.4 daemon',
  },
  {
    href: '/docs/weftos/platform/exposures',
    name: 'Exposures',
    body: 'What a node offers to the mesh, who may use it, and a local kill switch.',
    status: 'Proposed, partly built',
  },
  {
    href: '/docs/weftos/platform/tailnet-router',
    name: 'Router',
    body: 'One HTTPS front door per development machine, routing by path prefix to each project.',
    status: 'Proposed, partly built',
  },
  {
    href: '/docs/weftos/platform/chief-of-staff',
    name: 'Chief of staff',
    body: 'An agent host with a loopback conversation API, models, voice and an MCP server.',
    status: 'Proposed, partly built',
  },
  {
    href: '/docs/weftos/platform/ai-org',
    name: 'AI org',
    body: 'A versioned agent catalog and agent teams you apply to a project.',
    status: 'Accepted',
  },
  {
    href: '/docs/weftos/platform/licence-proxy',
    name: 'Licence',
    body: 'One Cognitum Seed acts as the licence proxy for one mesh and grants cog checkouts.',
    status: 'Proposed, partly built',
  },
  {
    href: '/docs/weftos/platform/voice',
    name: 'Voice',
    body: 'Echo cancellation, speech to text, the agent brain and text to speech, with a local fallback.',
    status: 'Accepted, one record Proposed',
  },
  {
    href: '/docs/weftos/platform/update-and-signing',
    name: 'Update and signing',
    body: 'weaver update installs only releases whose signed hash list verifies, and fails closed.',
    status: 'Shipped',
  },
];

const installs = [
  {
    label: 'Release installer, Linux and macOS',
    cmd: 'curl -fsSL https://raw.githubusercontent.com/WeftOS/weftos/master/scripts/install.sh | sh',
  },
  { label: 'Homebrew', cmd: 'brew install weave-logic-ai/tap/clawft-cli weave-logic-ai/tap/clawft-weave' },
  { label: 'Docker', cmd: 'docker pull ghcr.io/weftos/weftos:latest' },
  { label: 'crates.io, the kernel daemon', cmd: 'cargo install weftos' },
];

const links = [
  { href: REPO, name: 'WeftOS on GitHub', note: 'The OS and the agent framework' },
  { href: COGS, name: 'Cogs repository', note: `Signed cogs, release ${facts.cogs_release}` },
  { href: '/docs/clawft/cli-reference', name: 'CLI reference', note: 'Every weaver and weft command' },
  { href: `${REPO}/blob/master/docs/adr/README.md`, name: 'Decision records', note: `${facts.adrs} ADRs` },
];

const focus =
  'focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-fd-primary';

export default function HomePage() {
  const figures = [
    { value: facts.crates, label: 'workspace crates' },
    { value: facts.published, label: 'published on crates.io' },
    { value: facts.adrs, label: 'decision records' },
    { value: facts.cogs, label: 'cogs' },
    { value: facts.catalog.modules, label: 'modules in the hardware catalog' },
    { value: facts.weaver_commands, label: 'weaver commands' },
  ];

  return (
    <>
      <a
        href="#main"
        className="sr-only focus:not-sr-only focus:fixed focus:left-4 focus:top-4 focus:z-50 focus:rounded-md focus:bg-fd-primary focus:px-4 focus:py-2 focus:text-fd-primary-foreground"
      >
        Skip to content
      </a>

      <header className="mx-auto flex max-w-6xl items-center justify-between gap-4 px-4 py-5 sm:px-8">
        <Link href="/" className={`text-lg font-semibold tracking-tight text-fd-foreground ${focus}`}>
          WeftOS
        </Link>
        <nav aria-label="Primary" className="flex items-center gap-1 text-sm sm:gap-3">
          {[
            ['/docs/weftos', 'Docs'],
            ['/docs/weftos/platform', 'Platform'],
            ['/docs/clawft/cli-reference', 'CLI'],
            [REPO, 'GitHub'],
          ].map(([href, text]) => (
            <a
              key={href}
              href={href}
              className={`rounded-md px-2 py-2 text-fd-muted-foreground transition-colors hover:text-fd-foreground sm:px-3 ${focus}`}
            >
              {text}
            </a>
          ))}
        </nav>
      </header>

      <main id="main" className="mx-auto max-w-6xl px-4 pb-24 sm:px-8">
        {/* Hero */}
        <section className="pb-20 pt-12 sm:pt-20 lg:pb-32">
          <h1 className="max-w-5xl text-balance text-4xl font-semibold leading-[1.05] tracking-tight text-fd-foreground sm:text-6xl lg:text-7xl">
            Run AI agents like processes, on a mesh you govern
          </h1>
          <div className="mt-10 grid gap-10 lg:grid-cols-2 lg:items-start lg:gap-16">
          <div className="min-w-0">
            <p className="max-w-xl text-lg leading-relaxed text-fd-foreground">
              WeftOS is an agent framework plus a cognitive OS layer, written in Rust. A kernel with a process
              table, typed IPC, an append-only audit chain and constitutional governance, with a machine mesh,
              governed workload placement and signed cogs on top.
            </p>
            <div className="mt-8 flex flex-wrap gap-3">
              <Link
                href="/docs/weftos/getting-started/installation"
                className={`rounded-md bg-fd-primary px-6 py-3 font-medium text-fd-primary-foreground transition-opacity hover:opacity-90 active:scale-[0.98] ${focus}`}
              >
                Install WeftOS {facts.version}
              </Link>
              <Link
                href="/docs/weftos/platform"
                className={`rounded-md bg-fd-secondary px-6 py-3 font-medium text-fd-foreground transition-colors hover:bg-fd-accent active:scale-[0.98] ${focus}`}
              >
                What is in 0.8
              </Link>
            </div>
          </div>

          <div id="install" className="min-w-0 rounded-lg bg-fd-secondary p-5 sm:p-6">
            <h2 className="text-xl font-semibold text-fd-foreground">Install</h2>
            <p className="mt-1 text-sm text-fd-muted-foreground">
              Each route installs weft, the agent CLI, and weaver, the operator CLI.
            </p>
            <dl className="mt-5 space-y-4">
              {installs.map((i) => (
                <div key={i.label}>
                  <dt className="text-sm text-fd-foreground">{i.label}</dt>
                  <dd className="mt-1">
                    <pre
                      tabIndex={0}
                      className={`overflow-x-auto rounded-md bg-fd-background p-3 text-[13px] leading-relaxed text-fd-foreground ${focus}`}
                    >
                      <code>{i.cmd}</code>
                    </pre>
                  </dd>
                </div>
              ))}
            </dl>
          </div>
          </div>
        </section>

        {/* Facts, read from facts.json */}
        <section aria-labelledby="facts-h" className="pb-20 lg:pb-32">
          <h2 id="facts-h" className="max-w-2xl text-3xl font-semibold tracking-tight text-fd-foreground sm:text-4xl">
            What is in release {facts.version}
          </h2>
          <ul className="mt-10 grid grid-cols-2 gap-x-6 gap-y-10 sm:grid-cols-3 lg:grid-cols-6">
            {figures.map((f) => (
              <li key={f.label}>
                <div className="text-5xl font-semibold tabular-nums tracking-tight text-fd-primary sm:text-6xl">
                  {f.value}
                </div>
                <div className="mt-2 text-sm text-fd-foreground">{f.label}</div>
              </li>
            ))}
          </ul>
        </section>

        {/* Platform features */}
        <section aria-labelledby="platform-h" className="pb-20 lg:pb-32">
          <h2 id="platform-h" className="max-w-2xl text-3xl font-semibold tracking-tight text-fd-foreground sm:text-4xl">
            The platform, with each part&apos;s real status
          </h2>
          <p className="mt-4 max-w-2xl text-base leading-relaxed text-fd-foreground">
            Each page states the status of its decision record and separates what is built from what is only
            planned. Proposed means the decision is not yet accepted.
          </p>
          <ul className="mt-10 grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
            {features.map((f, i) => (
              <li key={f.href} className={i === 0 ? 'lg:col-span-2' : undefined}>
                <Link
                  href={f.href}
                  className={`group flex h-full flex-col rounded-lg bg-fd-secondary p-6 transition-colors hover:bg-fd-accent active:scale-[0.99] ${focus}`}
                >
                  <span className="text-xl font-semibold text-fd-foreground">{f.name}</span>
                  <span className="mt-2 text-base leading-relaxed text-fd-foreground">{f.body}</span>
                  <span className="mt-auto pt-5 text-sm text-fd-primary">{f.status}</span>
                </Link>
              </li>
            ))}
          </ul>
        </section>

        {/* Links */}
        <section aria-labelledby="links-h">
          <h2 id="links-h" className="max-w-2xl text-3xl font-semibold tracking-tight text-fd-foreground sm:text-4xl">
            Read the source and the record
          </h2>
          <ul className="mt-10 grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
            {links.map((l) => (
              <li key={l.href}>
                <a
                  href={l.href}
                  className={`flex h-full flex-col rounded-lg bg-fd-primary p-6 text-fd-primary-foreground transition-opacity hover:opacity-90 active:scale-[0.99] ${focus}`}
                >
                  <span className="text-lg font-semibold">{l.name}</span>
                  <span className="mt-1 text-sm">{l.note}</span>
                </a>
              </li>
            ))}
          </ul>
        </section>
      </main>

      <footer className="bg-fd-secondary">
        <div className="mx-auto flex max-w-6xl flex-col gap-6 px-4 py-12 sm:px-8 md:flex-row md:items-end md:justify-between">
          <div className="text-5xl font-semibold tracking-tight text-fd-foreground sm:text-7xl">WeftOS</div>
          <div className="text-sm text-fd-foreground">
            <p>MIT or Apache-2.0. Built by <a className={`underline ${focus}`} href="https://weavelogic.ai">WeaveLogic</a>.</p>
            <p className="mt-2">
              <a className={`underline ${focus}`} href={`${REPO}/releases`}>Releases</a>
              {' · '}
              <a className={`underline ${focus}`} href="https://crates.io/crates/weftos">crates.io</a>
              {' · '}
              <a className={`underline ${focus}`} href="/docs">All documentation</a>
            </p>
          </div>
        </div>
      </footer>
    </>
  );
}
