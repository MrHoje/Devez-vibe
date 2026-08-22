import { tool } from "@opencode-ai/plugin"

export default tool({
  description:
    "Generate an image from a text prompt using the free Pollinations.ai API (keyless). Saves a PNG to the generated-images/ directory and returns its path.",
  args: {
    prompt: tool.schema
      .string()
      .describe("Detailed image description in English"),
    width: tool.schema.number().optional().describe("Image width in pixels (default 768)"),
    height: tool.schema.number().optional().describe("Image height in pixels (default 768)"),
    model: tool.schema
      .string()
      .optional()
      .describe("Pollinations model: flux (default), turbo, or others"),
    seed: tool.schema.number().optional().describe("Deterministic seed for reproducible output"),
  },
  async execute(args, context) {
    const { prompt, width = 768, height = 768, model = "flux", seed } = args
    const base = context.worktree ?? context.directory
    const outDir = `${base}/generated-images`
    await Bun.$`mkdir -p ${outDir}`.nothrow()

    const params = new URLSearchParams({
      width: String(width),
      height: String(height),
      nologo: "true",
      model,
    })
    if (seed !== undefined) params.set("seed", String(seed))

    const url = `https://image.pollinations.ai/prompt/${encodeURIComponent(prompt)}?${params.toString()}`
    const res = await fetch(url)
    if (!res.ok) return `Pollinations API error: HTTP ${res.status}`

    const buf = await res.arrayBuffer()
    const stamp = new Date().toISOString().replace(/[:.]/g, "-")
    const file = `${outDir}/${stamp}.png`
    await Bun.write(file, new Uint8Array(buf))
    return `Image saved to ${file} (${prompt})`
  },
})
