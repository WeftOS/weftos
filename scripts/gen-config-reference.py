#!/usr/bin/env python3
"""Generate docs/reference/config-sections.md from the config structs.

Reads the serde structs that define `[kernel]`, `[kernel.mesh]`, `[router]`,
the project manifest, `cog.toml` and `[mesh_limits]`, and writes one table per
struct: serde field name (with aliases), type, default and the doc comment.
Defaults come from `#[serde(default = "fn")]` bodies in the same file.

Struct headings carry file:line. Line numbers move with unrelated edits, so
this generator is NOT part of the CI drift gate; rerun it after changing a
config struct:

  scripts/build.sh gen-config-reference
  scripts/gen-config-reference.py [--check]
"""
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "docs/reference/config-sections.md"
CFG = "crates/clawft-types/src/config/kernel.rs"

# (heading, toml path, file, struct, extra note)
SECTIONS = [
    ("kernel", "[kernel]", CFG, "KernelConfig", "Top-level kernel settings in `weave.toml` or the `kernel` object of the JSON config."),
    ("kernel.cluster", "[kernel.cluster]", CFG, "ClusterNetworkConfig", ""),
    ("kernel.chain", "[kernel.chain]", CFG, "ChainConfig", ""),
    ("kernel.chain.external_anchor", "[kernel.chain.external_anchor]", CFG, "ChainExternalAnchorConfig", ""),
    ("kernel.resource_tree", "[kernel.resource_tree]", CFG, "ResourceTreeConfig", ""),
    ("kernel.vector", "[kernel.vector]", CFG, "VectorConfig", ""),
    ("kernel.spatial", "[kernel.spatial]", CFG, "SpatialConfig", ""),
    ("kernel.profiles", "[kernel.profiles]", CFG, "ProfilesConfig", ""),
    ("kernel.pairing", "[kernel.pairing]", CFG, "PairingConfig", ""),
    ("kernel.anchor", "[kernel.anchor]", CFG, "AnchorConfig", ""),
    ("kernel.ipc_tcp", "[kernel.ipc_tcp]", CFG, "IpcTcpConfig", ""),
    ("kernel.llm", "[kernel.llm]", CFG, "LlmEndpointConfig", ""),
    ("kernel.agent", "[kernel.agent]", CFG, "AgentAnchorConfig", ""),
    ("kernel.shared_services", "[kernel.shared_services]", CFG, "SharedServicesConfig", ""),
    ("kernel.governance", "[kernel.governance]", "crates/clawft-types/src/config/governance.rs", "GovernanceConfig", ""),
    ("kernel.mesh", "[kernel.mesh]", CFG, "MeshConfig", "The machine mesh transport and admission settings (ADR-103, ADR-106)."),
    ("router", "[router]", "crates/clawft-weave/src/router_cfg.rs", "RouterConfig", "The tailnet router in `~/.weftos/weave.toml` (ADR-116). Unknown keys are rejected."),
    ("project manifest", "~/.weftos/projects/<id>.toml", "crates/clawft-types/src/project/schema.rs", "ProjectManifest", "One file per registered project (ADR-103). Unknown keys are preserved on rewrite."),
    ("project manifest [serve]", "[serve]", "crates/clawft-types/src/project/schema.rs", "ServeSection", ""),
    ("project manifest [chain]", "[chain]", "crates/clawft-types/src/project/schema.rs", "ChainSection", ""),
    ("project manifest [binary]", "[binary]", "crates/clawft-types/src/project/schema.rs", "BinaryInfo", ""),
    ("cog.toml", "(file root)", "crates/clawft-kernel/src/workload_runtime/cog_spec.rs", "RawToml", "A cog's manifest (ADR-100, ADR-122). Fields without a doc comment are described in the cog operator guide."),
    ("cog.toml [cog]", "[cog]", "crates/clawft-kernel/src/workload_runtime/cog_spec.rs", "RawCog", ""),
    ("cog.toml [console]", "[console]", "crates/clawft-kernel/src/workload_runtime/cog_spec.rs", "RawConsole", ""),
    ("cog.toml [resources]", "[resources]", "crates/clawft-kernel/src/workload_runtime/cog_spec.rs", "RawResources", ""),
    ("cog.toml [[outputs]]", "[[outputs]]", "crates/clawft-kernel/src/workload_runtime/cog_decl.rs", "RawOutput", "Topic declarations. Every cog with outputs must also declare `[mesh_limits]`."),
    ("cog.toml [[inputs]]", "[[inputs]]", "crates/clawft-kernel/src/workload_runtime/cog_decl.rs", "RawInput", ""),
    ("cog.toml [[devices]]", "[[devices]]", "crates/clawft-kernel/src/workload_runtime/cog_devices.rs", "RawDevice", "Exactly one selector per entry."),
    ("cog.toml [mesh_limits]", "[mesh_limits]", "crates/clawft-types/src/cog_topics/limits.rs", "MeshLimits", "What one cog lets another node take from its topics (ADR-122 section 14). All four values are required for a cog with `[[outputs]]`; ranges are enforced at pack time."),
]

