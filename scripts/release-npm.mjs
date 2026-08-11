#!/usr/bin/env node
// Builds the release binary and stages the npm package under `npm/`.
//
//   node scripts/release-npm.mjs                    # build + stage + `npm publish --dry-run`
//   node scripts/release-npm.mjs --publish          # the above, then a real publish
//   node scripts/release-npm.mjs --publish --otp=123456
//   node scripts/release-npm.mjs --skip-build
//
// npm requires 2FA to publish: pass `--otp` with an authenticator code, or store
// a granular access token that has "bypass 2FA" enabled.
//
// `Cargo.toml` is the single source of truth for the version; `npm/package.json`
// is rewritten to match on every run.

import { spawnSync } from "node:child_process";
import { copyFileSync, existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const npmDir = join(root, "npm");
const manifestPath = join(npmDir, "package.json");
const binaryPath = join(root, "target", "release", "dvz.exe");
const stagedBinary = join(npmDir, "bin", "dvz.exe");

const argv = process.argv.slice(2);
const args = new Set(argv);
const publish = args.has("--publish");
const skipBuild = args.has("--skip-build");
const otp = argv.find((arg) => arg.startsWith("--otp="))?.slice("--otp=".length);

function fail(message) {
  console.error(`\n✕ ${message}`);
  process.exit(1);
}

function run(command, commandArgs, cwd) {
  console.log(`\n$ ${command} ${commandArgs.join(" ")}`);
  const result = spawnSync(command, commandArgs, { cwd, stdio: "inherit", shell: true });
  if (result.status !== 0) {
    fail(`\`${command} ${commandArgs.join(" ")}\` 실패 (exit ${result.status}).`);
  }
}

function cargoVersion() {
  const cargo = readFileSync(join(root, "Cargo.toml"), "utf8").replace(/^\uFEFF/, "");
  const packageSection = cargo.split(/^\[/m)[1] ?? "";
  const match = packageSection.match(/^version\s*=\s*"([^"]+)"/m);
  if (!match) {
    fail("Cargo.toml의 [package] 섹션에서 version을 찾지 못했습니다.");
  }
  return match[1];
}

/** True when this exact version is already on the registry. */
function alreadyPublished(name, version) {
  const result = spawnSync("npm", ["view", `${name}@${version}`, "version"], {
    encoding: "utf8",
    shell: true,
  });
  return result.status === 0 && result.stdout.trim() === version;
}

const version = cargoVersion();
const manifest = JSON.parse(readFileSync(manifestPath, "utf8").replace(/^\uFEFF/, ""));

if (manifest.version !== version) {
  manifest.version = version;
  writeFileSync(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`);
  console.log(`✓ npm/package.json 버전을 ${version}로 동기화했습니다.`);
} else {
  console.log(`✓ 버전 ${version} (Cargo.toml ↔ npm/package.json 일치)`);
}

if (!skipBuild) {
  run("cargo", ["build", "--release"], root);
}
if (!existsSync(binaryPath)) {
  fail(`빌드 결과물이 없습니다: ${binaryPath}`);
}

mkdirSync(join(npmDir, "bin"), { recursive: true });
copyFileSync(binaryPath, stagedBinary);
copyFileSync(join(root, "LICENSE"), join(npmDir, "LICENSE"));
console.log(`✓ ${stagedBinary} 준비 완료`);

if (!publish) {
  run("npm", ["publish", "--dry-run"], npmDir);
  console.log(
    `\n다음 단계: npm login 후 \`node scripts/release-npm.mjs --publish\`\n` +
      `설치 확인:   npm i -g ${manifest.name} && dvz --version`,
  );
  process.exit(0);
}

if (alreadyPublished(manifest.name, version)) {
  fail(
    `${manifest.name}@${version}은 이미 배포되어 있습니다.\n` +
      `  Cargo.toml의 version을 올린 뒤 다시 실행하세요 (npm은 같은 버전 재배포를 허용하지 않습니다).`,
  );
}

const publishArgs = ["publish", "--access", "public"];
if (otp) {
  publishArgs.push("--otp", otp);
}
run("npm", publishArgs, npmDir);
console.log(
  `\n✓ ${manifest.name}@${version} 배포 완료\n` +
    `확인: npm view ${manifest.name} version\n` +
    `설치: npm i -g ${manifest.name} && dvz --version`,
);
