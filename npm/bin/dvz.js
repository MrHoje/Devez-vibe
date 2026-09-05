#!/usr/bin/env node

import { existsSync, readFileSync } from "node:fs";
import { spawn } from "node:child_process";
import { dirname, isAbsolute, join } from "node:path";
import { fileURLToPath } from "node:url";

const packageExecutable = join(dirname(fileURLToPath(import.meta.url)), "dvz.exe");
const pointer = process.env.LOCALAPPDATA
  ? join(process.env.LOCALAPPDATA, "DevezVibe", "current-executable.txt")
  : "";

let executable = packageExecutable;
try {
  const candidate = pointer ? readFileSync(pointer, "utf8").trim() : "";
  if (isAbsolute(candidate) && existsSync(candidate)) executable = candidate;
} catch {
  // A missing or incomplete pointer falls back to the npm-packaged executable.
}

const child = spawn(executable, process.argv.slice(2), { stdio: "inherit" });
child.on("error", (error) => {
  console.error(`Devez Vibe 실행 실패: ${error.message}`);
  process.exit(1);
});
child.on("exit", (code, signal) => {
  if (signal) process.kill(process.pid, signal);
  else process.exit(code ?? 1);
});

for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"]) {
  process.on(signal, () => {
    if (!child.killed) child.kill(signal);
  });
}