# Raw cog.toml structs carry no doc comments; descriptions and defaults here
# come from cog_spec.rs / cog_decl.rs / cog_devices.rs (the enforcing code).
OVERRIDES = {
    "RawToml.cog": ("", "Identity table; required."),
    "RawToml.console": ("", "Limits on `weaver cog` console runs."),
    "RawToml.resources": ("", "Memory and CPU caps for the running cog."),
    "RawToml.config": ("`{}`", "Free-form cog configuration, passed to the cog unvalidated."),
    "RawToml.outputs": ("`[]`", "Topic declarations the cog publishes. A non-empty list requires `[mesh_limits]` at pack time."),
    "RawToml.inputs": ("`[]`", "Topic declarations the cog subscribes to."),
    "RawToml.devices": ("`[]`", "Devices the cog owns (a `/dev` path or glob, a USB selector, a socket or a netdev). Selectors that can name raw memory, disks, consoles or similar are refused at parse time."),
    "RawToml.mesh_limits": ("", "What other nodes may take from this cog's topics; see `[mesh_limits]` below."),
    "RawCog.id": ("", "Cog id; matches the catalog and the install directory."),
    "RawCog.version": ("", "Cog version."),
    "RawCog.interfaces": ("`[]`", "What the native half opens (for example a serial port or I2C bus)."),
    "RawConsole.allowed_commands": ("`[]`", "Exact argument strings a console run may use (at most 32 arguments each)."),
    "RawConsole.max_runtime_secs": ("`15`", "Wall-clock cap for a console run, in seconds (1 up to the code's maximum)."),
    "RawConsole.output_limit_bytes": ("`65536`", "Captured bytes per stream (1 up to the code's maximum)."),
    "RawResources.ram_mb": ("`256`", "Memory cap in MiB (1..=65536)."),
    "RawResources.cpu_pct": ("`100`", "CPU share in percent of one core (1..=400)."),
    "RawOutput.topic": ("", "Topic kind, for example `audio.pcm`; the live topic adds the source id."),
    "RawOutput.schema": ("", "Registered schema id."),
    "RawOutput.class": ("", "Size class: `reading`, `stream` or `blob`."),
    "RawOutput.rate": ("unset", "Free-text rate, for example `16000 Hz, 20 ms chunks`."),
    "RawInput.topic": ("", "Topic kind."),
    "RawInput.schema": ("", "Registered schema id."),
    "RawInput.policy": ("`\"drop_oldest\"`", "What a slow reader does: `drop_oldest`, `block` or `sample`."),
    "RawDevice.path": ("unset", "A `/dev` path or glob."),
    "RawDevice.usb": ("unset", "A USB selector."),
    "RawDevice.socket": ("unset", "A socket path."),
    "RawDevice.netdev": ("unset", "A network device name."),
    "RawDevice.access": ("`\"rw\"`", "`rw` (default) or `ro`."),
}

ENUMS = [
    ("MeshAdmissionMode", CFG),
    ("MeshServicePolicy", CFG),
    ("KernelProfile", CFG),
    ("SharedServiceMode", CFG),
    ("ChainAnchorBackend", CFG),
    ("VectorBackendKind", CFG),
    ("TopicClass", "crates/clawft-types/src/cog_topics/mod.rs"),
    ("InputPolicy", "crates/clawft-types/src/cog_topics/mod.rs"),
]


def find_block(text, header_re):
    m = re.search(header_re, text, re.M)
    if not m:
        return None
    i = text.index("{", m.end() - 1)
    depth, j = 0, i
    while j < len(text):
        c = text[j]
        if c == "{":
            depth += 1
        elif c == "}":
            depth -= 1
            if depth == 0:
                break
        j += 1
    line = text.count("\n", 0, m.start()) + 1
    # include attributes/docs above for rename_all
    head_start = text.rfind("\n\n", 0, m.start())
    head = text[head_start if head_start >= 0 else 0 : m.start()]
    return text[i + 1 : j], line, head


def const_value(text, name):
    m = re.search(r'const\s+' + re.escape(name) + r'\s*:\s*[^=]+=\s*(.+?);', text)
    return m.group(1).strip() if m else None


