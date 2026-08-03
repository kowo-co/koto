// koto BetterWright sidecar: NDJSON request/response.
//
// Two modes. Without arguments it speaks over stdio and dies with its parent
// (used by the unit tests). With `--listen <socket>` it becomes a daemon: one
// BetterWright instance, and therefore one live browser with its pages, kept
// alive across koto invocations the way the CDP engine's session holder keeps
// its browser. Clients connect, drive, disconnect; only `op:"shutdown"` (or a
// signal) tears the browser down.
//
// Protocol output goes to the transport only; diagnostics to stderr only.
import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";
import { execSync } from "node:child_process";
import readline from "node:readline";

const send = (x) => process.stdout.write(JSON.stringify(x) + "\n");

function requireFrom(dir) {
  return createRequire(dir.endsWith("/") ? dir : dir + "/");
}

async function loadBetterWright() {
  try {
    const mod = await import("betterwright");
    return { mod, version: versionOf(createRequire(import.meta.url)) };
  } catch (e) {
    console.error("koto sidecar: plain import failed:", e && e.message);
  }
  const dirs = [];
  if (process.env.KOTO_BETTERWRIGHT_DIR) {
    dirs.push(process.env.KOTO_BETTERWRIGHT_DIR);
  } else {
    try {
      dirs.push(execSync("npm root -g", { encoding: "utf8" }).trim());
    } catch (e) {
      console.error("koto sidecar: npm root -g failed:", e && e.message);
    }
  }
  for (const dir of dirs) {
    if (!dir) continue;
    try {
      const req = requireFrom(dir);
      const mod = await import(pathToFileURL(req.resolve("betterwright")).href);
      return { mod, version: versionOf(req) };
    } catch (e) {
      console.error("koto sidecar: resolve from", dir, "failed:", e && e.message);
    }
  }
  return null;
}

function versionOf(req) {
  try {
    return req("betterwright/package.json").version ?? null;
  } catch {
    return null;
  }
}

const loaded = await loadBetterWright();
if (!loaded) {
  send({ id: 0, ok: false, error: "module-not-found" });
  process.exit(1);
}

const BetterWright = loaded.mod.BetterWright ?? loaded.mod.default;
let bw = null;
// Sessions are a per-run() lane, not a constructor option; remember the name
// chosen at init and stamp it onto every run and session-scoped host call.
let session = null;

// Host-object methods `op:"call"` may reach. Everything else on the client —
// vault owner APIs above all — stays out of the protocol on purpose.
const HOST_METHODS = new Set([
  "startLiveView",
  "stopLiveView",
  "liveViewStatus",
  "waitForHandoff",
  "waitForAsk",
  "liveViewPostChat",
  "liveViewDrainChat",
  "closeSession",
]);
const SESSION_SCOPED = new Set(["waitForHandoff", "waitForAsk"]);

const listenAt = (() => {
  const i = process.argv.indexOf("--listen");
  return i >= 0 ? process.argv[i + 1] : null;
})();

function normalizeArtifacts(artifacts) {
  if (!Array.isArray(artifacts)) return [];
  const out = [];
  for (const a of artifacts) {
    let p = null;
    if (typeof a === "string") p = a;
    else if (a && typeof a === "object") p = a.path ?? a.media ?? null;
    if (typeof p !== "string" || !p) continue;
    out.push(p.startsWith("MEDIA:") ? p.slice(6) : p);
  }
  return out;
}

async function shutdown() {
  try {
    if (bw) await bw.close();
  } catch (e) {
    console.error("koto sidecar: close failed:", e && e.message);
  }
}

