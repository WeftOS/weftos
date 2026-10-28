# Installing WeftOS

`scripts/install.sh` is the canonical install path for the `weft`,
`weaver`, and `weftos` binaries. Fetch it from the `master` branch with
`curl | sh`, as below.

## Quick start

```bash
curl -fsSL https://raw.githubusercontent.com/WeftOS/weftos/master/scripts/install.sh | sh
```

The installer is idempotent: re-running it upgrades to the latest
release if you are not already on it, and is a no-op otherwise.

## Provenance verification

Every release archive that `cargo-dist` publishes ships with a
sigstore-rekor attestation. By default, `install.sh` runs
`gh attestation verify` against each downloaded archive before
installing it:

| Mode      | Trigger                                                       | Behaviour |
|-----------|---------------------------------------------------------------|-----------|
| `default` | (no flag)                                                     | Verify if `gh` is installed; warn and continue if `gh` is missing. |
| `force`   | `--verify` (or `WEFTOS_VERIFY=1` for parity with `--no-verify`) | `gh` MUST be installed. Verification failure aborts the install. |
| `skip`    | `--no-verify` or `WEFTOS_NO_VERIFY=1`                         | Skip verification entirely. |

Examples:

```bash
# Default behaviour (verify when gh is present, warn otherwise)
curl -fsSL https://raw.githubusercontent.com/WeftOS/weftos/master/scripts/install.sh | sh

# Hard-require attestation verification — refuse to install on failure
curl -fsSL https://raw.githubusercontent.com/WeftOS/weftos/master/scripts/install.sh | sh -s -- --verify

# Skip verification (for air-gapped systems without gh)
curl -fsSL https://raw.githubusercontent.com/WeftOS/weftos/master/scripts/install.sh | sh -s -- --no-verify
```

`--verify` is the recommended setting for any production install. It
guarantees that the archive was produced by the WeftOS `Release`
workflow on a tag in this repository — not pulled from a tampered
mirror.

The same check can be run manually:

```bash
gh attestation verify clawft-cli-x86_64-unknown-linux-musl.tar.gz \
  --repo WeftOS/weftos
```

