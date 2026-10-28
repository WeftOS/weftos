# Cog ingest bridge: moved to WeftOS/weftos-cogs

The operator documentation for the ingest bridge (how a placed cog's `POST /api/v1/store/ingest`
reaches the store owner) lives in the Cogs repository:

- [docs/cogs/ingest-bridge.md at weftos-cogs v0.1.4](https://github.com/WeftOS/weftos-cogs/blob/v0.1.4/docs/cogs/ingest-bridge.md)

The last WeftOS copy of this page is in the `v0.8.3` tag. The daemon side of the bridge is
still WeftOS code, in `crates/clawft-kernel/src/cog_ingest/` and
`crates/clawft-weave/src/cog_ingest_serve.rs`; its decision is
[ADR-100](../adr/adr-100-cog-workload-kind.md), "Decision 5 resolved (2026-10-02)". The
`weftos.cog.v1` wire types are in `crates/weftos-cog-protocol`.