async function handle(msg, reply) {
  const id = msg.id;
  if (msg.op === "init") {
    if (bw) {
      // Re-attaching to a live daemon must not discard its pages.
      if (msg.session != null) session = msg.session;
      reply({ id, ok: true, version: loaded.version, reused: true });
      return;
    }
    const opts = {};
    if (msg.profile != null) opts.profile = msg.profile;
    if (msg.platform != null) opts.platform = msg.platform;
    if (msg.session != null) session = msg.session;
    bw = new BetterWright(opts);
    reply({ id, ok: true, version: loaded.version, reused: false });
  } else if (msg.op === "run") {
    const env = await bw.run(msg.code, {
      timeout: msg.timeout_ms,
      ...(session != null ? { session } : {}),
      ...(msg.approved_downloads ? { approvedDownloads: msg.approved_downloads } : {}),
    });
    reply({
      id,
      ok: env.ok !== false,
      result: env.result ?? null,
      artifacts: normalizeArtifacts(env.artifacts),
      challenges: env.challenges ?? [],
      warnings: env.warnings ?? [],
      error: env.error ?? null,
      duration_ms: env.durationMs ?? null,
    });
  } else if (msg.op === "call") {
    if (!HOST_METHODS.has(msg.method)) {
      reply({ id, ok: false, error: `unknown-method: ${msg.method}` });
      return;
    }
    const params = { ...(msg.params ?? {}) };
    if (session != null && SESSION_SCOPED.has(msg.method) && params.session == null) {
      params.session = session;
    }
    if (msg.method === "closeSession") {
      const out = await bw.closeSession(params.session ?? session ?? undefined);
      reply({ id, ok: out.ok !== false, result: out, error: out.error ?? null });
      return;
    }
    const out = await bw[msg.method](params);
    reply({ id, ok: out?.ok !== false, result: out ?? null, error: out?.error ?? null });
  } else if (msg.op === "close" || msg.op === "shutdown") {
    // In daemon mode a client's `close` is only a disconnect — the browser is
    // shared. Only an explicit `shutdown` ends it.
    if (listenAt && msg.op === "close") {
      reply({ id, ok: true, kept: true });
      return;
    }
    // Acknowledge before tearing down — the caller is waiting on this line,
    // and browser close takes seconds.
    reply({ id, ok: true });
    if (listenAt) {
      const fs = await import("node:fs");
      try {
        fs.unlinkSync(listenAt);
      } catch {}
    }
    await shutdown();
    process.exit(0);
  } else {
    reply({ id, ok: false, error: "unknown-op" });
  }
}

function serve(socket, reply) {
  const lines = readline.createInterface({ input: socket });
  lines.on("line", async (line) => {
    if (!line.trim()) return;
    let id = -1;
    try {
      const msg = JSON.parse(line);
      id = msg.id;
      await handle(msg, reply);
    } catch (e) {
      reply({ id: id ?? -1, ok: false, error: String((e && e.message) || e) });
    }
  });
  return lines;
}

if (listenAt) {
  const net = await import("node:net");
  const fs = await import("node:fs");
  try {
    fs.unlinkSync(listenAt);
  } catch {}
  const server = net.createServer((socket) => {
    socket.on("error", () => {});
    serve(socket, (x) => {
      if (socket.writable) socket.write(JSON.stringify(x) + "\n");
    });
  });
  server.on("error", (e) => {
    console.error("koto sidecar: listen failed:", e && e.message);
    process.exit(1);
  });
  server.listen(listenAt, () => {
    try {
      fs.chmodSync(listenAt, 0o600);
    } catch {}
    // Readiness goes to stdout so the spawning koto can wait for it.
    send({ id: 0, event: "ready", protocol: 2, version: loaded.version, listening: listenAt });
  });
  // Stop accepting and drop the socket path BEFORE closing the browser:
  // browser teardown takes seconds, and a koto that connects during it would
  // get a connection that dies mid-request instead of cleanly starting a new
  // daemon.
  const teardown = async () => {
    try {
      server.close();
    } catch {}
    try {
      fs.unlinkSync(listenAt);
    } catch {}
    await shutdown();
    process.exit(0);
  };
  for (const sig of ["SIGTERM", "SIGINT"]) {
    process.on(sig, teardown);
  }
} else {
  send({ id: 0, event: "ready", protocol: 2, version: loaded.version });
  serve(process.stdin, send);
}
