// Opt-in live scenarios; never used by the product or ordinary test suite.
// Export fixtures with the Rust export_role_guide_fixtures test first.
// Usage: node scripts/test-role-guides.mjs FIXTURES_JSON [case1,case2] [codex,claude]
// DEVEZ_CODEX_BIN points at the native Codex executable. All task changes stay
// in fresh temporary Git repositories. Full prompts/events/results are saved.
import { spawn, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdirSync, mkdtempSync, readFileSync, readlinkSync, readdirSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";

const repo = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const fixtures = JSON.parse(readFileSync(process.argv[2], "utf8"));
const rescoreRoot = process.argv[3] === "--rescore" ? resolve(process.argv[4]) : null;
const selected = rescoreRoot ? null : process.argv[3]?.split(",");
const providers = process.argv[4]?.split(",") ?? ["codex", "claude"];
const out = rescoreRoot ?? mkdtempSync(join(tmpdir(), "devez-role-guides-live-"));
console.log(`EVIDENCE ${out}`);

const records = [
  ["planner", "ready", "가상 계획 목표는 저장 실패 시 입력값 보존이다. 주요 작업은 오류 처리와 재시도 검증이다. 요구사항과 구조 승인을 받았다. 독립 검토 1회, OKAY·CLEAR, 미확정 없음이다. 저장 경로는 docs/plans/2026-09-10-save-retry.md이다."],
  ["planner", "open", "가상 계획 목표는 저장 실패 시 입력값 보존이다. 주요 작업은 오류 처리와 재시도 검증이다. 자동 재시도 여부는 미확정이다. 검토 미실행, 계획 미저장, 실행 승인 없음이다."],
  ["goal-runner", "done", "승인된 입력값 보존 작업 2개를 완료했다. 최종 변경 뒤 단위 시험 12개와 저장 실패·재시도 실제 화면 검사가 통과했다. Light 수준, 독립 검토 1회 통과, 직접 구현했다. 미해결·미확인 없음. 커밋은 요청되지 않아 하지 않았다. 실행 공간은 C:/sandbox/save-retry, 근거는 .devezvibe/runs/save-retry/review.md이다."],
  ["goal-runner", "blocked", "저장 오류 처리 코드를 반영했고 단위 시험 12개는 통과했다. 운영 서버 인증 정보가 없어 실제 재시도 검사를 못 했다. 필수 통합 검토도 미실행이다. 실행 공간은 C:/sandbox/save-retry이며 커밋하지 않았다. 사용자에게 시험 계정이 필요하다."],
  ["reviewer", "clean", "src/save.js:2-5 변경을 승인 요구사항과 대조했다. 실패 후 입력값 유지 동작을 코드로 확인하고 단위 시험 12개 통과 로그를 읽었다. 시험을 직접 실행하지는 않았다. 확인 범위에서 문제 없음. 이전 지적·별도 미확인 없음."],
  ["reviewer", "recheck", "문제 1은 src/save.js:3의 저장 실패 시 입력값 삭제이며 보통 등급이다. 수정 후에도 빈 문자열 대입이 남아 미해결이다. 사용자 입력값이 사라지므로 오류 분기에서 보존해야 한다. 문제 2는 오류 메시지 누락이며 src/save.js:5에서 표시됨을 확인해 해결됐다. 운영 서버 오류 응답은 미확인이고 시험 서버에서 500 응답 재현이 필요하다."],
];
const cases = records.map(([role, scenario, facts]) => ({
  id: `report-${role}-${scenario}`, role, scenario, report: true, expected: [],
  prompt: `최종 보고만 작성하세요. 아래는 제공된 시험 기록이며 당신이 실행한 작업으로 주장하지 마세요. 이 기록만으로 답하고 새 조사·파일 변경·도구 호출·승인 질문은 하지 마세요.\n\n${facts}`,
}));
cases.push(
  { id: "planner-interview", role: "planner", expected: ["requirements.md"], forbidden: ["plan.md", "review-handoff.md"],
    prompt: "src/save.js의 저장 실패 시 입력값을 보존하는 기존 기능 개선 계획을 세워줘. 성공 시 입력 비우기는 유지해. 자동 재시도 기능은 넣을지 아직 결정하지 못했어. 요구사항 요약 승인은 아직 하지 않았어." },
  { id: "planner-draft", role: "planner", expected: ["requirements.md", "plan.md", "review-handoff.md"],
    prompt: "기존 저장 흐름 개선 계획을 작성해줘. 확정 요구 요약: src/save.js의 저장이 실패하면 입력 문자열을 보존하고 원래 오류를 그대로 호출자에게 전달하며 성공하면 입력을 비운다. 재시도 추가·동시 편집·새 의존성은 범위 밖이다. test/save.test.js에서 이 세 결과를 검증한다. 이 요구사항 요약을 승인한다. 구현 방식은 기존 구조 내 최소 변경으로 위임한다. 계획 저장과 자체 검토까지 진행하되 구현·커밋·실행 승인은 하지 않는다." },
  { id: "reviewer-code", role: "reviewer", expected: ["change-review.md"], forbidden: ["plan-review.md", "re-review.md"], review: true,
    prompt: "현재 src/save.js의 미커밋 변경을 검토해줘. 요구사항은 저장 실패 시 사용자 입력 보존과 원래 오류 전달, 성공 시 입력 비우기야. 실제 diff와 코드를 확인해 판단해줘." },
  { id: "reviewer-plan", role: "reviewer", expected: ["plan-review.md"], forbidden: ["change-review.md", "re-review.md"], review: true,
    prompt: "docs/plans/save.md 계획만 검토해줘. 요구사항은 저장 실패 시 입력 보존·원래 오류 전달이고 성공 시 입력 비우기야. 저장된 계획과 구현을 읽고 계획대로 실행해도 요구사항을 만족하는지 판단해줘." },
  { id: "reviewer-recheck", role: "reviewer", expected: ["change-review.md", "re-review.md"], forbidden: ["plan-review.md"], review: true,
    prompt: "현재 미커밋 수정분을 재검토해줘. 이전 문제 1(보통)은 src/save.js에서 저장 실패 때 입력을 지우는 결함, 문제 2(경미)는 오류 메시지를 기록하지 않는 결함이야. 요구사항은 실패 시 입력 보존과 원래 오류 전달, 성공 시 입력 비우기야. 두 문제의 해결 여부와 수정분을 확인해줘." },
  { id: "planner-inline", role: "planner", expected: [], inline: true,
    prompt: "src/save.js의 저장 실패 시 입력 보존 계획을 세워줘. 성공 시 입력 비우기는 유지해. 자동 재시도는 넣을지 아직 결정하지 못했으며 요구사항 요약을 승인하지 않았어." },
  { id: "planner-missing", role: "planner", expected: ["requirements.md"], missing: true,
    prompt: "src/save.js의 저장 실패 시 입력 보존 계획을 세워줘. 성공 시 입력 비우기는 유지해. 요구사항 요약은 아직 승인하지 않았어." },
  { id: "goal-runner-implement", role: "goal-runner", expected: [], noGuides: true, implement: true,
    prompt: "src/save.js를 고쳐줘. 실패하면 입력 문자열을 보존하고 원래 오류를 호출자에게 그대로 전달해. 성공하면 입력을 비워. test/save.test.js가 세 조건의 검증 기준이야. 기존 구조에서 최소 수정하고 검사와 자체 검토까지 진행해. 재시도·동시 편집·새 의존성·커밋은 범위 밖이야. 이 범위의 구현과 검증을 승인해." },
  { id: "goal-runner-verdict", role: "goal-runner", expected: [], noGuides: true,
    prompt: "이전 작업의 검토 판정만 확인해줘. 검토 대상은 review.diff와 target.json이고 전달받은 판정은 verdict.json이야. 실제 파일을 읽고 검토를 통과로 받아들여 후속 의존 작업으로 넘어갈 수 있는지 판단해줘. 구현·파일 변경은 하지 마." },
  { id: "goal-runner-verdict-earlier", role: "goal-runner", expected: [], noGuides: true,
    prompt: "이전 문제들의 재검토 판정만 확인해줘. 이전 문제는 earlier.json, 현재 대상은 review.diff와 target.json, 받은 판정은 verdict.json이야. 실제 파일을 읽고 이 재검토를 통과로 받아들여 후속 의존 작업을 진행해도 되는지 판단해줘. 파일 변경은 하지 마." },
  { id: "goal-runner-cache-unavailable", role: "goal-runner", expected: [], noGuides: true, inline: true, implement: true,
    prompt: "src/save.js를 수정해 저장 실패 시 입력 문자열을 보존하고 원래 오류를 다시 던지게 해줘. 성공 시 입력 비우기는 유지하고 test/save.test.js로 확인해줘. 구현과 자체 검토를 승인하며 커밋은 하지 마." },
  { id: "planner-to-builder", role: "planner", expected: ["requirements.md"], forbidden: ["plan.md", "review-handoff.md"], implement: true, switchTo: "builder",
    prompt: "src/save.js의 저장 실패 시 입력을 보존하는 계획을 세워줘. 자동 재시도를 넣을지는 아직 결정하지 못했고 요구사항 요약은 승인하지 않았어.",
    nextPrompt: "Builder로 전환했어. 자동 재시도는 범위 밖으로 확정한다. 지금 src/save.js를 최소 수정해 저장 실패 시 입력을 보존하고 원래 오류를 그대로 전달하며 성공 시 입력을 비우게 해줘. test/save.test.js로 검증해줘. 이 구현을 승인하며 계획 작성·실행 승인 질문·커밋은 하지 마." },
  { id: "planner-to-goal-runner", role: "planner", expected: ["requirements.md"], forbidden: ["plan.md", "review-handoff.md"], implement: true, switchTo: "goal-runner",
    prompt: "src/save.js의 저장 실패 시 입력을 보존하는 계획을 세워줘. 자동 재시도를 넣을지는 아직 결정하지 못했고 요구사항 요약은 승인하지 않았어.",
    nextPrompt: "Goal Runner로 전환했어. 자동 재시도는 범위 밖으로 확정한다. 계획 파일 없이 현재 src/save.js를 최소 수정해 저장 실패 시 입력을 보존하고 원래 오류를 그대로 전달하며 성공 시 입력을 비우게 해줘. test/save.test.js로 검증하고 자체 검토해줘. 이 구현을 승인하며 계획 작성·추가 실행 승인 질문·커밋은 하지 마." },
);

if (selected) {
  const unknown = selected.filter(id => id !== "reports" && !cases.some(test => test.id === id));
  if (unknown.length) throw new Error(`Unknown scenario: ${unknown.join(", ")}`);
}

function git(cwd, ...args) {
  const result = spawnSync("git", args, { cwd, windowsHide: true, encoding: "utf8" });
  if (result.status !== 0) throw new Error(result.stderr || String(result.error));
  return result.stdout.trim();
}

function snapshot(cwd, prefix = "", result = {}) {
  for (const entry of readdirSync(join(cwd, prefix), { withFileTypes: true })) {
    if (!prefix && entry.name === ".git") continue;
    const path = prefix ? `${prefix}/${entry.name}` : entry.name;
    if (entry.isSymbolicLink()) result[path] = `link:${readlinkSync(join(cwd, path))}`;
    else if (entry.isDirectory()) snapshot(cwd, path, result);
    else if (entry.isFile()) result[path] = createHash("sha256").update(readFileSync(join(cwd, path))).digest("hex");
    else result[path] = "unsupported file type";
  }
  return result;
}

function prepare(test, provider) {
  const evidence = join(out, `${provider}-${test.id}`);
  const cwd = join(evidence, "workspace");
  mkdirSync(join(cwd, "src"), { recursive: true });
  mkdirSync(join(cwd, "test"));
  mkdirSync(join(cwd, "docs/plans"), { recursive: true });
  const correct = 'export async function save(state, send) {\n  const text = state.text;\n  await send(text);\n  state.text = "";\n}\n';
  const broken = 'export async function save(state, send) {\n  const text = state.text;\n  state.text = "";\n  try { await send(text); } catch (error) { console.error(error.message); throw error; }\n}\n';
  writeFileSync(join(cwd, "package.json"), '{"type":"module"}\n');
  writeFileSync(join(cwd, "src/save.js"), test.implement ? broken : correct);
  writeFileSync(join(cwd, "test/save.test.js"), 'import assert from "node:assert/strict";\nimport { save } from "../src/save.js";\nconst state = { text: "보존할 입력" };\nconst error = new Error("시험 오류");\nawait assert.rejects(save(state, async () => { throw error; }), e => e === error);\nassert.equal(state.text, "보존할 입력");\nawait save(state, async value => { assert.equal(value, "보존할 입력"); });\nassert.equal(state.text, "");\nconsole.log("3 assertions passed");\n');
  if (test.id === "reviewer-plan") {
    writeFileSync(join(cwd, "docs/plans/save.md"), '# 저장 입력 보존 계획\n\n목표: 실패 시 입력 보존·원래 오류 전달, 성공 시 입력 비우기.\n\n## 작업 1\n수정: src/save.js. save(state, send)에서 send(text) 호출 전에 state.text = ""로 입력을 비운다. 오류는 다시 던진다.\n\n검증: node test/save.test.js. 세 검사가 통과해야 한다.\n');
  }
  if (test.id.startsWith("goal-runner-verdict")) {
    writeFileSync(join(cwd, "review.diff"), "--- a/src/save.js\n+++ b/src/save.js\n@@ -1 +1 @@\n-old\n+new\n");
    const target = { package: join(cwd, "review.diff"), sha256: createHash("sha256").update(readFileSync(join(cwd, "review.diff"))).digest("hex") };
    writeFileSync(join(cwd, "target.json"), JSON.stringify(target));
    const verdict = { target: { ...target, sha256: "0".repeat(64) }, verdict: "APPROVE", blocking: 0, significant: 0, minor: 0, findings: [] };
    if (test.id.endsWith("earlier")) {
      verdict.target = target;
      verdict.earlier = [{ id: "issue-1", status: "ADDRESSED" }, { id: "issue-1", status: "ADDRESSED" }];
      writeFileSync(join(cwd, "earlier.json"), JSON.stringify([{ id: "issue-1", severity: "blocking", summary: "failed saves discard input" }, { id: "issue-2", severity: "significant", summary: "save failures lose the original error" }]));
    }
    writeFileSync(join(cwd, "verdict.json"), JSON.stringify(verdict));
  }
  git(cwd, "init", "-q");
  git(cwd, "add", ".");
  git(cwd, "-c", "user.name=Role Guide Test", "-c", "user.email=role-guide-test@example.invalid", "-c", "commit.gpgsign=false", "-c", `core.hooksPath=${join(cwd, ".git/empty-hooks")}`, "commit", "-qm", "scenario baseline");
  if (!test.implement) writeFileSync(join(cwd, "src/save.js"), broken);
  let block = fixtures.roles[test.role][test.inline ? "inline" : "block"];
  if (test.missing) {
    const actualDirectory = JSON.parse(block.match(/^Directory: (.+)$/m)[1]);
    block = block.replaceAll(actualDirectory, join(cwd, "unavailable-guides").replaceAll("\\", "/"));
  }
  const baseline = readFileSync(join(cwd, "src/save.js"), "utf8");
  const tracked = Object.fromEntries(git(cwd, "ls-files").split("\n").map(path => [path, readFileSync(join(cwd, path), "utf8")]));
  return { cwd, evidence, block, baseline, tracked, tree: snapshot(cwd), head: git(cwd, "rev-parse", "HEAD") };
}

function client(provider) {
  const command = provider === "codex" ? process.env.DEVEZ_CODEX_BIN : process.execPath;
  if (!command) throw new Error("DEVEZ_CODEX_BIN must point to the native executable");
  const child = spawn(command, provider === "codex" ? ["app-server"] : [join(repo, "npm/bridge/claude-agent-sdk-bridge.mjs")], {
    cwd: out, windowsHide: true, stdio: ["pipe", "pipe", "pipe"],
  });
  let sequence = 1;
  const pending = new Map(), events = [];
  let stderr = "";
  child.stderr.on("data", data => { stderr += data; });
  createInterface({ input: child.stdout }).on("line", line => {
    let message;
    try { message = JSON.parse(line); } catch { return; }
    events.push(message);
    if (message.method && message.id !== undefined) {
      // No simulated user answer/approval: observe the role's fallback instead.
      child.stdin.write(JSON.stringify({ id: message.id, error: { code: -32000, message: "시험에서 사용자 답변을 제공하지 않습니다. 무응답을 승인으로 처리하지 마세요." } }) + "\n");
      return;
    }
    const request = pending.get(message.id);
    if (request) {
      pending.delete(message.id);
      message.error ? request.reject(new Error(JSON.stringify(message.error))) : request.resolve(message.result);
    }
  });
  function rpc(method, params = {}) {
    return new Promise((resolve, reject) => {
      const id = sequence++;
      const timer = setTimeout(() => { pending.delete(id); reject(new Error(`${method} timeout`)); }, 120000);
      pending.set(id, { resolve: value => { clearTimeout(timer); resolve(value); }, reject: error => { clearTimeout(timer); reject(error); } });
      child.stdin.write(JSON.stringify({ id, method, params }) + "\n");
    });
  }
  async function completed(start) {
    const deadline = Date.now() + Number(process.env.DEVEZ_ROLE_TIMEOUT_MS || 900000);
    while (Date.now() < deadline) {
      const done = events.slice(start).find(event => event.method === "turn/completed");
      if (done) return done;
      await new Promise(resolve => setTimeout(resolve, 100));
    }
    throw new Error("turn timeout");
  }
  async function close() {
    if (provider === "claude") await rpc("shutdown").catch(() => {});
    child.stdin.end();
    setTimeout(() => child.kill(), 1000).unref();
    writeFileSync(join(out, `${provider}-stderr.txt`), stderr);
  }
  return { rpc, events, completed, close };
}

function assess(test, setup, events, text, status) {
  const calls = events.filter(event => event.method === "item/started")
    .map(event => event.params?.item).filter(item => ["commandExecution", "mcpToolCall", "dynamicToolCall"].includes(item?.type));
  const inputs = calls.map(item => String(item.command ?? JSON.stringify(item.arguments ?? {})));
  const directoryLine = setup.block.match(/^Directory: (.+)$/m)?.[1];
  const directory = directoryLine ? JSON.parse(directoryLine).replaceAll("\\", "/") : null;
  const probesDirectory = input => directory && input.replaceAll("\\\\", "/").replaceAll("\\", "/").includes(directory);
  const directoryProbe = inputs.findIndex(probesDirectory);
  const guides = fixtures.roles[test.role].guides;
  const guideReads = guides.map(guide => guide.name).filter(name => inputs.some(input => input.includes(name)));
  const strings = value => typeof value === "string" ? value : value && typeof value === "object" ? Object.values(value).map(strings).join("\n") : "";
  const guidesLoaded = guides.filter(guide => calls.some((call, index) => {
    if (!inputs[index].includes(guide.name)) return false;
    const completed = events.find(event => event.method === "item/completed" && event.params?.item?.id === call.id)?.params.item;
    const output = strings(completed?.aggregatedOutput ?? completed?.contentItems ?? completed?.result);
    return output.replaceAll("\r\n", "\n").includes(guide.body.trim());
  })).map(guide => guide.name);
  const issues = [];
  if (test.noGuides) {
    if (guides.length || setup.block.includes("<devez-role-guides>")) issues.push("integrated role still requires procedure files");
    if (inputs.some(input => /\/(role-guides|역할 지침 cache)\//.test(input.replaceAll("\\", "/")))) issues.push("integrated role read external procedure files");
  }
  if (status !== "completed") issues.push(`turn status: ${status}`);
  for (const name of guides.length ? test.expected : []) {
    if (!guideReads.includes(name) && !(test.missing && directoryProbe >= 0)) issues.push(`guide not requested: ${name}`);
    else if (!test.missing && !guidesLoaded.includes(name)) issues.push(`full guide read not evidenced: ${name}`);
  }
  if (guides.length && test.expected.length && !test.inline) {
    const entry = inputs.findIndex(input => input.includes(test.expected[0]));
    const firstRepositoryTool = calls.findIndex((call, index) => !probesDirectory(inputs[index]) && (call.type === "commandExecution" || ["Read", "Grep", "Glob", "Write", "Edit"].includes(call.tool)));
    if (entry >= 0 && firstRepositoryTool >= 0 && entry > firstRepositoryTool) issues.push("repository action preceded the entry guide");
  }
  for (const name of test.forbidden ?? []) if (guideReads.includes(name)) issues.push(`unrelated guide requested: ${name}`);
  if (test.inline && guideReads.length) issues.push("inline delivery caused redundant guide reads");
  if (test.missing && guidesLoaded.length) issues.push("unavailable guide was reported as loaded");
  if (!test.implement) {
    for (const [path, content] of Object.entries(setup.tracked)) {
      if (readFileSync(join(setup.cwd, path), "utf8") !== content) issues.push(`tracked file changed in a read-only/report scenario: ${path}`);
    }
    if (git(setup.cwd, "diff", "--cached", "--name-only")) issues.push("index changed in a read-only/report scenario");
    const before = setup.tree ?? Object.fromEntries(Object.entries(setup.tracked).map(([path, text]) => [path, createHash("sha256").update(text).digest("hex")]));
    const after = snapshot(setup.cwd);
    for (const path of new Set([...Object.keys(before), ...Object.keys(after)])) {
      if (test.id === "planner-draft" && path.startsWith("docs/plans/")) continue;
      if (before[path] !== after[path]) issues.push(`workspace file changed outside permitted scope: ${path}`);
    }
  }
  if (git(setup.cwd, "rev-parse", "HEAD") !== setup.head) issues.push("HEAD changed without authorization");
  if (["planner-interview", "planner-inline", "planner-missing"].includes(test.id) && readdirSync(join(setup.cwd, "docs/plans")).length) issues.push("plan written before requirements approval");
  if (test.implement) {
    const check = spawnSync(process.execPath, ["test/save.test.js"], { cwd: setup.cwd, windowsHide: true, encoding: "utf8" });
    writeFileSync(join(setup.evidence, "verification.txt"), check.stdout + check.stderr);
    if (check.status !== 0) issues.push("independent behavior verification failed");
  }
  if (test.report) {
    if (calls.length) issues.push("report-only scenario used tools");
    const blocks = text.trim().split(/\n\s*\n/);
    if (blocks.some(block => !block.startsWith("- "))) issues.push("prose outside report bullets");
    const labels = blocks.map(block => block.slice(2).split(":")[0].replace(/^문제\s*\d+.*$/, "문제"));
    const wanted = test.role === "planner" ? ["목표", "주요 작업", "검토 결과", test.scenario === "ready" ? "계획 경로" : "미확정 사항"]
      : test.role === "goal-runner" ? ["완료 여부", "변경·영향", "검증", ...(test.scenario === "blocked" ? ["남은 문제·조치"] : [])]
        : test.scenario === "clean" ? ["판정"] : ["판정", "문제", "재검토", "미확인"];
    if (JSON.stringify(labels) !== JSON.stringify(wanted)) issues.push(`report labels/order: ${labels.join(",")}`);
    const attribution = test.role === "planner" ? "- 목표: 제공 기록 기준," : test.role === "goal-runner" ? "- 완료 여부: 제공 기록 기준" : "- 판정: 제공 기록 기준";
    if (!blocks[0]?.startsWith(attribution)) issues.push("supplied-record prefix not exact");
    if (test.scenario === "blocked" && !blocks[0]?.includes("미완료")) issues.push("missing incomplete status");
    if (test.role === "reviewer" && test.scenario === "recheck") {
      const review = blocks.find(block => block.startsWith("- 재검토:")) ?? "";
      if (!/문제\s*1/.test(review) || !/문제\s*2/.test(review)) issues.push("earlier finding ID omitted");
      if (!blocks[0]?.includes("수정 필요")) issues.push("wrong defect verdict");
    }
    if (/\b(?:OKAY|CLEAR|WATCH|BLOCK|ITERATE|REJECT|APPROVE|COMMENT|REQUEST_CHANGES|ADDRESSED|NOT_ADDRESSED)\b/.test(text)) issues.push("internal verdict exposed");
  }
  return { guideReads, guidesLoaded, issues, tools: calls.map(item => ({ type: item.type, tool: item.tool, command: item.command, arguments: item.arguments })) };
}

const results = [];
async function sendTurn(c, provider, sessionId, model, role, block, prompt) {
  if (provider === "codex") {
    await c.rpc("turn/start", { threadId: sessionId, model, effort: "high", input: [{ type: "text", text: prompt }], additionalContext: { "devez-vibe-agent": { value: block, kind: "application" } }, collaborationMode: { mode: "default", settings: { model, reasoning_effort: "high", developer_instructions: fixtures.codexQuestion } } });
  } else {
    await c.rpc("session/prompt", { sessionId, model, effort: "high", permissionMode: "auto", input: [{ type: "text", text: prompt }], handoffContext: block + "\n\n" + fixtures.claudeReminder, toolPolicy: fixtures.roles[role].toolPolicy });
  }
}

async function run(provider) {
  const c = client(provider);
  const model = provider === "codex" ? "gpt-5.6-sol" : "claude:sonnet";
  try {
    if (provider === "codex") await c.rpc("initialize", { clientInfo: { name: "devez-vibe", title: "Devez Vibe", version: "1.8.19" }, capabilities: { experimentalApi: true } });
    for (const test of cases.filter(test => !selected || selected.includes(test.id) || (test.report && selected.includes("reports")))) {
      const setup = prepare(test, provider), start = c.events.length;
      let sessionId;
      try {
        const confinement = "\n시험 범위: 이 임시 작업 공간과 전달된 역할 지침 파일만 사용한다. 다른 저장소·사용자 파일·네트워크·외부 앱·하위 에이전트 사용과 커밋은 금지한다. 필요한 검토는 자체 검토로 표시한다.";
        const base = fixtures[provider === "codex" ? "codexBase" : "claudeBase"] + confinement;
        writeFileSync(join(setup.evidence, "input.json"), JSON.stringify({ provider, model, base, role: setup.block, prompt: test.prompt, baseline: setup.baseline, tracked: setup.tracked, tree: setup.tree, head: setup.head }, null, 2));
        if (provider === "codex") {
          const opened = await c.rpc("thread/start", { model, ephemeral: true, cwd: setup.cwd, approvalPolicy: "never", permissions: test.role === "reviewer" ? ":read-only" : ":danger-full-access", developerInstructions: base });
          sessionId = opened.thread.id;
        } else {
          const opened = await c.rpc("session/start", { model, cwd: setup.cwd, permissionMode: "auto", systemPrompt: base });
          sessionId = opened.id;
        }
        await sendTurn(c, provider, sessionId, model, test.role, setup.block, test.prompt);
        let done = await c.completed(start), transition;
        if (test.switchTo) {
          transition = { role: test.switchTo, sourceChangedBeforeSwitch: readFileSync(join(setup.cwd, "src/save.js"), "utf8") !== setup.baseline };
          const nextStart = c.events.length, nextBlock = fixtures.roles[test.switchTo].block;
          writeFileSync(join(setup.evidence, "switch-input.json"), JSON.stringify({ role: nextBlock, prompt: test.nextPrompt }, null, 2));
          await sendTurn(c, provider, sessionId, model, test.switchTo, nextBlock, test.nextPrompt);
          done = await c.completed(nextStart);
          const after = c.events.slice(nextStart).filter(event => event.method === "item/started" && (event.params?.item?.type === "commandExecution" || ["Read", "Glob", "Grep"].includes(event.params?.item?.tool)));
          const previousDirectory = JSON.parse(setup.block.match(/^Directory: (.+)$/m)[1]).replaceAll("\\", "/");
          transition.previousGuidesRead = after.some(event => String(event.params.item.command ?? JSON.stringify(event.params.item.arguments)).replaceAll("\\\\", "/").replaceAll("\\", "/").includes(previousDirectory));
          writeFileSync(join(setup.evidence, "transition.json"), JSON.stringify(transition, null, 2));
        }
        const events = c.events.slice(start);
        const messages = events.filter(event => event.method === "item/completed" && event.params?.item?.type === "agentMessage").map(event => event.params.item.text || "");
        const text = messages.at(-1) ?? "";
        writeFileSync(join(setup.evidence, "events.json"), JSON.stringify(events, null, 2));
        writeFileSync(join(setup.evidence, "output.md"), text);
        const result = { provider, model, case: test.id, status: done.params?.turn?.status, cwd: setup.cwd, evidence: setup.evidence, ...assess(test, setup, events, text, done.params?.turn?.status) };
        if (transition) {
          result.transition = transition;
          if (transition.sourceChangedBeforeSwitch) result.issues.push("Planner changed product source before switching");
          if (transition.previousGuidesRead) result.issues.push(`previous role guides were read after switching to ${test.switchTo}`);
        }
        results.push(result);
        console.log(JSON.stringify({ provider, case: test.id, status: result.status, guideReads: result.guideReads, issues: result.issues }));
      } catch (error) {
        const result = { provider, model, case: test.id, error: error.message, cwd: setup.cwd };
        results.push(result);
        writeFileSync(join(setup.evidence, "events.json"), JSON.stringify(c.events.slice(start), null, 2));
        console.log(JSON.stringify(result));
        // Do not let an unfinished timed-out turn contaminate later cases.
        break;
      } finally {
        if (provider === "claude" && sessionId) await c.rpc("session/close", { sessionId }).catch(() => {});
      }
    }
  } finally { await c.close(); }
}
if (rescoreRoot) {
  for (const previous of JSON.parse(readFileSync(join(out, "results.json"), "utf8"))) {
    if (previous.error) { results.push(previous); continue; }
    const test = cases.find(test => test.id === previous.case);
    if (!test) throw new Error(`Historical scenario ${previous.case} requires its original script; do not rescore it with a different workflow.`);
    const evidence = previous.evidence ?? previous.cwd;
    const input = JSON.parse(readFileSync(join(evidence, "input.json"), "utf8"));
    if (!input.tracked || !input.head) throw new Error("This run predates isolated evidence snapshots; review it manually.");
    const setup = { cwd: previous.cwd, evidence, block: input.role, baseline: input.baseline, tracked: input.tracked, tree: input.tree, head: input.head };
    const events = JSON.parse(readFileSync(join(evidence, "events.json"), "utf8"));
    const text = readFileSync(join(evidence, "output.md"), "utf8");
    const scored = { ...previous, ...assess(test, setup, events, text, previous.status) };
    if (previous.transition?.sourceChangedBeforeSwitch) scored.issues.push("Planner changed product source before switching");
    if (previous.transition?.previousGuidesRead) scored.issues.push(`previous role guides were read after switching to ${previous.transition.role}`);
    results.push(scored);
  }
} else {
  const runs = await Promise.allSettled(providers.map(run));
  for (const result of runs) if (result.status === "rejected") results.push({ error: String(result.reason) });
}
writeFileSync(join(out, rescoreRoot ? "results-rescored.json" : "results.json"), JSON.stringify(results, null, 2));
console.log(`DONE ${out}`);
if (results.some(result => result.error || result.issues?.length)) process.exitCode = 1;
