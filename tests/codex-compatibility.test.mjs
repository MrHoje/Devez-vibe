import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import test from "node:test";
import { fileURLToPath } from "node:url";

const script = new URL("../scripts/check-codex-compatibility.mjs", import.meta.url);

test("rejects a baseline without codexVersion", () => {
  const root = mkdtempSync(join(tmpdir(), "devez-compat-"));
  try {
    const baselineDir = join(root, "compatibility", "codex-app-server");
    mkdirSync(baselineDir, { recursive: true });
    writeFileSync(join(baselineDir, "baseline.json"), "{}\n");

    const result = spawnSync(process.execPath, [fileURLToPath(script), "--mode", "baseline"], {
      encoding: "utf8",
      env: { ...process.env, DEVEZ_COMPAT_ROOT: root },
    });

    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /baseline\.json must contain a non-empty codexVersion/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
