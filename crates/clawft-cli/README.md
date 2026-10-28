# clawft-cli

`weft`, the WeftOS agent CLI. Use it to talk to an agent, run the gateway for
channels, manage skills, tools, sessions and memory, and run the chief-of-staff
host. Not published to crates.io (`publish = false`); install the binary from
[GitHub Releases](https://github.com/WeftOS/weftos/releases) or build it.

```sh
export OPENAI_API_KEY="sk-..."            # or configure another provider
weft agent -m "Summarize this project"
weft agent --local -m "Summarize this project"   # no daemon: run in-process
weft tools list
weft skills list
weft --help                               # every command
```

The operator side (kernel, placement, mesh) is [`weaver`](../clawft-weave/README.md).

Part of [WeftOS](https://github.com/WeftOS/weftos), version 0.8.4.
See the [CLI reference](https://weftos.weavelogic.ai/docs/clawft/cli-reference)
and the [quickstart](https://weftos.weavelogic.ai/docs/weftos/getting-started/quickstart).

## License

MIT OR Apache-2.0
