# weft-cog-host `/hw/*` API: moved to WeftOS/weftos-cogs

The Cog Host and its hardware routes (`/hw/*`, the host token, allowed origins and the agent
command) are part of the Cogs product. Their documentation lives in the Cogs repository:

- [docs/cogs/host-hw-api.md at weftos-cogs v0.1.3](https://github.com/WeftOS/weftos-cogs/blob/v0.1.3/docs/cogs/host-hw-api.md)
- Successor decisions: [COG-106](https://github.com/WeftOS/weftos-cogs/blob/v0.1.3/docs/decisions/COG-106-host-licence.md) (host licence) and [COG-107](https://github.com/WeftOS/weftos-cogs/blob/v0.1.3/docs/decisions/COG-107-catalog-link.md) (catalog link)

The last WeftOS copy of this page is in the `v0.8.3` tag. WeftOS keeps the boundary the host
talks to: the `weftos.cog.v1` wire types in `crates/weftos-cog-protocol` and the daemon side
described in [ADR-100](../adr/adr-100-cog-workload-kind.md) and
[ADR-106](../adr/adr-106-seed-licence-proxy.md).

WeftOS still links three Cogs crates, `cog-sources`, `cog-repo` and `cog-market`, from
the tagged Cogs release (pinned by commit in the root `Cargo.toml`), for `weaver cog`,
`weaver update` and `weft-licence`. The target is for WeftOS to depend only on the cog
protocol crate.
