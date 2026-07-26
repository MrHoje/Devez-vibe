import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

test("workflow checks pull requests and weekly upstream changes", () => {
  const workflow = readFileSync(".github/workflows/codex-compatibility.yml", "utf8");

  assert.match(workflow, /pull_request:/);
  assert.match(workflow, /cron: '0 9 \* \* 1'/);
  assert.match(workflow, /workflow_dispatch:/);
  assert.match(workflow, /if: always\(\)/);
  assert.match(workflow, /retention-days: 30/);
});

test("operations guide explains baseline and latest checks", () => {
  const guide = readFileSync("docs/codex-cli-compatibility.md", "utf8");

  assert.match(guide, /--mode baseline/);
  assert.match(guide, /--mode latest/);
  assert.match(guide, /artifacts\/codex-compatibility/);
  assert.match(guide, /호환/);
  assert.match(guide, /코드 수정 필요/);
  assert.match(guide, /기준 스냅샷 갱신 필요/);
});