def fn_default(text, name):
    m = re.search(r'fn\s+' + re.escape(name) + r'\s*\(\)\s*->\s*[^{]+\{', text)
    if not m:
        return f"`{name}()`"
    depth, j = 1, m.end()
    while j < len(text) and depth:
        depth += {"{": 1, "}": -1}.get(text[j], 0)
        j += 1
    e = re.sub(r'\s+', ' ', text[m.end() : j - 1]).strip().rstrip(";")
    e = re.sub(r'\.(to_owned|to_string|into)\(\)', '', e)
    e = e.replace("String::from(", "(").replace("Self::", "")
    e = re.sub(r'^\((".*")\)$', r'\1', e)
    cv = const_value(text, e)
    if cv:
        e = cv
    fm = re.fullmatch(r'format!\("(.*)",? ?(.*)\)', e)
    if fm:
        e = '"' + re.sub(r'\{[^}]*\}', lambda _m: "{" + _m.group(0)[1:-1] + "}", fm.group(1)) + '"'
        for c in re.findall(r'\{([A-Z_]+)\}', e):
            v = const_value(text, c)
            if v:
                e = e.replace("{" + c + "}", v.strip('"'))
    return f"`{e}`"


def enum_default(text, ty):
    """Serialized name of an enum's default variant, if it can be read."""
    r = render_enum_text(text, ty)
    if r is None:
        return None
    dm = re.search(r'impl\s+Default\s+for\s+' + re.escape(ty) + r'\s*\{[^}]*?Self::(\w+)', text, re.S)
    if dm:
        want = dm.group(1)
    else:
        blk = find_block(text, r'^(?:pub(?:\([^)]*\))?\s+)?enum\s+' + re.escape(ty) + r'\b[^;{]*\{')
        hm = re.search(r'#\[default\]\s*(?:#\[[^\n]*\]\s*)*([A-Z]\w*)', blk[0]) if blk else None
        if not hm:
            return None
        want = hm.group(1)
    for raw_name, ser in r[1]:
        if raw_name == want:
            return f"`\"{ser}\"`"
    return None


def zero_default(ty):
    t = ty.strip()
    if t.startswith("Option<"):
        return "unset"
    if t == "bool":
        return "`false`"
    if t.startswith("Vec<") or t.startswith("BTreeSet<"):
        return "`[]`"
    if t.startswith("BTreeMap<") or t.startswith("HashMap<") or t == "toml::Table":
        return "`{}`"
    if t in ("String", "PathBuf"):
        return '`""`'
    if re.fullmatch(r'[ui](8|16|32|64|size)|f(32|64)', t):
        return "`0`"
    return "type default"


def doc_text(lines):
    paras, cur = [], []
    for ln in lines:
        if ln.strip() == "":
            if cur:
                paras.append(" ".join(cur))
                cur = []
        else:
            cur.append(ln.strip())
    if cur:
        paras.append(" ".join(cur))
    return paras[0] if paras else ""


def parse_fields(body, whole):
    fields = []
    docs, attrs = [], []
    buf = ""
    for raw in body.split("\n"):
        s = raw.strip()
        if buf:
            buf += " " + s
            if buf.count("(") <= buf.count(")") and buf.endswith("]"):
                attrs.append(buf)
                buf = ""
            continue
        if s.startswith("///"):
            docs.append(s[3:].strip() if len(s) > 3 else "")
        elif s.startswith("#["):
            if s.endswith("]") and s.count("(") <= s.count(")"):
                attrs.append(s)
            else:
                buf = s
        elif s.startswith("//"):
            continue
        elif s == "":
            continue
        else:
            m = re.match(r'(?:pub(?:\([^)]*\))?\s+)?([a-z_][a-z0-9_]*)\s*:\s*(.+?),?$', s)
            if m:
                fields.append((m.group(1), m.group(2).rstrip(","), docs, attrs))
            docs, attrs = [], []
    return fields


