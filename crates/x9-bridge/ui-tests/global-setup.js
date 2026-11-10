// Starts x9-mock and x9-bridge on spare ports, as tests/e2e.py does, and
// stops both with SIGINT once every test has run. Then merges the JS
// coverage the tests saved and prints the table.

import { spawn } from "node:child_process";
import fs from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { PREVIEW_JPEG } from "./support/preview-jpeg.js";
import { TOKENS, ROLES } from "./support/tokens.js";
import { summarize, RAW_DIR } from "./support/coverage.js";

const here = path.dirname(fileURLToPath(import.meta.url));
const BIN = path.resolve(here, process.env.X9_BIN ?? "../../../target/debug");

function freePort() {
  return new Promise((resolve, reject) => {
    const s = net.createServer();
    s.unref();
    s.on("error", reject);
    s.listen(0, "127.0.0.1", () => {
      const { port } = s.address();
      s.close(() => resolve(port));
    });
  });
}

async function waitUp(url, child, what) {
  const deadline = Date.now() + 15_000;
  while (Date.now() < deadline) {
    if (child.exitCode != null) throw new Error(`${what} exited with ${child.exitCode} before it came up`);
    try {
      const r = await fetch(url);
      if (r.ok) return;
    } catch { /* not listening yet */ }
    await new Promise((r) => setTimeout(r, 100));
  }
  throw new Error(`${what} did not come up at ${url}`);
}

function stop(child) {
  if (child.exitCode != null || child.signalCode != null) return Promise.resolve();
  return new Promise((resolve) => {
    const t = setTimeout(() => { child.kill("SIGKILL"); }, 5000);
    child.once("exit", () => { clearTimeout(t); resolve(); });
    child.kill("SIGINT");
  });
}

export default async function globalSetup() {
  for (const b of ["x9-mock", "x9-bridge"]) {
    if (!fs.existsSync(path.join(BIN, b))) {
      throw new Error(`${path.join(BIN, b)} is missing: run \`cargo build -p x9-bridge -p x9-mock\` or set X9_BIN`);
    }
  }
  fs.rmSync(RAW_DIR, { recursive: true, force: true });
  fs.mkdirSync(RAW_DIR, { recursive: true });

  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "x9-ui-tests-"));
  const fixtures = path.join(tmp, "fixtures");
  const recipes = path.join(tmp, "recipes");
  fs.mkdirSync(fixtures);
  fs.mkdirSync(recipes);
  // Previews only: with no .tzf the bridge can't decode points, as with a
  // scan file it has not downloaded.
  fs.writeFileSync(path.join(fixtures, "dome.jpg"), PREVIEW_JPEG);
  fs.writeFileSync(path.join(fixtures, "area.jpg"), PREVIEW_JPEG);
  fs.copyFileSync(path.join(here, "../tests/recipes/window-measurement.toml"), path.join(recipes, "window-measurement.toml"));
  for (const f of fs.readdirSync(path.join(here, "recipes"))) fs.copyFileSync(path.join(here, "recipes", f), path.join(recipes, f));
  const tokfile = path.join(tmp, "tokens");
  fs.writeFileSync(tokfile, "# name role token\n" + Object.entries(TOKENS).map(([who, t]) => `${who}-1 ${ROLES[who] ?? who} ${t}`).join("\n") + "\n");

  const [mockPort, bridgePort] = [await freePort(), await freePort()];
  const quiet = process.env.X9_UI_LOGS ? "inherit" : "ignore";
  const mock = spawn(path.join(BIN, "x9-mock"), [], {
    env: { ...process.env, X9_MOCK_ADDR: `127.0.0.1:${mockPort}`, X9_MOCK_SPEED: "20", X9_MOCK_CANCEL_LINGER: "4", X9_MOCK_FIXTURES: fixtures },
    stdio: ["ignore", quiet, quiet],
  });
  await waitUp(`http://127.0.0.1:${mockPort}/api/v1/status`, mock, "x9-mock");
  const bridge = spawn(path.join(BIN, "x9-bridge"), [], {
    env: {
      ...process.env, X9_BASE: `http://127.0.0.1:${mockPort}/api/v1`, BRIDGE_ADDR: `127.0.0.1:${bridgePort}`,
      BRIDGE_TOKENS: tokfile, BRIDGE_POLL_MS: "200", BRIDGE_DATA: path.join(tmp, "data"), BRIDGE_RECIPES: recipes,
    },
    stdio: ["ignore", quiet, quiet],
  });
  await waitUp(`http://127.0.0.1:${bridgePort}/v1/health`, bridge, "x9-bridge");

  // Workers read these; the tests talk to the bridge only.
  process.env.X9_UI_BASE = `http://127.0.0.1:${bridgePort}`;

  return async () => {
    await stop(bridge);
    await stop(mock);
    fs.rmSync(tmp, { recursive: true, force: true });
    summarize();
  };
}
