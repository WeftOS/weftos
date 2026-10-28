# WeftOS

[![Release](https://img.shields.io/github/v/release/WeftOS/weftos)](https://github.com/WeftOS/weftos/releases)
[![crates.io](https://img.shields.io/crates/v/weftos)](https://crates.io/crates/weftos)
[![License](https://img.shields.io/crates/l/weftos)](LICENSE)

WeftOS is an agent framework plus a cognitive OS layer, written in Rust. The
framework (`clawft`) gives you LLM providers, channels, tools, skills and the
`weft` agent CLI. The OS layer is a kernel with a process table, typed IPC, an
append-only audit chain (ExoChain) and constitutional governance. On top of it
sit a machine mesh, governed workload placement, and cogs: small signed
programs that run as workloads on the mesh. The operator CLI is `weaver`.

The current release is v0.8.4.

<!-- facts:start -->
- 65 workspace crates, 40 of them published to crates.io
- 123 architecture decision records
- 29 cogs in the Cogs repository, and 795 modules in the hardware catalog
<!-- facts:end -->

## Install

Each of these installs `weft` (agent CLI) and `weaver` (operator CLI). The
[installation guide](https://weftos.weavelogic.ai/docs/weftos/getting-started/installation)
has the details and the platform list.

```sh
# Release installer (Linux and macOS). Verifies each archive's sigstore
# attestation when the GitHub CLI is installed.
curl -fsSL https://raw.githubusercontent.com/WeftOS/weftos/master/scripts/install.sh | sh

# Homebrew (one formula per package: clawft-cli = weft, clawft-weave = weaver, weftos)
brew install weave-logic-ai/tap/clawft-cli
brew install weave-logic-ai/tap/clawft-weave

# Docker (linux/amd64 and linux/arm64)
docker pull ghcr.io/weftos/weftos:latest

# crates.io: libraries and the kernel daemon. weft and weaver are not
# published there (publish = false).
cargo install weftos
```

Keep an install current with `weaver update`. It refuses any release whose
signed manifest does not verify; see [docs/deployment/install.md](docs/deployment/install.md).
Release archives for Linux, macOS and Windows are on
[GitHub Releases](https://github.com/WeftOS/weftos/releases). Rustdoc for the
published crates is on docs.rs, for example [docs.rs/clawft-kernel](https://docs.rs/clawft-kernel).

## First run

```sh
export OPENAI_API_KEY="sk-..."           # or configure another provider, or a local model

weaver kernel start                      # start the per-project kernel daemon
weaver kernel status
weft agent -m "Summarize this project"
weft agent --local -m "Summarize this project"   # no daemon: run in-process
```

The [quickstart](https://weftos.weavelogic.ai/docs/weftos/getting-started/quickstart)
walks through `weftos init`, configuration and your first project.

## Documentation

| Where | What |
|-------|------|
| [Getting started](https://weftos.weavelogic.ai/docs/weftos/getting-started) | Install, quickstart, first project |
| [Platform](https://weftos.weavelogic.ai/docs/weftos/platform) | Cogs, cog data plane, placement, exposures, licence proxy, project topology, tailnet router, chief of staff, voice, signed updates |
| [CLI reference](https://weftos.weavelogic.ai/docs/clawft/cli-reference) | Generated `weft` and `weaver` command references |
| [Kernel and architecture](https://weftos.weavelogic.ai/docs/weftos) | Boot, process table, IPC, ExoChain, governance, mesh, ECC |
| [Architecture decisions](docs/adr/README.md) | ADR index: what was decided, when, and its status |
| [Cogs repository](https://github.com/WeftOS/weftos-cogs) | The cogs, the Cog Host, the catalog and the cog product docs |
| [API docs](https://docs.rs/weftos) | Rustdoc for the published crates on docs.rs |

Repository-side references live in [docs/reference/](docs/reference/) (CLI,
config, RPC, tools, capability vocabulary). The site sources are in
`docs/src/`; run `npm ci && npm run dev` there to preview it.

## Build from source

Rust 1.95 or newer (`rust-toolchain.toml`) and Node.js 18+ for the site.
Use `scripts/build.sh` rather than raw cargo.

```sh
git clone https://github.com/WeftOS/weftos.git
cd weftos
scripts/build.sh native          # release build
scripts/build.sh native-debug    # fast iteration
scripts/build.sh test            # workspace tests
scripts/build.sh clippy          # lint, warnings as errors
scripts/build.sh gate            # full phase gate
scripts/build.sh --help          # all commands
```

Browser (`wasm32-unknown-unknown`) and WASI (`wasm32-wasip2`) targets have their
own lanes, `scripts/build.sh browser` and `scripts/build.sh wasi`. The browser
guides are in [docs/browser/](docs/browser/); see also
[ADR-083](docs/adr/adr-083-browser-wasm-support.md) and
[ADR-044](docs/adr/adr-044-wasm-wasip2-target.md).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Run `scripts/build.sh gate` before
submitting a pull request, and follow the
[kernel developer guide](docs/src/content/docs/weftos/kernel-guide.mdx).

## License

The workspace crates declare `MIT OR Apache-2.0` in `Cargo.toml`. The
repository's [LICENSE](LICENSE) file is the MIT text.

## Attribution

WeftOS builds on ideas and patterns from
[claude-code](https://github.com/anthropics/claude-code),
[claude-flow](https://github.com/ruvnet/claude-flow) and
[ruvector](https://github.com/ruvnet/ruvector). The kernel draws on microkernel
OS design (L4, seL4), capability-based security (Capsicum) and distributed
systems research (SWIM, Raft, CRDTs, Kademlia).
