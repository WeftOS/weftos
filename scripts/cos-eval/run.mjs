#!/usr/bin/env node
// cos-eval: run the chief-of-staff E-cases through `weftos-cos mcp`.
//
//   node scripts/cos-eval/run.mjs                      scratch agent on 127.0.0.1:18320
//   node scripts/cos-eval/run.mjs --only E1,E13        some cases
//   node scripts/cos-eval/run.mjs --agent http://127.0.0.1:18300   the live agent
//
// Default target is a scratch agent this script starts (and stops, by pid,
// per case so no case sees another's history): speech off, no mic, the
// existing local model server, a fake harness for the board, a fake daemon
// runtime dir (so no daemon answers), scratch staging and notes store. It
// never touches a process it did not start. Cases are data in cases.json; the
// assertions are tool-trace expectations and rule-based answer checks. Exit 0
// when the score meets the pass bar and every critical case passes, 1 when
// not, 2 when the run could not be set up.
import { spawn, spawnSync } from "node:child_process";
import fs from "node:fs";
import net from "node:net";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { PROJECT_ID, TOKEN, startFakeHarness } from "./fake-harness.mjs";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(HERE, "../..");

function parseArgs(argv) {
  const a = { agent: null, bin: null, port: 18320, harnessPort: 18321, only: null, casesFile: path.join(HERE, "cases.json"), modelBase: "http://127.0.0.1:8081/v1", model: "local/mlx-community/GLM-4.7-Flash-4bit", report: null, timeoutS: 150 };
  for (let i = 0; i < argv.length; i++) {
    const k = argv[i];
    const v = () => {
      if (i + 1 >= argv.length) die(`${k} needs a value`);
      return argv[++i];
    };
    if (k === "--agent") a.agent = v().replace(/\/$/, "");
    else if (k === "--bin") a.bin = v();
    else if (k === "--port") a.port = Number(v());
    else if (k === "--harness-port") a.harnessPort = Number(v());
    else if (k === "--only") a.only = v().split(",").map((s) => s.trim().toUpperCase()).filter(Boolean);
    else if (k === "--cases") a.casesFile = path.resolve(v());
    else if (k === "--model-base") a.modelBase = v();
    else if (k === "--model") a.model = v();
    else if (k === "--report") a.report = path.resolve(v());
    else if (k === "--timeout") a.timeoutS = Number(v());
    else if (k === "-h" || k === "--help") {
      console.log(fs.readFileSync(fileURLToPath(import.meta.url), "utf8").split("\n").slice(1, 14).map((l) => l.replace(/^\/\/ ?/, "")).join("\n"));
      process.exit(0);
    } else die(`unknown argument ${k}`);
  }
  for (const [n, p] of [["--port", a.port], ["--harness-port", a.harnessPort]]) {
    if (!Number.isInteger(p) || p < 1024 || p > 65535) die(`${n} must be a port from 1024 to 65535`);
  }
  if (!Number.isFinite(a.timeoutS) || a.timeoutS < 1 || a.timeoutS > 600) die("--timeout must be 1 to 600 seconds");
  return a;
}

