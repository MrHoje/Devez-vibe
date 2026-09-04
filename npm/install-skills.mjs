import {
  copyFileSync,
  existsSync,
  mkdirSync,
  readdirSync,
  rmSync,
} from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const packageRoot = dirname(fileURLToPath(import.meta.url));
const skillNames = ["luna-loop", "insane-search"];
const userHome = homedir();
const codexHome = process.env.CODEX_HOME?.trim() || join(userHome, ".codex");
const claudeHome = process.env.CLAUDE_CONFIG_DIR?.trim() || join(userHome, ".claude");

const homes = [
  { name: "Codex", root: join(codexHome, "skills") },
  { name: "Claude", root: join(claudeHome, "skills") },
];

function copyTree(source, destination) {
  mkdirSync(destination, { recursive: true });
  for (const entry of readdirSync(source, { withFileTypes: true })) {
    const sourcePath = join(source, entry.name);
    const destinationPath = join(destination, entry.name);
    if (entry.isDirectory()) {
      copyTree(sourcePath, destinationPath);
    } else if (entry.isFile()) {
      copyFileSync(sourcePath, destinationPath);
    }
  }
}

// 덮어쓰기만 하면 원본에서 없앤 참고 문서가 설치본에 남아 계속 읽힌다.
// 복사한 뒤 원본에 없는 항목만 지운다. 복사가 먼저이므로 실패해도 설치본이 비지 않는다.
function pruneTree(source, destination) {
  for (const entry of readdirSync(destination, { withFileTypes: true })) {
    const sourcePath = join(source, entry.name);
    const destinationPath = join(destination, entry.name);
    if (!existsSync(sourcePath)) {
      rmSync(destinationPath, { recursive: true, force: true });
      continue;
    }
    if (entry.isDirectory()) pruneTree(sourcePath, destinationPath);
  }
}

let failed = false;
for (const skill of skillNames) {
  const sourceRoot = join(packageRoot, "skills", skill);
  // 원본이 없으면 그 스킬만 건너뛴다. 하나가 빠졌다고 나머지 설치까지 막지 않는다.
  if (!existsSync(join(sourceRoot, "SKILL.md"))) {
    failed = true;
    console.error(`스킬 원본(${skill})을 찾지 못했습니다: ${sourceRoot}`);
    continue;
  }
  for (const home of homes) {
    const target = join(home.root, skill);
    try {
      copyTree(sourceRoot, target);
      pruneTree(sourceRoot, target);
      console.log(`스킬 설치 완료 (${home.name}/${skill}): ${target}`);
    } catch (error) {
      failed = true;
      console.error(`스킬 설치 실패 (${home.name}/${skill}): ${error instanceof Error ? error.message : error}`);
    }
  }
}

if (failed) process.exitCode = 1;
