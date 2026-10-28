# How a project gets cogs: moved to WeftOS/weftos-cogs

The operator how-to for cog sources (the `weftos`, `cognitum` and `private` source kinds), licences and
private repositories is part of the Cogs product. Its documentation lives in the Cogs repository:

- [docs/cogs/cog-sources.md at weftos-cogs v0.1.4](https://github.com/WeftOS/weftos-cogs/blob/v0.1.4/docs/cogs/cog-sources.md)
- Successor decision: [COG-105](https://github.com/WeftOS/weftos-cogs/blob/v0.1.4/docs/decisions/COG-105-cog-sources.md) (cog sources)

The last WeftOS copy of this page is in the `v0.8.3` tag. WeftOS keeps the design record,
[ADR-105](../adr/adr-105-cog-sources.md), and the commands that read the source list: the
`weaver cog` group and `weaver workload catalog --kind cog` (see the
[CLI reference](../reference/cli.md)). The licence side that WeftOS implements is described in
[weft-licence.md](weft-licence.md) and [ADR-106](../adr/adr-106-seed-licence-proxy.md).
