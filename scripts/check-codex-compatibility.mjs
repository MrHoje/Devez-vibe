import {
  cpSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { dirname, join, relative, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";

const root = resolve(process.env.DEVEZ_COMPAT_ROOT ?? dirname(dirname(fileURLToPath(import.meta.url))));
const compatibilityDir = join(root, "compatibility", "codex-app-server");
const baselinePath = join(compatibilityDir, "baseline.json");
const baselineSchemaPath = join(compatibilityDir, "schema.ts");
const artifactDir = join(root, "artifacts", "codex-compatibility");
const args = process.argv.slice(2);
const modeIndex = args.indexOf("--mode");
const mode = modeIndex === -1 ? undefined : args[modeIndex + 1];
const updateBaseline = args.includes("--update-baseline");

function fail(message) {
  console.error(message);
  process.exit(1);
}

if (
  modeIndex !== 0 ||
  !["baseline", "latest"].includes(mode) ||
  args.length !== (updateBaseline ? 3 : 2) ||
  (updateBaseline && args[2] !== "--update-baseline")
) {
  fail("Usage: node scripts/check-codex-compatibility.mjs --mode baseline|latest [--update-baseline]");
}
if (updateBaseline && mode !== "baseline") {
  fail("--update-baseline is only valid with --mode baseline");
}

function readBaseline() {
  if (!existsSync(baselinePath)) {
    if (updateBaseline) return null;
    fail(`Missing baseline metadata: ${baselinePath}`);
  }
  let baseline;
  try {
    baseline = JSON.parse(readFileSync(baselinePath, "utf8"));
  } catch {
    fail("baseline.json must be valid JSON");
  }
  if (typeof baseline.codexVersion !== "string" || baseline.codexVersion.trim() === "") {
    fail("baseline.json must contain a non-empty codexVersion");
  }
  return baseline;
}

function run(command, commandArgs) {
  const result = spawnSync(command, commandArgs, { encoding: "utf8", shell: process.platform === "win32" });
  if (result.status !== 0) {
    throw new Error(`${command} ${commandArgs.join(" ")} failed: ${(result.stderr || result.stdout || result.error?.message || "no output").trim()}`);
  }
  return result.stdout.trim();
}

function codexCommand(version) {
  return version
    ? ["npm", ["exec", "--yes", `--package=@openai/codex@${version}`, "--", "codex"]]
    : ["npm", ["exec", "--yes", "--package=@openai/codex", "--", "codex"]];
}

function listTypeScriptFiles(directory) {
  return readdirSync(directory, { withFileTypes: true })
    .flatMap((entry) => {
      const path = join(directory, entry.name);
      return entry.isDirectory() ? listTypeScriptFiles(path) : entry.name.endsWith(".ts") ? [path] : [];
    })
    .sort();
}

function combinedSchema(directory) {
  const files = listTypeScriptFiles(directory);
  if (files.length === 0) throw new Error("Codex generated no TypeScript schema files");
  return files
    .map((file) => `// ${relative(directory, file).replaceAll("\\", "/")}\n${readFileSync(file, "utf8").trimEnd()}\n`)
    .join("\n");
}

function writeReport(lines) {
  mkdirSync(artifactDir, { recursive: true });
  writeFileSync(join(artifactDir, "compatibility-report.md"), `# Codex CLI Compatibility Report\n\n${lines.join("\n")}\n`);
}

const baseline = readBaseline();
const requestedVersion = mode === "baseline" && !updateBaseline ? baseline?.codexVersion : undefined;
const [runner, runnerPrefix] = codexCommand(requestedVersion);
const tempDir = mkdtempSync(join(tmpdir(), "devez-codex-schema-"));

try {
  const installedVersion = run(runner, [...runnerPrefix, "--version"]);
  run(runner, [...runnerPrefix, "app-server", "generate-ts", "--experimental", "--out", tempDir]);
  const generatedSchema = combinedSchema(tempDir);
  mkdirSync(artifactDir, { recursive: true });
  writeFileSync(join(artifactDir, "schema.ts"), generatedSchema);

  if (updateBaseline) {
    mkdirSync(compatibilityDir, { recursive: true });
    writeFileSync(baselineSchemaPath, generatedSchema);
    writeFileSync(
      baselinePath,
      `${JSON.stringify({ codexVersion: installedVersion.replace(/^codex-cli\s+/, ""), generatedAt: new Date().toISOString().slice(0, 10), command: "codex app-server generate-ts --experimental --out <output-dir>" }, null, 2)}\n`,
    );
    writeReport([`- Mode: ${mode}`, `- Installed Codex CLI: ${installedVersion}`, "- Result: baseline updated", "- Next action: commit the reviewed baseline snapshot"]);
    process.exit(0);
  }

  const expectedSchema = readFileSync(baselineSchemaPath, "utf8");
  const equal = generatedSchema === expectedSchema;
  if (!equal) {
    const diff = spawnSync("git", ["diff", "--no-index", "--", baselineSchemaPath, join(artifactDir, "schema.ts")], { encoding: "utf8" });
    writeFileSync(join(artifactDir, "schema.diff"), diff.stdout || diff.stderr);
  }
  writeReport([
    `- Mode: ${mode}`,
    `- Installed Codex CLI: ${installedVersion}`,
    `- Baseline Codex CLI: ${baseline.codexVersion}`,
    `- Result: ${equal ? "schema matches" : "schema changed"}`,
    `- Diff artifact: ${equal ? "none" : "schema.diff"}`,
    `- Next action: ${equal ? "호환" : "코드 수정 필요 또는 기준 스냅샷 갱신 필요"}`,
    `- Reproduce: node scripts/check-codex-compatibility.mjs --mode ${mode}`,
  ]);
  if (!equal) process.exitCode = 1;
} catch (error) {
  writeReport([`- Mode: ${mode}`, `- Baseline Codex CLI: ${baseline?.codexVersion ?? "not created"}`, "- Result: check failed", `- Error: ${error.message}`, `- Reproduce: node scripts/check-codex-compatibility.mjs --mode ${mode}`]);
  console.error(error.message);
  process.exitCode = 1;
} finally {
  rmSync(tempDir, { recursive: true, force: true });
}