def render_struct(file, name, rename_all_hint=None):
    path = ROOT / file
    text = path.read_text()
    blk = find_block(text, r'^(?:pub(?:\([^)]*\))?\s+)?struct\s+' + re.escape(name) + r'\b[^;{]*\{')
    if blk is None:
        raise SystemExit(f"struct {name} not found in {file}")
    body, line, head = blk
    rows = []
    for fname, ty, docs, attrs in parse_fields(body, text):
        a = " ".join(attrs)
        if "serde(skip)" in a:
            continue
        key = fname
        m = re.search(r'rename\s*=\s*"([^"]+)"', a)
        if m:
            key = m.group(1)
        aliases = re.findall(r'alias\s*=\s*"([^"]+)"', a)
        if "flatten" in a:
            key = "(flattened)"
        d = re.search(r'default\s*=\s*"([^"]+)"', a)
        if d:
            default = fn_default(text, d.group(1).split("::")[-1])
        elif re.search(r'default\b', a):
            default = zero_default(ty)
        elif ty.startswith("Option<"):
            default = "unset"
        else:
            default = "required"
        if default == "type default":
            bare = ty.strip()
            ed = enum_default(text, bare)
            if ed is None:
                for f2 in (CFG,):
                    ed = enum_default((ROOT / f2).read_text(), bare)
                    if ed:
                        break
            if ed:
                default = ed
        ty_disp = re.sub(r'\s+', ' ', ty).replace("super::", "").replace("|", "\\|")
        doc = doc_text(docs).replace("|", "\\|")
        k = f"`{key}`" + (" (alias " + ", ".join(f"`{x}`" for x in aliases) + ")" if aliases else "")
        ov = OVERRIDES.get(f"{name}.{fname}")
        if ov:
            default = ov[0] or default
            doc = ov[1]
        rows.append((k, f"`{ty_disp}`", default, doc))
    return line, rows


def render_enum_text(text, name):
    blk = find_block(text, r'^(?:pub(?:\([^)]*\))?\s+)?enum\s+' + re.escape(name) + r'\b[^;{]*\{')
    if blk is None:
        return None
    body, line, head = blk
    ra = re.search(r'rename_all\s*=\s*"([^"]+)"', head)
    out, docs, attrs = [], [], []
    for raw in body.split("\n"):
        s = raw.strip()
        if s.startswith("///"):
            docs.append(s[3:].strip())
        elif s.startswith("#["):
            attrs.append(s)
        elif re.match(r'[A-Z]\w*', s):
            v = re.match(r'([A-Z]\w*)', s).group(1)
            r = re.search(r'rename\s*=\s*"([^"]+)"', " ".join(attrs))
            if r:
                v2 = r.group(1)
            elif ra and ra.group(1) == "snake_case":
                v2 = re.sub(r'(?<!^)(?=[A-Z])', '_', v).lower()
            elif ra and ra.group(1) == "lowercase":
                v2 = v.lower()
            else:
                v2 = v
            out.append((v, v2, doc_text(docs)))
            docs, attrs = [], []
    return line, [(a, b) for a, b, _ in out], [(b, c) for _, b, c in out]


def render_enum(name, file):
    r = render_enum_text((ROOT / file).read_text(), name)
    if r is None:
        return None
    return r[0], r[2]


def render():
    o = []
    w = o.append
    w("# Configuration sections: kernel, mesh, router, project and cog.toml\n")
    w("Generated by `scripts/gen-config-reference.py` from the serde structs named in each heading. Do not edit by hand; run `scripts/build.sh gen-config-reference`.\n")
    w("This page covers the sections the main [configuration reference](config.md) does not. Field names are the serde names; `alias` lists accepted alternatives. `required` means the field has no default and parsing fails without it. Defaults are read from the struct's `default` functions, so a default set elsewhere (for example in an `impl Default`) reads as `type default`.\n")
    w("## Contents\n")
    for head, toml, file, st, note in SECTIONS:
        anchor = re.sub(r'[^a-z0-9 -]', '', head.lower()).replace(" ", "-")
        w(f"- [`{toml}`](#{anchor}) from `{st}`")
    w("")
    for head, toml, file, st, note in SECTIONS:
        line, rows = render_struct(file, st)
        w(f"## {head}\n")
        w(f"Table `{toml}`. Struct `{st}` at `{file}:{line}`.\n")
        if note:
            w(note + "\n")
        w("| Field | Type | Default | Description |")
        w("|-------|------|---------|-------------|")
        for r in rows:
            w("| " + " | ".join(r) + " |")
        w("")
    w("## Enum values\n")
    for name, file in ENUMS:
        r = render_enum(name, file)
        if not r:
            continue
        line, vals = r
        w(f"### {name}\n")
        w(f"`{file}:{line}`\n")
        w("| Value | Meaning |")
        w("|-------|---------|")
        for v, d in vals:
            w(f"| `{v}` | {d.replace('|', chr(92) + '|')} |")
        w("")
    return "\n".join(o)


def main():
    text = render()
    if "--check" in sys.argv:
        cur = OUT.read_text() if OUT.exists() else ""
        if cur != text:
            sys.stderr.write("DRIFT: docs/reference/config-sections.md differs from the config structs\n")
            sys.exit(1)
        print("config reference up to date")
        return
    OUT.write_text(text)
    print(f"wrote {OUT.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
