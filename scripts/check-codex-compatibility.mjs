#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  existsSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, relative, sep } from "node:path";
import { fileURLToPath } from "node:url";

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const artifactDir = join(root, "artifacts", "codex-compatibility");
const baselinePath = join(artifactDir, "schema.ts");
const baselineVersionPath = join(artifactDir, "baseline-version.txt");
const currentPath = join(artifactDir, "schema-current.ts");
const reportPath = join(artifactDir, "compatibility-report.md");
const npm = process.platform === "win32" ? "npm.cmd" : "npm";

function usage() {
  console.log(`사용법:
  node scripts/check-codex-compatibility.mjs --mode latest [--update-baseline]
  node scripts/check-codex-compatibility.mjs --mode fixed --version <버전> [--update-baseline]

종료 코드: 일치 또는 기준 갱신 0, 스키마 불일치 1, 실행 오류 2`);
}

function fail(message) {
  console.error(`오류: ${message}`);
  process.exit(2);
}

function isVersion(value) {
  return /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(value);
}

function run(command, args) {
  const environment = { ...process.env, NO_COLOR: "1" };
  delete environment.FORCE_COLOR;
  const executable = process.platform === "win32" ? process.env.ComSpec || "cmd.exe" : command;
  const executableArgs = process.platform === "win32" ? ["/d", "/s", "/c", command, ...args] : args;
  const result = spawnSync(executable, executableArgs, {
    cwd: root,
    encoding: "utf8",
    env: environment,
    windowsHide: true,
  });
  if (result.error) {
    fail(`${command} 실행 실패: ${result.error.message}`);
  }
  if (result.status !== 0) {
    const detail = (result.stderr || result.stdout).trim();
    fail(`${command} ${args.join(" ")} 실패 (종료 코드 ${result.status})${detail ? `\n${detail}` : ""}`);
  }
  return result.stdout.trim();
}

function parseArgs(argv) {
  let mode;
  let version;
  let updateBaseline = false;

  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--mode") {
      mode = argv[++index];
    } else if (arg === "--version") {
      version = argv[++index];
    } else if (arg === "--update-baseline") {
      updateBaseline = true;
    } else if (arg === "--help" || arg === "-h") {
      usage();
      process.exit(0);
    } else {
      fail(`알 수 없는 인수: ${arg}`);
    }
  }

  if (mode !== "latest" && mode !== "fixed") {
    fail("--mode는 latest 또는 fixed여야 합니다.");
  }
  if (mode === "fixed" && !version) {
    fail("fixed 모드에는 --version이 필요합니다.");
  }
  if (mode === "latest" && version) {
    fail("latest 모드에는 --version을 함께 지정할 수 없습니다.");
  }
  if (version && !isVersion(version)) {
    fail(`지원하지 않는 버전 형식: ${version}`);
  }

  return { mode, version, updateBaseline };
}

function filesRecursively(directory) {
  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const path = join(directory, entry.name);
    return entry.isDirectory() ? filesRecursively(path) : [path];
  });
}

function combineTypescript(directory) {
  const files = filesRecursively(directory)
    .filter((path) => path.endsWith(".ts"))
    .map((path) => ({ path, relativePath: relative(directory, path).split(sep).join("/") }))
    .sort((left, right) =>
      left.relativePath < right.relativePath ? -1 : left.relativePath > right.relativePath ? 1 : 0,
    );

  if (files.length === 0) {
    fail("Codex가 TypeScript 스키마 파일을 생성하지 않았습니다.");
  }

  const schema = files
    .map(
      ({ path, relativePath }) =>
        `// ${relativePath}\n${readFileSync(path, "utf8").replaceAll("\r\n", "\n").trimEnd()}\n\n`,
    )
    .join("")
    .slice(0, -1);

  return { schema, fileNames: files.map(({ relativePath }) => relativePath) };
}

function schemaFileNames(schema) {
  return [...schema.matchAll(/^\/\/ (.+\.ts)$/gm)].map((match) => match[1]);
}

function hash(content) {
  return createHash("sha256").update(content).digest("hex");
}

function listDifference(left, right) {
  const rightSet = new Set(right);
  return left.filter((item) => !rightSet.has(item));
}