function die(msg) {
  console.error(`cos-eval: ${msg}`);
  process.exit(2);
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function portInUse(port) {
  return new Promise((resolve) => {
    const s = net.connect(port, "127.0.0.1");
    s.once("connect", () => { s.destroy(); resolve(true); });
    s.once("error", () => resolve(false));
  });
}

// ── The weftos-cos binary ───────────────────────────────────────────────
function resolveBin(args) {
  const candidates = [args.bin, process.env.WEFTOS_COS_BIN, path.join(ROOT, "target/debug/weftos-cos"), path.join(ROOT, "target/release/weftos-cos")].filter(Boolean);
  for (const c of candidates) {
    if (!fs.existsSync(c)) continue;
    const help = spawnSync(c, ["--help"], { encoding: "utf8" });
    if (/\bmcp\b/.test(help.stdout ?? "")) return c;
    console.error(`cos-eval: ${c} has no \`mcp\` command (built before M1); skipping it`);
  }
  die("no weftos-cos binary with the `mcp` command; run `scripts/build.sh cos` (it builds target/debug/weftos-cos without installing it) or pass --bin");
}

// ── A stdio MCP client ──────────────────────────────────────────────────
class McpClient {
  constructor(bin, agent, endpoint) {
    this.child = spawn(bin, ["mcp", "--agent", agent, "--endpoint", endpoint], { stdio: ["pipe", "pipe", "pipe"] });
    this.nextId = 1;
    this.pending = new Map();
    this.buf = "";
    this.stderr = "";
    this.child.stdout.on("data", (d) => this.#data(d));
    this.child.stderr.on("data", (d) => { this.stderr = (this.stderr + d).slice(-4000); });
    this.child.on("exit", () => {
      for (const { reject } of this.pending.values()) reject(new Error(`mcp server exited: ${this.stderr.trim().split("\n").pop()}`));
      this.pending.clear();
    });
  }

  #data(d) {
    this.buf += d;
    let nl;
    while ((nl = this.buf.indexOf("\n")) >= 0) {
      const line = this.buf.slice(0, nl).trim();
      this.buf = this.buf.slice(nl + 1);
      if (!line) continue;
      let msg;
      try { msg = JSON.parse(line); } catch { continue; }
      const p = this.pending.get(msg.id);
      if (p) {
        this.pending.delete(msg.id);
        p.resolve(msg);
      }
    }
  }

  request(method, params) {
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`);
    });
  }

  async init() {
    const r = await this.request("initialize", { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "cos-eval", version: "1" } });
    if (r.error) throw new Error(`initialize: ${r.error.message}`);
    this.child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" })}\n`);
  }

  /** Call a tool; returns { isError, body } where body is the parsed JSON text. */
  async call(name, args) {
    const r = await this.request("tools/call", { name, arguments: args });
    if (r.error) return { isError: true, body: { status: "protocol-error", reason: r.error.message } };
    const text = r.result?.content?.[0]?.text ?? "";
    let body;
    try { body = JSON.parse(text); } catch { body = { raw: text }; }
    return { isError: Boolean(r.result?.isError), body };
  }

  stop() {
    this.child.stdin.end();
    this.child.kill("SIGTERM");
  }
}

// ── The scratch agent ───────────────────────────────────────────────────
class Scratch {
  constructor(args, bin, dir) {
    this.args = args;
    this.bin = bin;
    this.dir = dir;
    this.child = null;
    this.url = `http://127.0.0.1:${args.port}`;
    this.staging = path.join(dir, "staging");
    this.mentra = path.join(dir, "mentra");
    for (const d of [this.staging, this.mentra, path.join(dir, "rt"), path.join(dir, "registry")]) fs.mkdirSync(d, { recursive: true });
    fs.chmodSync(dir, 0o700);
    const tokenFile = path.join(dir, "board-token");
    fs.writeFileSync(tokenFile, `${TOKEN}\n`, { mode: 0o600 });
    this.cfgPath = path.join(dir, "cos.json");
    fs.writeFileSync(this.cfgPath, JSON.stringify({
      bind: `127.0.0.1:${args.port}`,
      models: { quick: args.model, plan: args.model, fallback: [], local_api_base: args.modelBase },
      harness: { base_url: `http://127.0.0.1:${args.harnessPort}`, token_file: tokenFile, project_id: PROJECT_ID },
      staging_dir: this.staging,
      tool_log: path.join(dir, "tool-log.jsonl"),
      voice: { tts: { engine: "none" } },
      brain: { path: path.join(dir, "notes.store"), installation: "local" },
      devices: { mentra_dir: this.mentra, glance_status: path.join(dir, "glance.status.json"), leaf_registry_dir: path.join(dir, "registry"), pad_leaf_ids: ["abcdef12"] },
      serve: { bind: null },
    }, null, 2));
  }

  async start(tag) {
    if (await portInUse(this.args.port)) throw new Error(`port ${this.args.port} is already in use by a process this script did not start; pick another with --port`);
    const log = fs.openSync(path.join(this.dir, `agent-${tag}.log`), "a");
    this.child = spawn(this.bin, ["serve", "--config", this.cfgPath, "--no-stdin"], {
      cwd: ROOT,
      stdio: ["ignore", log, log],
      // WEFTOS_RUNTIME_DIR points the daemon resolver at an empty directory,
      // so nodes.health and the pad check find no daemon.
      env: { ...process.env, WEFTOS_RUNTIME_DIR: path.join(this.dir, "rt"), WEFTOS_BOARD_TOKEN_FILE: path.join(this.dir, "board-token") },
    });
    fs.closeSync(log);
    for (let i = 0; i < 150; i++) {
      if (this.child.exitCode !== null) throw new Error(`scratch agent exited (${this.child.exitCode}); see ${path.join(this.dir, `agent-${tag}.log`)}`);
      try {
        const r = await fetch(`${this.url}/healthz`);
        if (r.ok) return;
      } catch { /* not yet */ }
      await sleep(200);
    }
    throw new Error("scratch agent did not answer /healthz in 30 s");
  }

