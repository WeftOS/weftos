# clawft-weave

`weaver`, the WeftOS operator CLI, and the kernel daemon it controls. Use it to
run the kernel, place governed workloads on the mesh, manage cogs, control what a
node exposes, and update to a verified release. Not published to crates.io
(`publish = false`); install the binary from
[GitHub Releases](https://github.com/WeftOS/weftos/releases) or build it.

```sh
weaver kernel start               # start the per-project kernel daemon
weaver kernel status
weaver workload catalog --kind cog
weaver expose list
weaver update --check             # verify the latest signed release, install nothing
weaver --help                     # every command
```

Top-level commands include `kernel`, `workload`, `cog`, `mesh`, `cluster`,
`expose`, `route`, `chain`, `update` and `doctor`. The generated reference is in
the [CLI reference](https://weftos.weavelogic.ai/docs/clawft/cli-reference).

Part of [WeftOS](https://github.com/WeftOS/weftos), version 0.8.4.
Platform pages: [placement](https://weftos.weavelogic.ai/docs/weftos/platform/placement),
[cogs](https://weftos.weavelogic.ai/docs/weftos/platform/cogs),
[exposures](https://weftos.weavelogic.ai/docs/weftos/platform/exposures),
[signed updates](https://weftos.weavelogic.ai/docs/weftos/platform/update-and-signing).

## License

MIT OR Apache-2.0
