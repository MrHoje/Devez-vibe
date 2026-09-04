// insane-search 스킬이 첫 호출에서 받는 파이썬 패키지를 설치 시점에 미리 깔아 둔다.
// 준비는 어디까지나 선택적이다. 파이썬이 없거나 설치가 실패해도 안내만 남기고 정상 종료해서
// devezVibe 설치 자체가 깨지지 않게 한다. 스킬은 첫 사용 때 다시 설치를 시도하므로 손해가 없다.
import { spawnSync } from "node:child_process";

// SKILL.md의 의존성 가드와 같은 조합이다. 한쪽만 바뀌면 첫 호출에서 다시 설치가 돈다.
const guard = 'import curl_cffi,bs4,yaml,pypdf,markdownify; v=curl_cffi.__version__.split("."); assert (int(v[0]),int(v[1]))>=(0,15)';
const packages = ["curl_cffi>=0.15.0", "beautifulsoup4", "pyyaml", "pypdf", "markdownify"];

function run(command, args) {
  return spawnSync(command, args, { encoding: "utf8", windowsHide: true, timeout: 300000 });
}

// Windows에는 python3가 없는 대신 실행하면 스토어를 여는 별칭이 잡혀 있을 수 있다.
// 버전 출력을 실제로 확인해서 쓸 수 있는 실행기만 고른다.
function findPython() {
  for (const candidate of ["python3", "python", "py"]) {
    const probe = run(candidate, ["-c", "import sys; print(sys.version_info[0])"]);
    if (probe.status === 0 && probe.stdout.trim() === "3") return candidate;
  }
  return null;
}

const python = findPython();
if (!python) {
  console.log("insane-search 준비 건너뜀: 파이썬 3을 찾지 못했습니다. 설치하면 첫 사용 때 자동으로 준비됩니다.");
  process.exit(0);
}

if (run(python, ["-c", guard]).status === 0) {
  console.log("insane-search 의존성 준비 완료: 이미 설치되어 있습니다.");
  process.exit(0);
}

// --user는 관리자 권한 없이도 설치되게 하고, 가상환경에서는 pip가 알아서 무시한다.
const install = run(python, ["-m", "pip", "install", "-U", "--user", "-q", ...packages]);
if (install.status === 0 && run(python, ["-c", guard]).status === 0) {
  console.log("insane-search 의존성 준비 완료");
} else {
  const detail = (install.stderr || install.stdout || "").trim().split("\n").pop() || "원인 미상";
  console.log(`insane-search 준비 건너뜀 (${detail}). 첫 사용 때 다시 시도합니다.`);
}