See [`release.md` "Verifying Provenance"](./release.md#verifying-provenance)
for the full release-side picture.

## Custom install location

The default install directory is `/usr/local/bin`. Override with
`WEFTOS_INSTALL_DIR`:

```bash
mkdir -p ~/.local/bin
WEFTOS_INSTALL_DIR=$HOME/.local/bin \
  curl -fsSL https://raw.githubusercontent.com/WeftOS/weftos/master/scripts/install.sh | sh
```

The installer writes one binary per channel (`weft`, `weaver`,
`weftos`) directly into that directory; no extra wrappers are
created.

## Updating

```bash
weaver update              # verify, install all binaries, offer a daemon restart
weaver update --check      # report only; install nothing
weaver update --dry-run    # show which files would be replaced
weaver update --restart    # restart the per-user daemon without asking
weaver update --no-restart # never ask; print the restart command
weaver update --force      # reinstall the same version, or update an install with no receipt
```

`--allow-downgrade` and `--insecure-skip-signature` also exist; see below
before using either.

`weaver update` reads `dist-manifest.json` from the latest GitHub Release
and checks that the release is signed before it believes anything in it (see
below). It then downloads the archive for this platform and checks each
archive's sha256 against the signed list and against the published `.sha256`
(and `sha256.sum`, when the release has one) before anything is installed. A
missing, malformed or mismatching checksum aborts the update with nothing
changed. Archives are unpacked by `weaver`
itself: only regular files and directories are written, links, `..` and
absolute paths are refused, and the unpacked size is capped. Each extracted
binary is also run once with `--version` to confirm it matches the release.

**Release signature.** Every release carries `weftos-release.json`, which
lists the sha256 of every file in the release (`dist-manifest.json`
included), and `weftos-release.json.sig`, an Ed25519 signature over it by
the WeaveLogic release key. Release CI makes both. `weaver` has the public key
compiled in. It is a dedicated release key, separate from the key that signs cogs
(COG-008), and no file,
variable or setting replaces it. Before downloading any archive, `weaver
update` requires:

- the signature to verify under that key;
- the signed tag to be the manifest's tag;
- `dist-manifest.json` to hash to its signed entry.

Each archive must then hash to its own signed entry. The `.sha256` files only
prove integrity: they come from the same release as the archives, so anyone
who can replace an archive can also rehash it. The signature is what proves
the release came from WeaveLogic. An unsigned release, a bad signature, a
signature for another tag, or an archive or manifest that does not match the
signed list is refused, with nothing downloaded or installed. `--check`
refuses too, so an unverified version is never reported as available.

The signed list also records when the release was signed (`published`). If
the latest release was signed more than 90 days ago, `weaver update` warns:
a mirror or attacker may be serving an old release and hiding newer ones.

If an operator has revoked the release key (a `signer_key` entry in
`revoked_subjects.json`, the same list cog installs honour), `weaver update`
refuses everything, `--check` included. It reads the user-level lists
whatever directory you run it from: `$WEFTOS_RUNTIME_DIR` when set,
otherwise `~/.weftos/run` and the legacy `~/.clawft`. Inside a project, it
also adds that project's `.weftos/runtime` list. An unreadable user-level
list stops the update. An unreadable project list is ignored with a
warning, so a broken file in a cloned repository cannot block updates. A revoked key means a new key, which can only
arrive with a new `weaver`. Reinstall out of band from a source you trust,
following WeaveLogic's key-rotation announcement.

**No downgrades.** `weaver update` never installs a release older than the
running build, and `--force` does not change that: it only reinstalls the
same version or confirms an install with no receipt. The receipt also records
the highest version ever installed through it (`weftos_highest_version`),
and a release below that is refused as well. This stops a validly signed but
older release, served as "latest", from rolling you back to fixed bugs.
`--allow-downgrade` installs the older release anyway, with a warning. A
downgrade never lowers the recorded highest version.

`--insecure-skip-signature` installs without checking the signature. It
prints a warning, and the sha256 and archive checks still run. It exists for
an emergency, such as a release published before signing existed or a lost
key, and means trusting whoever controls the GitHub release.

Downloads use `curl -q` (your
`.curlrc` is ignored), https only, at most 5 redirects, and ignore
`CURL_CA_BUNDLE`, `SSL_CERT_FILE` and `SSL_CERT_DIR`. Standard proxy variables
(`HTTPS_PROXY`, `ALL_PROXY`, `NO_PROXY`) still apply.

All binaries in the release (`weft`, `weaver`, `weftos`) are replaced
together. Each file is swapped with an atomic same-directory rename, and if
any swap fails every binary is restored to the version it had before.

It only updates installs it owns:

| Install | What `weaver update` does |
|---|---|
| Release installer (cargo-dist receipt at `~/.config/weftos/weftos-receipt.json`) | Updates all binaries in the receipt's install directory and records the new version in the receipt. |
| No receipt (for example a hand-copied release tarball) | Updates the binaries next to the running `weaver`; any that are missing there are skipped. Because nothing records who installed them, it asks first on a terminal and otherwise needs `--force`. |
| System package manager (`/usr/bin`, `/usr/lib`, `/bin`, `/nix/store`, `/snap`) | Refuses and points at the package manager. Detected by path only; dpkg and rpm databases are not consulted. |
| Homebrew | Refuses and prints `brew upgrade weave-logic-ai/tap/<formula>`. |
| `cargo install` | Refuses and prints the matching command. |
| Source build (`scripts/build.sh install`, a `-dirty` build) | Refuses and prints `scripts/build.sh install`. |

A refused update exits non-zero. A refusal also happens when any binary in the
install directory belongs to one of the channels above, or when the running
`weaver` is not the copy the receipt manages. When the install
directory is not writable, the update stops before changing anything and tells
you to fix its permissions or reinstall as the user who owns it.

After installing, `weaver update` looks for the per-user daemon. On a
terminal it asks whether to restart it; in a script it prints the command
instead, unless you pass `--restart`. When the daemon runs under launchd or a
systemd user unit, the restart goes through that service manager and the
printed command is the matching `launchctl kickstart -k` or
`systemctl --user restart`. A daemon started from a different binary path is
reported, not restarted. Under `sudo` (root with `SUDO_USER` set) it never
restarts a daemon, because the home directory and uid would be root's. The machine mesh service is never touched; the
command to refresh it is printed. Finally, any other copies of the binaries
found on this machine are listed with the command that updates them.

`weaver update --check` always exits 0, so read its output rather than its exit
code.

## Checking what is installed

`weaver doctor install` (or `weft doctor install`) lists every copy of
`weft`, `weaver` and `weftos`, which install channel owns each, which
one wins on `$PATH`, and any running daemon whose binary is a different
file. It is read-only. See the
[CLI reference](../reference/cli.md#weft-doctor--weaver-doctor).

## Uninstall

There is no separate uninstall script. Remove the three binaries from
`$WEFTOS_INSTALL_DIR`:

```bash
sudo rm /usr/local/bin/weft /usr/local/bin/weaver /usr/local/bin/weftos
```

User-level state lives under `~/.clawft/` — see the
[Canonical Install Paths](./release.md#canonical-install-paths)
section in the release docs.