function renderList(items) {
  return items.length === 0 ? "none" : items.map((item) => `\`${item}\``).join(", ");
}

const { mode, version: requestedVersion, updateBaseline } = parseArgs(process.argv.slice(2));
const targetVersion =
  mode === "latest" ? run(npm, ["view", "@openai/codex", "version"]) : requestedVersion;
if (!isVersion(targetVersion)) {
  fail(`npm에서 지원하지 않는 버전 형식을 받았습니다: ${targetVersion}`);
}
const packageSpec = `@openai/codex@${targetVersion}`;
const cliVersionOutput = run(npm, ["exec", "--yes", `--package=${packageSpec}`, "--", "codex", "--version"]);
const cliVersion = cliVersionOutput.match(/codex-cli\s+(\S+)/)?.[1];
if (!cliVersion) {
  fail(`Codex 버전을 해석하지 못했습니다: ${cliVersionOutput}`);
}
if (cliVersion !== targetVersion) {
  fail(`요청 버전 ${targetVersion}과 실행 버전 ${cliVersion}이 다릅니다.`);
}

const temporaryDirectory = mkdtempSync(join(tmpdir(), "devez-codex-compat-"));
let generated;
try {
  run(npm, [
    "exec",
    "--yes",
    `--package=${packageSpec}`,
    "--",
    "codex",
    "app-server",
    "generate-ts",
    "--experimental",
    "--out",
    temporaryDirectory,
  ]);
  generated = combineTypescript(temporaryDirectory);
} finally {
  rmSync(temporaryDirectory, { recursive: true, force: true });
}

const baseline = existsSync(baselinePath)
  ? readFileSync(baselinePath, "utf8").replaceAll("\r\n", "\n")
  : "";
const previousVersion = existsSync(baselineVersionPath)
  ? readFileSync(baselineVersionPath, "utf8").trim()
  : "unknown";
const matches = generated.schema === baseline;
const addedFiles = listDifference(generated.fileNames, schemaFileNames(baseline));
const removedFiles = listDifference(schemaFileNames(baseline), generated.fileNames);

let result;
let baselineVersion = previousVersion;
let diffArtifact = "none";
let nextAction;

if (updateBaseline) {
  writeFileSync(baselinePath, generated.schema, "utf8");
  writeFileSync(baselineVersionPath, `${cliVersion}\n`, "utf8");
  if (existsSync(currentPath)) unlinkSync(currentPath);
  result = matches ? "schema matches; baseline version refreshed" : "baseline updated";
  baselineVersion = cliVersion;
  nextAction = "호환성 기준 갱신 완료";
} else if (matches) {
  if (existsSync(currentPath)) unlinkSync(currentPath);
  result = "schema matches";
  nextAction = "호환";
} else {
  writeFileSync(currentPath, generated.schema, "utf8");
  result = "schema differs";
  diffArtifact = "artifacts/codex-compatibility/schema-current.ts";
  nextAction = "스키마 차이 검토 후 --update-baseline 실행";
}

const reproduce =
  mode === "latest"
    ? "node scripts/check-codex-compatibility.mjs --mode latest"
    : `node scripts/check-codex-compatibility.mjs --mode fixed --version ${targetVersion}`;
const report = `# Codex CLI Compatibility Report

- Mode: ${mode}
- Target Codex CLI: ${cliVersion}
- Baseline Codex CLI: ${baselineVersion}
- Result: ${result}
- Baseline SHA-256: ${hash(updateBaseline ? generated.schema : baseline)}
- Current SHA-256: ${hash(generated.schema)}
- Added generated files: ${renderList(addedFiles)}
- Removed generated files: ${renderList(removedFiles)}
- Diff artifact: ${diffArtifact}
- Next action: ${nextAction}
- Reproduce: ${reproduce}
`;
writeFileSync(reportPath, report, "utf8");

console.log(`Codex CLI ${cliVersion}: ${result}`);
console.log(`보고서: ${relative(root, reportPath)}`);
if (!updateBaseline && !matches) {
  console.error("스키마가 기준과 다릅니다. 종료 코드 1을 반환합니다.");
  process.exit(1);
}
