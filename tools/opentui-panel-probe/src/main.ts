import { BoxRenderable, TextRenderable, createCliRenderer } from "@opentui/core"

const renderer = await createCliRenderer({
  screenMode: "alternate-screen",
  backgroundColor: "#1e1e1e",
  consoleMode: "disabled",
  exitOnCtrlC: true,
  targetFps: 30,
})

const transcript = new TextRenderable(renderer, {
  id: "transcript",
  position: "absolute",
  left: 2,
  top: 2,
  right: 32,
  content: "응답 스트리밍 검증\n\nOpenTUI가 본문과 별도로 패널 사각형을 합성합니다.",
  fg: "#e6e6e6",
})

const panel = new BoxRenderable(renderer, {
  id: "info-panel",
  position: "absolute",
  right: 0,
  top: 0,
  width: 30,
  height: "100%",
  paddingLeft: 2,
  paddingTop: 1,
  backgroundColor: "#393939",
  zIndex: 100,
})

panel.add(
  new TextRenderable(renderer, {
    id: "panel-title",
    content: "Info panel\nNo information yet",
    fg: "#c5d8f8",
  }),
)

renderer.root.add(transcript)
renderer.root.add(panel)

let tick = 0
const timer = setInterval(() => {
  tick += 1
  transcript.content = [
    "응답 스트리밍 검증",
    "",
    "OpenTUI가 본문과 별도로 패널 사각형을 합성합니다.",
    `streaming chunk ${tick} · 한글 행 폭도 함께 갱신 중`,
  ].join("\n")
}, 120)

renderer.keyInput.on("keypress", (key) => {
  if (key.name !== "q") return
  clearInterval(timer)
  renderer.destroy()
  process.exit(0)
})
