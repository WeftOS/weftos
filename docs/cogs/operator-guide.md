# Operating cogs: moved to WeftOS/weftos-cogs

The step-by-step procedure for taking a cog from a source to a running, governed workload (pack, sign,
pair, place, revoke, redistribute) is part of the Cogs product. Its documentation lives in the Cogs
repository:

- [docs/cogs/operator-guide.md at weftos-cogs v0.1.4](https://github.com/WeftOS/weftos-cogs/blob/v0.1.4/docs/cogs/operator-guide.md)

The last WeftOS copy of this page is in the `v0.8.3` tag. WeftOS keeps the design and the commands the
guide drives: [ADR-099](../adr/adr-099-governed-workload-placement.md) (placement and trust),
[ADR-100](../adr/adr-100-cog-workload-kind.md) (the cog kind), and the `weaver workload` and
`weaver cog` groups in the [CLI reference](../reference/cli.md). The ARM test lane is
[test-pi.md](test-pi.md), and the conformance harness is [conformance-harness.md](conformance-harness.md).