  async stop() {
    const c = this.child;
    if (!c) return;
    this.child = null;
    if (c.exitCode !== null) return;
    const exited = new Promise((r) => c.once("exit", r));
    c.kill("SIGTERM");
    const t = await Promise.race([exited.then(() => "exit"), sleep(5000).then(() => "timeout")]);
    if (t === "timeout") {
      c.kill("SIGKILL");
      await exited;
    }
  }

  clearStaging() {
    for (const f of fs.readdirSync(this.staging)) fs.rmSync(path.join(this.staging, f), { recursive: true, force: true });
  }

  stagedFiles() {
    return fs.readdirSync(this.staging).length;
  }
}

// ── Device fixtures: status files the glasses loops would write ─────────
function writeFixture(dir, name) {
  const now = new Date();
  const iso = (ago = 0) => new Date(now.getTime() - ago * 1000).toISOString();
  const takes = name === "glasses-silent"
    ? [0, 1, 2, 3].map((i) => ({ at: iso(20 + i * 15), rms: 0.004, voiced: 0.02, outcome: "skipped", reason: "below_floor", transcript: null }))
    : [
        { at: iso(90), rms: 0.031, voiced: 0.62, outcome: "posted", reason: null, transcript: "what is next on the board" },
        { at: iso(40), rms: 0.005, voiced: 0.03, outcome: "skipped", reason: "below_floor", transcript: null },
        { at: iso(10), rms: 0.027, voiced: 0.58, outcome: "posted", reason: null, transcript: "is the pad connected" },
      ];
  fs.writeFileSync(path.join(dir, "listen.status.json"), JSON.stringify({ updated_at: iso(), listening: true, armed: true, pid: process.pid, recent: takes, last_take: takes[takes.length - 1] }));
  fs.writeFileSync(path.join(dir, "speak.status.json"), JSON.stringify({ updated_at: iso(), up: true, last_play: { at: iso(120), chars: 180, ok: true, error: null } }));
}

function startFixture(dir, name) {
  if (!name) return () => {};
  writeFixture(dir, name);
  // The status files go stale after 30 s; keep them fresh while the case runs.
  const t = setInterval(() => writeFixture(dir, name), 4000);
  return () => clearInterval(t);
}

// ── Assertions ──────────────────────────────────────────────────────────
function matches(text, rule) {
  const lower = text.toLowerCase();
  if (typeof rule === "string") {
    if (rule.startsWith("re:")) return new RegExp(rule.slice(3), "i").test(text);
    return lower.includes(rule.toLowerCase());
  }
  if (rule && Array.isArray(rule.any)) return rule.any.some((r) => matches(text, r));
  throw new Error(`bad answer rule ${JSON.stringify(rule)}`);
}

const SECRET = /\bwf[bt]_[0-9a-f]{16,}|\bsk-[A-Za-z0-9_-]{20,}|bearer\s+[A-Za-z0-9._-]{20,}/i;

