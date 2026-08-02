// koto BetterWright sidecar: NDJSON over stdio, one `run` = one bw.run().
// Protocol output goes to stdout only; diagnostics to stderr only.
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
    return await import("betterwright");
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
      return await import(pathToFileURL(requireFrom(dir).resolve("betterwright")).href);
    } catch (e) {
      console.error("koto sidecar: resolve from", dir, "failed:", e && e.message);
    }
  }
  return null;
}

const mod = await loadBetterWright();
if (!mod) {
  send({ id: 0, ok: false, error: "module-not-found" });
  process.exit(1);
}
send({ id: 0, event: "ready", protocol: 1 });

const BetterWright = mod.BetterWright ?? mod.default;
let bw = null;

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

const rl = readline.createInterface({ input: process.stdin });
for await (const line of rl) {
  if (!line.trim()) continue;
  let id = -1;
  try {
    const msg = JSON.parse(line);
    id = msg.id;
    if (msg.op === "init") {
      const opts = {};
      if (msg.profile != null) opts.profile = msg.profile;
      if (msg.session != null) opts.session = msg.session;
      bw = new BetterWright(opts);
      send({ id, ok: true });
    } else if (msg.op === "run") {
      const env = await bw.run(msg.code, {
        timeout: msg.timeout_ms,
        ...(msg.approved_downloads ? { approvedDownloads: msg.approved_downloads } : {}),
      });
      send({
        id,
        ok: env.ok !== false,
        result: env.result ?? null,
        artifacts: normalizeArtifacts(env.artifacts),
        challenges: env.challenges ?? [],
        error: env.error ?? null,
        duration_ms: env.durationMs ?? null,
      });
    } else if (msg.op === "close") {
      try {
        if (bw) await bw.close();
      } catch (e) {
        console.error("koto sidecar: close failed:", e && e.message);
      }
      send({ id, ok: true });
      process.exit(0);
    } else {
      send({ id, ok: false, error: "unknown-op" });
    }
  } catch (e) {
    send({ id: id ?? -1, ok: false, error: String((e && e.message) || e) });
  }
}
