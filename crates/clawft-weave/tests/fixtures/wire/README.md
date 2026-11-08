# Weave wire fixtures (master copy)

One request line and one response line per method that Tapestry's hand-written client
(`crates/codegen/weft-journal/src/rpc.rs`, reused by `weft-policy`) uses on the weave socket
(`{"method","params","auth","proto":1}` in, `{"ok",...}` out):
`chain.append`, `chain.status`, `chain.tail`, `chain.verify`, `chain.get`, `chain.checkpoint`,
`governance.check`, `agent.sources`, plus an error reply and an unknown-method reply.

**This folder is the master. The Tapestry repo holds an identical twin** at
`crates/codegen/weft-journal/tests/fixtures/wire/`. Both must change together: edit here,
copy the `.json` files across, regenerate `sha256sums` (`shasum -a 256 *.json > sha256sums`),
and confirm the two `sha256sums` files are byte-identical. Each repo's test checks that its own
`sha256sums` matches its files; comparing the two `sha256sums` files detects drift between repos.

Tested by `crates/clawft-weave/tests/wire_fixtures.rs`: requests parse into the real
`clawft_rpc::Request` and params types; responses round-trip through the real result types;
`governance.check` replies are produced by the real handler; the chain replies are shape-checked
against a real `ChainManager`. Hashes and timestamps are well-formed but illustrative.

Not fixtures yet (not on this branch): `agent.directory` (feat/agent-directory-etag 031b107cb)
and `chain.get_range` (feat/chain-get-range b00ac6f95). Add them here and in the twin when they
land.