function evaluate(c, defaults, run) {
  const { say, trace, message, writes, staged } = run;
  const ex = c.expect ?? {};
  const fails = [];
  const called = (trace?.tools_called ?? []).map((t) => t.name);
  const gates = trace?.gates ?? [];

  const wantStatus = ex.status ?? null;
  if (wantStatus ? say.status !== wantStatus : !["ok", "held"].includes(say.status)) fails.push(`turn status ${say.status}${say.error ? ` (${say.error})` : ""}${wantStatus ? `, wanted ${wantStatus}` : ""}`);
  if (!trace) fails.push("no trace returned");
  if (ex.tools_any && !ex.tools_any.some((t) => called.includes(t))) fails.push(`none of [${ex.tools_any}] called (called: ${called.join(", ") || "nothing"})`);
  for (const t of ex.tools_none ?? []) if (called.includes(t)) fails.push(`forbidden tool ${t} was called`);
  for (const t of ex.tools_ok ?? []) {
    const calls = (trace?.tools_called ?? []).filter((x) => x.name === t);
    if (!calls.length || !calls.every((x) => x.ok)) fails.push(`${t} did not succeed`);
  }
  for (const g of ex.gates ?? []) if (!gates.includes(g)) fails.push(`gate ${g} not hit (gates: ${gates.join(", ") || "none"})`);
  const maxTools = ex.max_tools ?? defaults.max_tools;
  if (called.length > maxTools) fails.push(`${called.length} tool calls, budget ${maxTools}`);
  if (trace && trace.rounds > (ex.max_rounds ?? defaults.max_rounds)) fails.push(`${trace.rounds} model rounds, budget ${ex.max_rounds ?? defaults.max_rounds}`);
  if (trace && trace.latency_ms > (ex.max_ms ?? defaults.max_ms)) fails.push(`${trace.latency_ms} ms, budget ${ex.max_ms ?? defaults.max_ms}`);
  if (ex.board_writes !== undefined && writes !== null && writes !== ex.board_writes) fails.push(`${writes} board writes, wanted ${ex.board_writes}`);
  if (ex.staged_files_min !== undefined && staged !== null && staged < ex.staged_files_min) fails.push(`${staged} staged files, wanted at least ${ex.staged_files_min}`);
  if (ex.staged_files_max !== undefined && staged !== null && staged > ex.staged_files_max) fails.push(`${staged} staged files, wanted at most ${ex.staged_files_max}`);

  const text = message ?? "";
  if (!text.trim()) fails.push("no final message");
  for (const r of ex.answer_must ?? []) if (!matches(text, r)) fails.push(`answer lacks ${JSON.stringify(r)}`);
  for (const r of ex.answer_must_not ?? []) if (matches(text, r)) fails.push(`answer contains forbidden ${JSON.stringify(r)}`);
  if (SECRET.test(`${text}\n${JSON.stringify(trace ?? {})}`)) fails.push("a credential-shaped string appears in the output");
  return fails;
}

// ── Main ────────────────────────────────────────────────────────────────
async function main() {
  const args = parseArgs(process.argv.slice(2));
  const spec = JSON.parse(fs.readFileSync(args.casesFile, "utf8"));
  const defaults = spec.defaults ?? {};
  const known = new Set(spec.cases.map((c) => c.id));
  for (const id of args.only ?? []) if (!known.has(id)) die(`no case ${id} in ${args.casesFile}`);
  const cases = spec.cases.filter((c) => !args.only || args.only.includes(c.id));
  if (!cases.length) die("no cases selected");

  const bin = resolveBin(args);
  const runId = new Date().toISOString().replace(/[-:.TZ]/g, "").slice(0, 14);
  const dir = process.env.COS_EVAL_DIR ?? path.join(ROOT, "target/cos-eval", runId);
  fs.mkdirSync(dir, { recursive: true });
  const live = args.agent !== null;
  let scratch = null;
  let harness = null;
  if (!live) {
    if (await portInUse(args.harnessPort)) die(`port ${args.harnessPort} is in use; pick another with --harness-port`);
    harness = await startFakeHarness(args.harnessPort);
    scratch = new Scratch(args, bin, dir);
  } else {
    console.error("cos-eval: running against a live agent: history is shared between cases, and the board, staging and device-fixture checks are skipped");
  }
  const agentUrl = live ? args.agent : scratch.url;
  const mcp = new McpClient(bin, agentUrl, `test-${runId}`);
  const results = [];
  let setupError = null;

  const cleanup = async () => {
    mcp.stop();
    if (scratch) await scratch.stop();
    if (harness) harness.close();
  };
  process.on("SIGINT", async () => { await cleanup(); process.exit(130); });

  try {
    await mcp.init();
    const tools = (await mcp.request("tools/list", {})).result?.tools?.map((t) => t.name) ?? [];
    for (const need of ["cos.say", "cos.confirm", "cos.trace"]) if (!tools.includes(need)) throw new Error(`the MCP server lacks ${need}`);

    for (const c of cases) {
      if (c.pending) {
        results.push({ id: c.id, title: c.title, state: "skip", why: c.pending });
        console.log(`SKIP ${c.id.padEnd(4)} ${c.title}  [${c.pending}]`);
        continue;
      }
      const t0 = Date.now();
      let stopFixture = () => {};
      try {
        if (scratch) {
          scratch.clearStaging();
          harness.writes.length = 0;
          await scratch.start(c.id);
          stopFixture = startFixture(scratch.mentra, c.fixture);
        }
        const { isError, body } = await mcp.call("cos.say", { text: c.say, timeout_s: args.timeoutS });
        const say = { status: body.status ?? (isError ? "error" : "ok"), error: body.error ?? body.reason ?? null };
        const run = { say, trace: body.trace ?? null, message: body.message ?? null, writes: scratch ? harness.writes.length : null, staged: scratch ? scratch.stagedFiles() : null };
        const fails = evaluate(c, defaults, run);
        // A gate left held is cleared with an explicit "no", which also
        // proves cos.confirm answers a gate this endpoint raised.
        let confirmNote = "";
        if (body.status === "held") {
          const r = await mcp.call("cos.confirm", { turn_id: body.turn_id, decision: "no" });
          if (r.isError || !(r.body.trace?.gates ?? []).some((g) => g.endsWith(":cancelled"))) fails.push(`cos.confirm no did not cancel the held action (${JSON.stringify(r.body).slice(0, 160)})`);
          else confirmNote = " (held, cancelled by cos.confirm no)";
          if (scratch && c.expect?.staged_files_max !== undefined && scratch.stagedFiles() > c.expect.staged_files_max) fails.push("the cancelled action still staged a file");
        }
        const secs = ((Date.now() - t0) / 1000).toFixed(1);
        const called = (run.trace?.tools_called ?? []).map((t) => t.name);
        results.push({ id: c.id, title: c.title, state: fails.length ? "fail" : "pass", fails, tools: called, gates: run.trace?.gates ?? [], model: run.trace?.model ?? null, latency_ms: run.trace?.latency_ms ?? null, rounds: run.trace?.rounds ?? null, answer: run.message });
        console.log(`${fails.length ? "FAIL" : "PASS"} ${c.id.padEnd(4)} ${c.title}  [${called.join(", ") || "no tools"}; ${secs}s]${confirmNote}`);
        for (const f of fails) console.log(`       - ${f}`);
        if (fails.length && run.message) console.log(`       answer: ${run.message.replace(/\s+/g, " ").slice(0, 300)}`);
      } catch (e) {
        results.push({ id: c.id, title: c.title, state: "fail", fails: [`setup: ${e.message}`] });
        console.log(`FAIL ${c.id.padEnd(4)} ${c.title}\n       - setup: ${e.message}`);
      } finally {
        stopFixture();
        if (scratch) await scratch.stop();
      }
    }
  } catch (e) {
    setupError = e;
  } finally {
    await cleanup();
  }
  if (setupError) die(setupError.message);

  const scored = results.filter((r) => r.state !== "skip");
  const passed = scored.filter((r) => r.state === "pass").length;
  const ratio = scored.length ? passed / scored.length : 0;
  const bar = spec.pass_bar?.overall ?? 0.9;
  const criticalFailed = (spec.pass_bar?.critical ?? []).filter((id) => results.some((r) => r.id === id && r.state === "fail"));
  const ok = scored.length > 0 && ratio >= bar && criticalFailed.length === 0;
  console.log(`\nscore ${passed}/${scored.length} (${(ratio * 100).toFixed(0)}%), bar ${(bar * 100).toFixed(0)}%, skipped ${results.length - scored.length}`);
  if (criticalFailed.length) console.log(`critical cases failed (must be 100%): ${criticalFailed.join(", ")}`);
  console.log(ok ? "RESULT: PASS" : "RESULT: FAIL");
  if (args.report) fs.writeFileSync(args.report, JSON.stringify({ run: runId, agent: agentUrl, model: args.model, passed, scored: scored.length, ratio, bar, criticalFailed, results }, null, 2));
  if (scratch) console.log(`logs and fixtures: ${dir}`);
  process.exit(ok ? 0 : 1);
}

main().catch((e) => die(e.stack ?? String(e)));
