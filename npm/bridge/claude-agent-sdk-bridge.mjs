#!/usr/bin/env node

import { randomUUID } from "node:crypto";
import { createReadStream } from "node:fs";
import { readdir, readFile } from "node:fs/promises";
import { homedir } from "node:os";
import { join } from "node:path";
import { createInterface } from "node:readline";
import {
  deleteSession,
  forkSession,
  getSessionInfo,
  getSessionMessages,
  listSessions,
  startup,
} from "@anthropic-ai/claude-agent-sdk";

const VERSION = process.env.DEVEZ_VIBE_VERSION || "dev";
const sessions = new Map();
// The id this bridge proposes is not always the id the CLI persists the
// transcript under, so a session can be renamed mid-flight. Old id → live id,
// which keeps ids the host already handed out (or wrote to disk) resolvable.
const sessionAliases = new Map();
const pendingHostRequests = new Map();
const modelCatalogs = new Map();
let nextHostRequest = 1;

class AsyncQueue {
  constructor() {
    this.values = [];
    this.waiters = [];
    this.closed = false;
  }

  push(value) {
    if (this.closed) throw new Error("Claude 입력 큐가 종료되었습니다.");
    const waiter = this.waiters.shift();
    if (waiter) waiter({ value, done: false });
    else this.values.push(value);
  }

  close() {
    this.closed = true;
    for (const waiter of this.waiters.splice(0)) waiter({ value: undefined, done: true });
  }

  [Symbol.asyncIterator]() {
    return {
      next: () => {
        if (this.values.length) return Promise.resolve({ value: this.values.shift(), done: false });
        if (this.closed) return Promise.resolve({ value: undefined, done: true });
        return new Promise((resolve) => this.waiters.push(resolve));
      },
    };
  }
}

function write(message) {
  process.stdout.write(`${JSON.stringify(message)}\n`);
}

function notify(method, params) {
  write({ method, params });
}

function rpcError(error) {
  return {
    code: -32000,
    message: error instanceof Error ? error.message : String(error),
  };
}

function hostRequest(method, params, signal) {
  const id = `claude-host-${nextHostRequest++}`;
  return new Promise((resolve, reject) => {
    const abort = () => {
      pendingHostRequests.delete(id);
      reject(new Error("사용자 입력 요청이 취소되었습니다."));
    };
    if (signal?.aborted) return abort();
    signal?.addEventListener("abort", abort, { once: true });
    pendingHostRequests.set(id, {
      resolve: (value) => {
        signal?.removeEventListener("abort", abort);
        resolve(value);
      },
    });
    write({ id, method, params });
  });
}

function sanitizedEnvironment() {
  const env = { ...process.env };
  delete env.ANTHROPIC_API_KEY;
  delete env.ANTHROPIC_AUTH_TOKEN;
  env.CLAUDE_AGENT_SDK_CLIENT_APP = `devez-vibe/${VERSION}`;
  return env;
}

function applyClaudeExecutable(options, params) {
  const executable = String(params.claudePath || "").trim();
  if (!executable) return;
  // On Windows use the SDK's matching native CLI bundle. It reads the same
  // ~/.claude credentials, settings, hooks, skills, and plugins, while avoiding
  // EINVAL from command shims and incompatible separately-installed CLI builds.
  if (process.platform === "win32") return;
  options.pathToClaudeCodeExecutable = executable;
}

// The SDK model list carries no window size, so the status line would stay
// blank until a turn reports one — and a fresh session has no turn yet. Claude
// ships a 200k window, and the `[1m]` variants a 1M one.
function claudeContextWindow(...names) {
  const oneMillion = names.some((name) => String(name || "").includes("[1m]"));
  return oneMillion ? 1_000_000 : 200_000;
}

function capabilityContextWindow(capabilities, ...names) {
  const reported = Number(capabilities?.contextWindow || capabilities?.contextWindowSize || 0);
  return reported > 0
    ? reported
    : claudeContextWindow(...names, capabilities?.value, capabilities?.resolvedModel);
}

function modelCapabilities(models, model) {
  const value = stripClaudeModel(model);
  return models.find((candidate) => candidate.value === value || candidate.resolvedModel === value)
    || (!value ? models.find((candidate) => candidate.value === "default") : undefined);
}

function supportedEffort(model, requested) {
  if (!model) return requested || "high";
  const levels = Array.isArray(model.supportedEffortLevels) ? model.supportedEffortLevels : [];
  if (!model.supportsEffort || !levels.length) return "";
  if (requested && levels.includes(requested)) return requested;
  return levels.includes("high") ? "high" : levels.at(-1) || "";
}

function compactClaudeModelName(model) {
  const clean = (value) => String(value || "")
    .replace(/^Claude\s*(?:·|:|-)?\s*/i, "")
    .replace(
      /\s*(?:\[[^\]]*\bcontext\b[^\]]*\]|\([^)]*\bcontext\b[^)]*\)|\[\s*\d+(?:\.\d+)?\s*[mk]\s*\]|\(\s*\d+(?:\.\d+)?\s*[mk]\s*\))\s*$/i,
      "",
    )
    .trim();
  let fallback = "";
  for (const candidate of [model.displayName, model.resolvedModel, model.value]) {
    const source = clean(candidate);
    const match = source.match(/\b(fable|opus|sonnet|haiku)\b(?:[\s-]+(\d+(?:[.-]\d+)?))?/i);
    if (!match) continue;
    const family = match[1][0].toUpperCase() + match[1].slice(1).toLowerCase();
    const version = match[2]?.replace("-", ".");
    if (version) return `${family} ${version}`;
    fallback ||= family;
  }
  return fallback || clean(model.displayName || model.resolvedModel || model.value);
}

function catalogEntry(model, defaultResolvedModel) {
  const value = String(model.value || "");
  const resolved = String(model.resolvedModel || value);
  const efforts = model.supportsEffort && Array.isArray(model.supportedEffortLevels)
    ? model.supportedEffortLevels
    : [];
  const contextWindow = capabilityContextWindow(model, value, resolved);
  return {
    id: visibleModel(resolved),
    model: visibleModel(value),
    displayName: compactClaudeModelName(model),
    defaultReasoningEffort: efforts.includes("high") ? "high" : efforts.at(-1) || "",
    supportedReasoningEfforts: efforts.map((reasoningEffort) => ({ reasoningEffort })),
    isDefault: Boolean(defaultResolvedModel) && resolved === defaultResolvedModel,
    ...(contextWindow > 0 ? { contextWindow } : {}),
  };
}

async function loadModelCatalog(params) {
  const cacheKey = `${params.claudePath || "claude"}\n${params.cwd || process.cwd()}`;
  if (modelCatalogs.has(cacheKey)) return modelCatalogs.get(cacheKey);
  const pending = (async () => {
    const input = new AsyncQueue();
    const options = {
      cwd: params.cwd || process.cwd(),
      persistSession: false,
      settingSources: [],
      tools: [],
      env: sanitizedEnvironment(),
      stderr: (data) => process.stderr.write(data),
    };
    applyClaudeExecutable(options, params);
    const agentQuery = await startAgentQuery(input, options);
    const consumer = (async () => {
      try { for await (const _message of agentQuery) { /* initialization only */ } }
      catch { /* the caller receives the supportedModels error */ }
    })();
    try {
      const models = await agentQuery.supportedModels();
      const defaultResolvedModel = String(
        models.find((model) => model.value === "default")?.resolvedModel || "",
      );
      return {
        data: models
          .filter((model) => model.value && model.value !== "default")
          .map((model) => catalogEntry(model, defaultResolvedModel)),
      };
    } finally {
      input.close();
      agentQuery.close();
      await Promise.race([consumer, new Promise((resolve) => setTimeout(resolve, 1000))]);
    }
  })();
  modelCatalogs.set(cacheKey, pending);
  try {
    return await pending;
  } catch (error) {
    modelCatalogs.delete(cacheKey);
    throw error;
  }
}

function stripClaudeModel(model) {
  if (!model || model === "claude:default") return undefined;
  return model.startsWith("claude:") ? model.slice("claude:".length) : model;
}

function visibleModel(model) {
  if (!model) return "claude:default";
  return model.startsWith("claude:") ? model : `claude:${model}`;
}

function visibleSession(id) {
  return id.startsWith("claude:") ? id : `claude:${id}`;
}

function rawSession(id) {
  return id.startsWith("claude:") ? id.slice("claude:".length) : id;
}

/** Follows the rename chain from an id the host still remembers to the live one. */
function liveSessionId(id) {
  let current = rawSession(String(id ?? ""));
  const seen = new Set();
  while (sessionAliases.has(current) && !seen.has(current)) {
    seen.add(current);
    current = sessionAliases.get(current);
  }
  return current;
}

function lookupSession(id) {
  return sessions.get(liveSessionId(id));
}

/**
 * Binds the session to the id the CLI actually persists under. `options.sessionId`
 * is a request, not a guarantee: a session that gets rotated (a pre-warmed process
 * the first turn does not reuse, a resume the CLI declines) writes its transcript
 * under a different uuid, and everything downstream — `session/resume`,
 * `session/history`, DevezCode's `-r` on the next launch — keys off the persisted
 * id. Adopting it here and telling the host is what keeps a Claude-backed session
 * resumable at all.
 */
function adoptSessionId(session, incoming) {
  const real = rawSession(String(incoming ?? ""));
  if (!real || real === session.id) return;
  const previous = session.id;
  sessions.delete(previous);
  sessionAliases.set(previous, real);
  session.id = real;
  sessions.set(real, session);
  notify("claude/session/rebound", {
    threadId: visibleSession(previous),
    newThreadId: visibleSession(real),
  });
}

const PERMISSION_MODES = ["default", "acceptEdits", "plan", "auto", "bypassPermissions"];

function permissionMode(requested, fallback = "default") {
  const mode = String(requested || "");
  return PERMISSION_MODES.includes(mode) ? mode : fallback;
}

// Moves a live session onto a mode the badge picked. A rejected mode — policy
// disables bypass, say — leaves the session on the one it already had.
async function applyPermissionMode(session, requested) {
  const mode = permissionMode(requested, session.permissionMode || "default");
  if (mode === session.permissionMode) return;
  try {
    await session.query.setPermissionMode(mode);
    session.permissionMode = mode;
  } catch (error) {
    notify("claude/permissionMode/rejected", {
      threadId: visibleSession(session.id),
      permissionMode: mode,
      message: error?.message || String(error),
    });
  }
}

function makeOptions(params, sessionId, resume) {
  const options = {
    cwd: params.cwd || process.cwd(),
    includePartialMessages: true,
    permissionMode: permissionMode(params.permissionMode),
    // Not a mode, a capability: the SDK refuses `bypassPermissions` outright
    // unless the session was started with this. Devez Vibe already auto-allows
    // tools through `canUseTool`, so allowing the mode to be *reachable* grants
    // nothing the session did not already have.
    allowDangerouslySkipPermissions: true,
    enableFileCheckpointing: true,
    persistSession: true,
    settingSources: ["user", "project", "local"],
    skills: "all",
    tools: { type: "preset", preset: "claude_code" },
    systemPrompt: {
      type: "preset",
      preset: "claude_code",
      append: params.systemPrompt || "",
    },
    env: sanitizedEnvironment(),
    stderr: (data) => process.stderr.write(data),
  };
  const model = stripClaudeModel(params.model);
  if (model) options.model = model;
  if (params.effort) options.effort = params.effort;
  applyClaudeExecutable(options, params);
  if (resume) options.resume = resume;
  else options.sessionId = sessionId;
  options.canUseTool = (toolName, input, permission) =>
    requestToolPermission(toolName, input, permission);
  return options;
}

async function startAgentQuery(prompt, options) {
  const warm = await startup({ options });
  try {
    return warm.query(prompt);
  } catch (error) {
    warm.close();
    throw error;
  }
}

async function requestToolPermission(toolName, input, permission) {
  if (toolName === "AskUserQuestion") {
    const questions = (input.questions || []).map((question, index) => ({
      id: `q${index}`,
      header: question.header || "질문",
      question: question.question || "입력이 필요합니다.",
      options: (question.options || []).map((option) => ({
        label: option.label || String(option),
        description: option.description || "",
      })),
      isOther: true,
      multiSelect: Boolean(question.multiSelect),
    }));
    const response = await hostRequest(
      "item/tool/requestUserInput",
      { questions },
      permission.signal,
    );
    const answers = {};
    for (let index = 0; index < questions.length; index += 1) {
      const selected = response?.answers?.[`q${index}`]?.answers;
      if (Array.isArray(selected) && selected.length) {
        answers[questions[index].question] = questions[index].multiSelect
          ? selected.join(", ")
          : selected[0];
      }
    }
    return { behavior: "allow", updatedInput: { ...input, answers } };
  }

  // Plan mode only means anything if leaving it is the user's call — the CLI
  // shows the plan and waits. The blanket allow below would answer for them.
  const planApproval = toolName === "ExitPlanMode";

  // Devez Vibe runs in its existing full-access profile. Claude's callback is
  // still kept so AskUserQuestion and explicit user-authored ask rules reach UI.
  if (!permission.matchedAskRule && !planApproval) {
    return { behavior: "allow", updatedInput: input };
  }

  let method = "item/permissions/requestApproval";
  let params = {
    reason: permission.decisionReason || permission.description || permission.title,
    permissions: { tool: toolName, blockedPath: permission.blockedPath },
  };
  if (planApproval) {
    params = {
      reason: input.plan || permission.description || "계획대로 진행할까요?",
      permissions: { tool: toolName },
    };
  } else if (toolName === "Bash") {
    method = "item/commandExecution/requestApproval";
    params = {
      command: input.command || "command",
      cwd: input.cwd,
      reason: permission.decisionReason || permission.description,
    };
  } else if (["Edit", "Write", "NotebookEdit"].includes(toolName)) {
    method = "item/fileChange/requestApproval";
    params = {
      grantRoot: permission.blockedPath || input.file_path || input.notebook_path,
      reason: permission.decisionReason || permission.description,
    };
  }
  const response = await hostRequest(method, params, permission.signal);
  const accepted = response?.decision === "accept" || response?.decision === "acceptForSession"
    || response?.scope === "turn" || response?.scope === "session";
  if (!accepted) return { behavior: "deny", message: "사용자가 작업을 거부했습니다." };
  return {
    behavior: "allow",
    updatedInput: input,
    ...(response?.decision === "acceptForSession" || response?.scope === "session"
      ? { updatedPermissions: permission.suggestions }
      : {}),
  };
}

async function createSession(params, resumeId) {
  const id = resumeId || randomUUID();
  const queue = new AsyncQueue();
  const session = {
    id,
    cwd: params.cwd || process.cwd(),
    model: visibleModel(params.model),
    effort: params.effort || "",
    permissionMode: permissionMode(params.permissionMode),
    models: [],
    queue,
    query: null,
    turn: null,
    // Prompts that arrived while a turn was running, run in order afterwards.
    pendingPrompts: [],
    turnSequence: 1,
    itemSequence: 1,
    streamBlocks: new Map(),
    tools: new Map(),
    tasks: new Map(),
    planCreatePending: false,
    subagents: new Map(),
    knownSubagents: new Map(),
    lastContextUsage: null,
    lastContextWindow: 0,
  };
  const agentQuery = await startAgentQuery(queue, makeOptions(params, id, resumeId));
  session.query = agentQuery;
  sessions.set(id, session);
  const consumer = consume(session).catch((error) => {
    notify("error", {
      threadId: id,
      provider: "Claude",
      error: { message: error instanceof Error ? error.message : String(error) },
      willRetry: false,
    });
    clearSubagents(session);
    if (session.turn) finishTurn(session, error);
  });
  session.consumer = consumer;
  let initialization;
  try {
    initialization = await agentQuery.initializationResult();
  } catch (error) {
    sessions.delete(id);
    queue.close();
    agentQuery.close();
    await Promise.race([consumer, new Promise((resolve) => setTimeout(resolve, 1000))]);
    throw error;
  }
  session.models = Array.isArray(initialization.models) ? initialization.models : [];
  const capabilities = modelCapabilities(session.models, params.model);
  // A resumed session can only name its model as the resolved id the transcript
  // recorded (`claude-opus-4-5-...`), which matches nothing in the host's model
  // list. Report the catalog value instead so the picker lands on that model.
  if (capabilities?.value && capabilities.value !== "default") {
    session.model = visibleModel(capabilities.value);
  }
  session.effort = supportedEffort(capabilities, params.effort);
  const account = initialization.account || await safeAccount(agentQuery);
  const usage = await safeUsage(agentQuery);
  return { session, initialization, account, usage };
}

async function safeAccount(agentQuery) {
  try { return await agentQuery.accountInfo(); } catch { return null; }
}

async function safeUsage(agentQuery) {
  try { return await agentQuery.usage_EXPERIMENTAL_MAY_CHANGE_DO_NOT_RELY_ON_THIS_API_YET(); }
  catch { return null; }
}

function nextItemId(session, suffix) {
  return `claude-item-${session.itemSequence++}-${suffix}`;
}

function emitItem(session, phase, item) {
  notify(`item/${phase}`, { threadId: session.id, turnId: session.turn?.id, item });
}

function emitDelta(session, method, itemId, delta) {
  notify(method, { threadId: session.id, turnId: session.turn?.id, itemId, delta, provider: "Claude" });
}

function isKoreanPrompt(input) {
  const prompt = (Array.isArray(input) ? input : [])
    .filter((item) => item?.type === "text")
    .map((item) => String(item.text || ""))
    .join("\n");
  return /[\uac00-\ud7a3]/.test(prompt);
}

function openingNotice(input) {
  return isKoreanPrompt(input)
    ? "요청 내용을 확인하고 필요한 작업을 진행하겠습니다."
    : "I’ll review the request and proceed with the necessary work.";
}

// `Now the tile view logic.` carries nothing a Korean reader needs, and the
// stand-in that used to replace it carried even less — the same sentence before
// every tool call, however many calls the turn made. Drop the line instead; the
// tool item that follows already names what is being read.
function normalizeProgressText(turn, text) {
  const value = String(text || "");
  const trimmed = value.trim();
  if (turn?.koreanRequest
    && trimmed.length <= 160
    && !/[\uac00-\ud7a3]/.test(trimmed)
    && /^Now\b[^\r\n]*[.!?]?$/i.test(trimmed)) {
    return "";
  }
  return value;
}

// Claude can answer with a tool_use as its first and only content block even
// when the prompt asks for an opening update. Keep the visible contract stable
// without duplicating a real model-written update.
function emitOpeningNotice(session) {
  if (!session.turn || session.turn.openingNoticeEmitted) return;
  const text = session.turn.openingNotice;
  const id = nextItemId(session, "opening");
  const item = { id, type: "agentMessage", text, provider: "Claude" };
  emitItem(session, "started", item);
  emitItem(session, "completed", item);
  session.turn.openingNoticeEmitted = true;
}

// Windows rounds larger timer delays up to the next scheduler slice. Ten
// milliseconds lands near one terminal frame instead of visibly stepping at ~30ms.
const SMOOTH_TEXT_INTERVAL_MS = 10;
const SMOOTH_TEXT_TARGET_FRAMES = 10;
const SMOOTH_TEXT_MAX_GRAPHEMES = 24;
const graphemeSegmenter = typeof Intl.Segmenter === "function"
  ? new Intl.Segmenter(undefined, { granularity: "grapheme" })
  : null;

function splitGraphemes(text) {
  return graphemeSegmenter
    ? Array.from(graphemeSegmenter.segment(text), ({ segment }) => segment)
    : Array.from(text);
}

// Claude can deliver a whole phrase in one SDK event. Drain roughly one visual
// frame's share at a time, catching up quickly when a large backlog arrives.
function takeSmoothTextChunk(text) {
  const graphemes = splitGraphemes(text);
  const size = Math.min(
    SMOOTH_TEXT_MAX_GRAPHEMES,
    Math.max(1, Math.ceil(graphemes.length / SMOOTH_TEXT_TARGET_FRAMES)),
  );
  return {
    chunk: graphemes.slice(0, size).join(""),
    rest: graphemes.slice(size).join(""),
  };
}

class SmoothTextStream {
  constructor(emit, intervalMs = SMOOTH_TEXT_INTERVAL_MS) {
    this.emit = emit;
    this.intervalMs = intervalMs;
    this.pending = "";
    this.timer = null;
    this.waiters = [];
  }

  push(text) {
    this.pending += text;
    this.schedule();
  }

  schedule() {
    if (this.timer != null) return;
    // Wait one visual frame before the first drain. Claude often sends several
    // tiny deltas back-to-back; batching them removes the uneven one-character
    // jumps while keeping added latency below one frame.
    this.timer = setTimeout(() => {
      this.timer = null;
      this.drain();
    }, this.intervalMs);
  }

  drain() {
    if (!this.pending) {
      this.timer = null;
      for (const resolve of this.waiters.splice(0)) resolve();
      return;
    }
    const { chunk, rest } = takeSmoothTextChunk(this.pending);
    this.pending = rest;
    this.emit(chunk);
    if (this.pending) {
      this.schedule();
    } else {
      for (const resolve of this.waiters.splice(0)) resolve();
    }
  }

  finish() {
    if (!this.pending && this.timer == null) return Promise.resolve();
    return new Promise((resolve) => this.waiters.push(resolve));
  }

  flush() {
    if (this.timer != null) clearTimeout(this.timer);
    this.timer = null;
    if (this.pending) this.emit(this.pending);
    this.pending = "";
    for (const resolve of this.waiters.splice(0)) resolve();
  }
}

function flushSmoothStreams(session) {
  for (const block of session.streamBlocks.values()) block.smooth?.flush();
}

function tokenBreakdown(usage) {
  if (!usage) return null;
  const input = Number(usage.input_tokens ?? usage.inputTokens ?? 0);
  const cached = Number(usage.cache_read_input_tokens ?? usage.cacheReadInputTokens ?? 0);
  const cacheWrite = Number(usage.cache_creation_input_tokens ?? usage.cacheCreationInputTokens ?? 0);
  const output = Number(usage.output_tokens ?? usage.outputTokens ?? 0);
  return {
    inputTokens: input + cached + cacheWrite,
    cachedInputTokens: cached,
    cacheWriteInputTokens: cacheWrite,
    outputTokens: output,
    totalTokens: input + cached + cacheWrite + output,
  };
}

// A resumed session has no turn yet, so the status line would show no context
// until the next result. Rebuild both figures from the stored transcript.
function historyTokenUsage(messages, models, model) {
  const total = {
    inputTokens: 0,
    cachedInputTokens: 0,
    cacheWriteInputTokens: 0,
    outputTokens: 0,
    totalTokens: 0,
  };
  let last = null;
  let counted = false;
  for (const message of messages) {
    if (message.type !== "assistant" || message.message?.model === "<synthetic>") continue;
    const breakdown = tokenBreakdown(message.message?.usage);
    if (!breakdown) continue;
    counted = true;
    for (const key of Object.keys(total)) total[key] += breakdown[key];
    // Only the main thread occupies the context window; subagents run their own.
    if (!message.parent_tool_use_id) last = breakdown;
  }
  if (!counted) return null;
  const capabilities = modelCapabilities(models, model);
  const contextWindow = capabilityContextWindow(capabilities, model);
  return {
    total,
    ...(last ? { last } : {}),
    ...(contextWindow > 0 ? { modelContextWindow: contextWindow } : {}),
  };
}

function emitHeldStart(session, current) {
  if (!current?.pendingStart) return;
  emitItem(session, "started", current.pendingStart);
  current.pendingStart = null;
}

async function processStreamEvent(session, message) {
  if (!session.turn || message.parent_tool_use_id) return;
  const event = message.event || {};
  if (event.type === "message_start") {
    flushSmoothStreams(session);
    session.streamBlocks.clear();
  }
  if (event.type === "content_block_start") {
    const block = event.content_block || {};
    if (block.type !== "text" && block.type !== "thinking") return;
    const id = nextItemId(session, block.type);
    const item = block.type === "text"
      ? { id, type: "agentMessage", text: "", provider: "Claude" }
      : { id, type: "reasoning", summary: [] };
    const smooth = block.type === "text"
      ? new SmoothTextStream((delta) => emitDelta(session, "item/agentMessage/delta", id, delta))
      : null;
    // A held English line can end up dropped entirely, and an item announced
    // before that decision would stay on screen as an empty bubble. Hold the
    // start too, and emit it with the first text that survives.
    const held = block.type === "text" && session.turn.koreanRequest;
    session.streamBlocks.set(event.index, {
      id,
      type: block.type,
      text: "",
      smooth,
      languagePending: held ? "" : null,
      holdEnglishProgress: false,
      pendingStart: held ? item : null,
    });
    if (!held) emitItem(session, "started", item);
    return;
  }
  if (event.type === "content_block_delta") {
    const current = session.streamBlocks.get(event.index);
    if (!current) return;
    const delta = event.delta?.text || event.delta?.thinking || "";
    if (!delta) return;
    current.text += delta;
    // Held text may still be dropped, and counting it as visible would suppress
    // the opening notice in its place — leaving the turn with nothing to show.
    if (current.type === "text" && current.languagePending == null) session.turn.sawVisibleText = true;
    if (!current.smooth) {
      emitDelta(session, "item/reasoning/summaryTextDelta", current.id, delta);
      return;
    }
    if (current.languagePending != null) {
      current.languagePending += delta;
      const probe = current.languagePending.trimStart();
      const lower = probe.toLowerCase();
      if (!current.holdEnglishProgress && "now".startsWith(lower)) return;
      if (/^now(?:\s|$)/i.test(probe)) {
        current.holdEnglishProgress = true;
        return;
      }
      emitHeldStart(session, current);
      session.turn.sawVisibleText = true;
      current.smooth.push(current.languagePending);
      current.languagePending = null;
      return;
    }
    current.smooth.push(delta);
    return;
  }
  if (event.type === "content_block_stop") {
    const current = session.streamBlocks.get(event.index);
    if (!current) return;
    if (current.languagePending != null) {
      const visible = normalizeProgressText(session.turn, current.languagePending);
      current.text = visible;
      current.languagePending = null;
      if (!visible.trim() && current.pendingStart) {
        session.streamBlocks.delete(event.index);
        return;
      }
      emitHeldStart(session, current);
      if (visible.trim()) session.turn.sawVisibleText = true;
      current.smooth?.push(visible);
    }
    emitHeldStart(session, current);
    await current.smooth?.finish();
    const item = current.type === "text"
      ? { id: current.id, type: "agentMessage", text: current.text, provider: "Claude" }
      : { id: current.id, type: "reasoning", summary: [current.text] };
    emitItem(session, "completed", item);
    session.streamBlocks.delete(event.index);
  }
}

function processAssistant(session, message) {
  if (!session.turn) return;
  if (message.parent_tool_use_id) {
    recordSubagentMessage(session, message);
    return;
  }
  session.lastContextUsage = tokenBreakdown(message.message?.usage);
  const capabilities = modelCapabilities(
    session.models,
    message.message?.model || session.model,
  );
  session.lastContextWindow = capabilityContextWindow(
    capabilities,
    message.message?.model,
    session.model,
  );
  const content = Array.isArray(message.message?.content) ? message.message.content : [];
  const hasToolUse = content.some((block) => block.type === "tool_use");
  const hasVisibleText = session.turn.sawVisibleText || content.some(
    (block) => block.type === "text" && String(block.text || "").trim(),
  );
  // Without partial SDK events, replay completed text before tool items so the
  // visible order still matches the assistant content order.
  if (!session.streamBlocks.size && !session.turn.sawStreamText) {
    for (const block of content) {
      if (block.type !== "text" && block.type !== "thinking") continue;
      const visible = block.type === "text"
        ? normalizeProgressText(session.turn, block.text)
        : "";
      if (block.type === "text" && !visible.trim() && String(block.text || "").trim()) continue;
      const id = nextItemId(session, block.type);
      const item = block.type === "text"
        ? { id, type: "agentMessage", text: visible, provider: "Claude" }
        : { id, type: "reasoning", summary: [block.thinking || ""] };
      emitItem(session, "started", item);
      emitItem(session, "completed", item);
    }
  }
  if (hasToolUse && !hasVisibleText) emitOpeningNotice(session);
  for (const block of content) {
    if (block.type === "tool_use") processToolUse(session, block);
  }
}

function processToolUse(session, block) {
  const name = block.name || "Tool";
  const input = block.input || {};
  if (name === "TaskCreate" || name === "TaskUpdate" || name === "TaskList") {
    updatePlanFromToolUse(session, name, block.id, input);
    session.tools.set(block.id, { name, input, suppressed: true });
    return;
  }
  if (name === "AskUserQuestion") {
    flushPendingPlan(session);
    session.tools.set(block.id, { name, input, suppressed: true });
    return;
  }
  flushPendingPlan(session);
  const item = toolItem(session, block.id, name, input);
  session.tools.set(block.id, { name, input, item });
  emitItem(session, "started", item);
  if (SUBAGENT_TOOLS.includes(name)) startSubagent(session, block);
}

function toolItem(session, id, name, input) {
  if (name === "Bash") {
    return { id, type: "commandExecution", command: input.command || "command", status: "inProgress", aggregatedOutput: "" };
  }
  if (["Edit", "Write", "NotebookEdit"].includes(name)) {
    return { id, type: "fileChange", status: "inProgress", changes: fileChanges(name, input) };
  }
  if (name === "WebSearch") {
    return { id, type: "webSearch", query: input.query || "" };
  }
  if (name === "Agent" || name === "Task") {
    return { id, type: "collabAgentToolCall", tool: { name, arguments: input } };
  }
  if (name.startsWith("mcp__")) {
    const [, server = "server", ...tool] = name.split("__");
    return { id, type: "mcpToolCall", server, tool: tool.join("__") || name, arguments: input };
  }
  return { id, type: "dynamicToolCall", tool: name, arguments: input };
}

function fileChanges(name, input) {
  const path = input.file_path || input.notebook_path || "unknown";
  if (name === "Edit") {
    const before = String(input.old_string || "").split("\n").map((line) => `-${line}`).join("\n");
    const after = String(input.new_string || "").split("\n").map((line) => `+${line}`).join("\n");
    return [{ path, kind: { type: "update" }, diff: `@@ -1 +1 @@\n${before}\n${after}` }];
  }
  const content = String(input.content || input.new_source || input.new_cell_source || "");
  const additions = content.split("\n").map((line) => `+${line}`).join("\n");
  return [{ path, kind: { type: name === "Write" ? "add" : "update" }, diff: `@@ -0,0 +1 @@\n${additions}` }];
}

function numberedTaskIndex(subject) {
  const match = String(subject || "").trim().match(/^(\d+)[.)]\s+/);
  return match ? Number(match[1]) : null;
}

// Claude의 Task id는 세션 전체에서 누적된다. 제목 번호가 다시 1부터
// 시작할 때만 새 계획이며, 3·4번만 갱신되는 턴은 기존 1~6번을 지킨다.
function prepareTaskPlanForCreate(tasks, subject) {
  if (numberedTaskIndex(subject) === 1) tasks.clear();
}

function applyTaskUpdate(tasks, input, turnId, onIntermediate) {
  const task = tasks.get(String(input.taskId));
  if (!task) return false;
  if (input.subject) task.subject = input.subject;
  const status = input.status;
  if (status === "in_progress" || status === "completed") {
    const entries = [...tasks.values()];
    const targetIndex = entries.indexOf(task);

    // Claude occasionally closes a later pending task at the end of a turn
    // without ever starting it. Keep the visible plan truthful and sequential:
    // every skipped predecessor and the target itself pass through in_progress.
    for (let index = 0; index < targetIndex; index++) {
      const previous = entries[index];
      if (previous.status === "completed") continue;
      if (previous.status !== "in_progress") {
        previous.status = "in_progress";
        previous.turnId = turnId;
        onIntermediate?.();
      }
      previous.status = "completed";
      previous.turnId = turnId;
      onIntermediate?.();
    }

    for (let index = 0; index < entries.length; index++) {
      const other = entries[index];
      if (other === task || other.status !== "in_progress") continue;
      other.status = index < targetIndex ? "completed" : "pending";
      other.turnId = turnId;
      onIntermediate?.();
    }

    if (status === "completed" && task.status !== "in_progress" && task.status !== "completed") {
      task.status = "in_progress";
      task.turnId = turnId;
      onIntermediate?.();
    }
    task.status = status;
  } else if (status) {
    task.status = status;
  }
  task.turnId = turnId;
  return true;
}

// TaskList에는 예전 계획까지 함께 들어올 수 있다. 마지막으로 번호가 1부터
// 시작한 묶음만 현재 계획으로 삼되, 번호가 없는 목록은 손실 없이 그대로 둔다.
function latestTaskPlan(tasks) {
  const entries = [...tasks.entries()];
  let start = 0;
  for (let index = 0; index < entries.length; index++) {
    if (numberedTaskIndex(entries[index][1].subject) === 1) start = index;
  }
  return new Map(entries.slice(start));
}

function updatePlanFromToolUse(session, name, toolUseId, input) {
  const turnId = session.turn?.id;
  if (name === "TaskCreate") {
    prepareTaskPlanForCreate(session.tasks, input.subject);
    session.tasks.set(`pending:${toolUseId}`, {
      id: `pending:${toolUseId}`,
      subject: input.subject || input.description || "작업",
      status: "pending",
      turnId,
    });
    session.planCreatePending = true;
    return;
  } else if (name === "TaskUpdate") {
    session.planCreatePending = false;
    applyTaskUpdate(session.tasks, input, turnId, () => emitPlan(session));
  }
  if (name === "TaskUpdate") emitPlan(session);
}

function flushPendingPlan(session) {
  if (!session.planCreatePending) return;
  session.planCreatePending = false;
  emitPlan(session);
}

function updatePlanFromToolResult(session, pending, message) {
  const value = message.tool_use_result;
  if (pending.name === "TaskCreate") {
    const temporary = session.tasks.get(`pending:${pending.toolUseId}`);
    const created = taskCreatedResult(value, message.message?.content);
    if (temporary && created?.id) {
      session.tasks.delete(temporary.id);
      temporary.id = String(created.id);
      temporary.subject = created.subject || temporary.subject;
      temporary.status = created.status || temporary.status;
      session.tasks.set(temporary.id, temporary);
    }
  } else if (pending.name === "TaskList" && Array.isArray(value?.tasks || value)) {
    const turnId = session.turn?.id;
    const previous = session.tasks;
    session.tasks = new Map();
    for (const task of value.tasks || value) {
      const id = String(task.id);
      session.tasks.set(id, {
        id,
        subject: task.subject || task.description || "작업",
        status: task.status || "pending",
        // 이 턴에서 다룬 적 없는 작업은 이전 턴의 것이므로 원래 turnId를 지켜 준다.
        turnId: previous.get(id)?.turnId ?? turnId,
      });
    }
    session.tasks = latestTaskPlan(session.tasks);
    emitPlan(session);
  }
}

function taskCreatedResult(structured, content) {
  const created = structured?.task || structured;
  if (created?.id) return created;
  const blocks = Array.isArray(content) ? content : [];
  const text = blocks.map((block) => toolOutput(block.content)).join("\n");
  const match = text.match(/Task #(\S+) created successfully(?::\s*(.+))?/i);
  return match ? { id: match[1], subject: match[2] } : null;
}

function planStatus(status) {
  if (status === "completed") return "completed";
  if (status === "in_progress" || status === "running") return "inProgress";
  return "pending";
}

function emitPlan(session) {
  // 빈 계획을 보내면 화면의 계획 카드가 사라진다. 보여줄 작업이 없을 때는 마지막 계획을 그대로 둔다.
  if (session.tasks.size === 0) return;
  notify("turn/plan/updated", {
    threadId: session.id,
    turnId: session.turn?.id,
    plan: [...session.tasks.values()].map((task, index) => ({
      step: numberedTaskSubject(task.subject, index),
      status: planStatus(task.status),
    })),
  });
}

// Claude의 Task 번호는 세션 전체에서 누적되므로 모델이 붙인 `4. `를 그대로 쓰면
// 다음 계획이 4번부터 시작한다. 지금 보여줄 목록 기준으로 항상 1번부터 다시 매긴다.
function numberedTaskSubject(subject, index) {
  const text = String(subject || "작업").trim().replace(/^\d+[.)]\s*/, "").trim();
  return `${index + 1}. ${text || "작업"}`;
}

// 서브에이전트는 자기 메시지를 부모 Task 툴콜의 `parent_tool_use_id`와 함께 흘려보낸다.
// 그 ID로 묶어 두면 지금 어떤 에이전트가 무슨 도구를 돌리는지 그대로 복원할 수 있다.
const SUBAGENT_TOOLS = ["Agent", "Task"];

function startSubagent(session, block) {
  const input = block.input || {};
  session.subagents.set(block.id, {
    id: block.id,
    taskId: "",
    background: false,
    name: firstLine(input.subagent_type || input.agentType || "agent", 40),
    description: firstLine(input.description || input.prompt || "", 120),
    tool: "",
    startedAt: Date.now(),
  });
  emitSubagents(session);
}

function isBackgroundSubagentResult(result) {
  return result?.isAsync === true || result?.status === "async_launched";
}

// Claude Code treats an async Agent result as a launch receipt. The agent remains
// live until its later task-notification names the same task or tool-use id.
function keepBackgroundSubagent(session, toolUseId, result) {
  if (!isBackgroundSubagentResult(result)) return false;
  const running = session.subagents.get(toolUseId);
  if (!running) return false;
  running.background = true;
  running.taskId = firstLine(result?.agentId || result?.taskId || "", 80);
  if (running.taskId) {
    session.knownSubagents.set(running.taskId, {
      name: running.name,
      description: running.description,
    });
  }
  return true;
}

// SendMessage can wake a completed agent from its transcript. Its new parent
// tool-use id owns this run, while the stable agent id lets later notifications
// and further resumes recover the original label.
function resumeBackgroundSubagent(session, toolUseId, pending, result) {
  const taskId = firstLine(result?.resumedAgentId || "", 80);
  if (!taskId) return false;
  const existing = [...session.subagents.values()].find((agent) => agent.taskId === taskId);
  if (existing) return true;
  const known = session.knownSubagents.get(taskId);
  const input = pending?.input || {};
  const running = {
    id: toolUseId,
    taskId,
    background: true,
    name: known?.name || "agent",
    description: known?.description || firstLine(input.summary || input.message || "", 120),
    tool: "",
    startedAt: Date.now(),
  };
  session.subagents.set(toolUseId, running);
  session.knownSubagents.set(taskId, {
    name: running.name,
    description: running.description,
  });
  emitSubagents(session);
  return true;
}

function findSubagent(session, id) {
  return session.subagents.get(id)
    || [...session.subagents.values()].find((agent) => agent.taskId === id);
}

// 서브에이전트가 실제로 무엇을 했는지는 자식 메시지에만 남는다. 열람용 기록은 여기서
// 한 줄씩 흘려보내고, 목록 행에 쓸 현재 도구만 따로 갱신한다.
function recordSubagentMessage(session, message) {
  const running = findSubagent(session, message.parent_tool_use_id);
  if (!running) return;
  const content = Array.isArray(message.message?.content) ? message.message.content : [];
  let toolChanged = false;
  for (const block of content) {
    if (block.type === "text") {
      const text = String(block.text || "").trim();
      if (text) emitSubagentLine(session, running.id, { kind: "text", text });
    } else if (block.type === "tool_use") {
      running.tool = subagentToolLabel(block);
      toolChanged = true;
      emitSubagentLine(session, running.id, {
        kind: "tool",
        text: running.tool,
        toolUseId: block.id,
      });
    }
  }
  if (toolChanged) emitSubagents(session);
}

function recordSubagentResult(session, message) {
  const running = findSubagent(session, message.parent_tool_use_id);
  if (!running) return;
  const content = Array.isArray(message.message?.content) ? message.message.content : [];
  for (const block of content) {
    if (block.type !== "tool_result") continue;
    emitSubagentLine(session, running.id, {
      kind: block.is_error ? "error" : "result",
      text: firstLine(toolOutput(block.content, message.tool_use_result), 200),
      toolUseId: block.tool_use_id,
    });
  }
}

function emitSubagentLine(session, parentToolUseId, line) {
  notify("turn/subagent/line", {
    threadId: session.id,
    turnId: session.turn?.id,
    parentToolUseId,
    line,
  });
}

function subagentToolLabel(block) {
  const name = block.name || "Tool";
  const input = block.input || {};
  const detail = input.command
    ?? input.pattern
    ?? input.file_path
    ?? input.description
    ?? input.query
    ?? input.url
    ?? "";
  const text = firstLine(detail, 60);
  return text ? `${name}(${text})` : name;
}

function finishSubagent(session, toolUseId) {
  if (!session.subagents.delete(toolUseId)) return;
  emitSubagents(session);
}

function finishSubagentTask(session, taskId) {
  if (!taskId) return false;
  const entry = [...session.subagents.entries()].find(([, agent]) => agent.taskId === taskId);
  if (!entry) return false;
  session.subagents.delete(entry[0]);
  emitSubagents(session);
  return true;
}

function clearForegroundSubagents(session) {
  let changed = false;
  for (const [id, agent] of session.subagents) {
    if (agent.background) continue;
    session.subagents.delete(id);
    changed = true;
  }
  if (changed) emitSubagents(session);
}

function clearSubagents(session) {
  if (!session.subagents.size) return;
  session.subagents.clear();
  emitSubagents(session);
}

function firstLine(value, limit) {
  return String(value ?? "").split("\n")[0].trim().slice(0, limit);
}

function emitSubagents(session) {
  notify("turn/subagents/updated", {
    threadId: session.id,
    turnId: session.turn?.id,
    subagents: [...session.subagents.values()].map((agent) => ({
      id: agent.id,
      name: agent.name,
      description: agent.description,
      tool: agent.tool,
      elapsedMs: Date.now() - agent.startedAt,
    })),
  });
}

function messageTextParts(message) {
  const content = message.message?.content;
  if (typeof content === "string") return [content];
  if (!Array.isArray(content)) return [];
  return content
    .filter((block) => block?.type === "text" && typeof block.text === "string")
    .map((block) => block.text);
}

function notificationTag(body, name) {
  const match = body.match(new RegExp(`<${name}>([\\s\\S]*?)</${name}>`));
  return match?.[1]?.trim() || "";
}

function taskNotifications(message) {
  if (message.origin?.kind !== "task-notification"
    && !messageTextParts(message).some((text) => text.includes("<task-notification>"))) {
    return [];
  }
  const notifications = [];
  for (const text of messageTextParts(message)) {
    for (const match of text.matchAll(/<task-notification>([\s\S]*?)<\/task-notification>/g)) {
      const body = match[1];
      notifications.push({
        taskId: notificationTag(body, "task-id"),
        toolUseId: notificationTag(body, "tool-use-id"),
        status: notificationTag(body, "status"),
        summary: notificationTag(body, "summary"),
      });
    }
  }
  return notifications;
}

function finishNotifiedSubagents(session, notifications) {
  for (const notification of notifications) {
    const byToolUse = notification.toolUseId
      ? session.subagents.get(notification.toolUseId)
      : null;
    const running = byToolUse || (notification.taskId
      ? [...session.subagents.values()].find((agent) => agent.taskId === notification.taskId)
      : null);
    if (!running) continue;
    emitSubagentLine(session, running.id, {
      kind: notification.status === "completed" ? "result" : "error",
      text: notification.summary || notification.status || "완료됨",
    });
    session.subagents.delete(running.id);
    emitSubagents(session);
  }
}

function processUser(session, message) {
  // 자식 tool_result의 tool_use_id는 부모 세션의 것과 다른 공간이므로, 부모 흐름에
  // 섞이기 전에 서브에이전트 기록으로 보낸다.
  if (message.parent_tool_use_id) {
    recordSubagentResult(session, message);
    return;
  }
  const notifications = taskNotifications(message);
  if (notifications.length) {
    if (!session.turn) beginTurn(session);
    finishNotifiedSubagents(session, notifications);
    return;
  }
  const content = Array.isArray(message.message?.content) ? message.message.content : [];
  for (const block of content) {
    if (block.type !== "tool_result") continue;
    const pending = session.tools.get(block.tool_use_id);
    const staysInBackground = SUBAGENT_TOOLS.includes(pending?.name)
      && keepBackgroundSubagent(session, block.tool_use_id, message.tool_use_result);
    if (!staysInBackground) finishSubagent(session, block.tool_use_id);
    if (pending?.name === "SendMessage") {
      resumeBackgroundSubagent(session, block.tool_use_id, pending, message.tool_use_result);
    } else if (pending?.name === "TaskStop" && message.tool_use_result?.success !== false) {
      finishSubagentTask(session, firstLine(pending.input?.task_id || "", 80));
    }
    if (!pending) continue;
    pending.toolUseId = block.tool_use_id;
    if (pending.suppressed) {
      updatePlanFromToolResult(session, pending, message);
      session.tools.delete(block.tool_use_id);
      continue;
    }
    const output = toolOutput(block.content, message.tool_use_result);
    const completed = { ...pending.item };
    if (completed.type === "commandExecution") {
      completed.status = block.is_error ? "failed" : "completed";
      completed.aggregatedOutput = output;
      const exitCode = message.tool_use_result?.exitCode ?? message.tool_use_result?.exit_code;
      if (Number.isInteger(exitCode)) completed.exitCode = exitCode;
    } else if (completed.type === "mcpToolCall") {
      if (block.is_error) completed.error = output;
      else completed.result = message.tool_use_result ?? output;
    } else if (completed.type === "dynamicToolCall") {
      completed.contentItems = message.tool_use_result ?? [{ type: "text", text: output }];
    } else {
      completed.result = message.tool_use_result ?? output;
    }
    emitItem(session, "completed", completed);
    session.tools.delete(block.tool_use_id);
  }
}

function toolOutput(content, structured) {
  if (typeof content === "string") return content;
  if (Array.isArray(content)) {
    return content.map((part) => typeof part === "string" ? part : part.text || JSON.stringify(part)).join("\n");
  }
  if (structured != null) return typeof structured === "string" ? structured : JSON.stringify(structured, null, 2);
  return content == null ? "" : JSON.stringify(content, null, 2);
}

async function processResult(session, message) {
  if (!session.turn) return;
  const interrupted = session.turn.interruptRequested === true;
  const totals = [...Object.values(message.modelUsage || {})].reduce((sum, usage) => ({
    inputTokens: sum.inputTokens + Number(usage.inputTokens || 0) + Number(usage.cacheReadInputTokens || 0) + Number(usage.cacheCreationInputTokens || 0),
    cachedInputTokens: sum.cachedInputTokens + Number(usage.cacheReadInputTokens || 0),
    cacheWriteInputTokens: sum.cacheWriteInputTokens + Number(usage.cacheCreationInputTokens || 0),
    outputTokens: sum.outputTokens + Number(usage.outputTokens || 0),
    totalTokens: sum.totalTokens + Number(usage.inputTokens || 0) + Number(usage.cacheReadInputTokens || 0) + Number(usage.cacheCreationInputTokens || 0) + Number(usage.outputTokens || 0),
    contextWindow: Math.max(sum.contextWindow, Number(usage.contextWindow || 0)),
  }), { inputTokens: 0, cachedInputTokens: 0, cacheWriteInputTokens: 0, outputTokens: 0, totalTokens: 0, contextWindow: 0 });
  notify("thread/tokenUsage/updated", {
    threadId: session.id,
    tokenUsage: {
      total: totals,
      ...(session.lastContextUsage ? { last: session.lastContextUsage } : {}),
      // `modelUsage` reports the window the turn actually ran under, so it wins
      // over the size guessed from the model name.
      modelContextWindow: totals.contextWindow || session.lastContextWindow || undefined,
    },
  });
  const error = message.is_error && !interrupted
    ? { message: message.errors?.join("\n") || message.stop_reason || "Claude 실행 실패" }
    : null;
  finishTurn(session, error, message.duration_ms);
  notify("claude/account/updated", {
    threadId: session.id,
    account: await safeAccount(session.query),
    usage: await safeUsage(session.query),
  });
  await runPendingPrompt(session);
}

// The turn that was waiting starts on its own, so the host sees it exactly like a
// prompt sent the moment the previous turn ended.
async function runPendingPrompt(session) {
  const next = session.pendingPrompts.shift();
  if (!next) return;
  try {
    await runPrompt(session, next);
  } catch (error) {
    notify("error", {
      threadId: session.id,
      provider: "Claude",
      error: { message: error instanceof Error ? error.message : String(error) },
      willRetry: false,
    });
    await runPendingPrompt(session);
  }
}

function finishTurn(session, error, durationMs) {
  if (!session.turn) return;
  flushPendingPlan(session);
  flushSmoothStreams(session);
  clearForegroundSubagents(session);
  const turn = { id: session.turn.id, status: error ? "failed" : "completed" };
  if (error) turn.error = { message: error instanceof Error ? error.message : error.message || String(error) };
  if (durationMs != null) turn.durationMs = durationMs;
  notify("turn/completed", { threadId: session.id, turn });
  session.turn = null;
  session.streamBlocks.clear();
}

async function consume(session) {
  for await (const message of session.query) {
    adoptSessionId(session, message.session_id);
    if (message.type === "stream_event") {
      if (message.event?.type === "content_block_delta" && (message.event?.delta?.text || message.event?.delta?.thinking)) {
        if (session.turn) session.turn.sawStreamText = true;
      }
      await processStreamEvent(session, message);
    } else if (message.type === "assistant") processAssistant(session, message);
    else if (message.type === "user") processUser(session, message);
    else if (message.type === "result") await processResult(session, message);
    else if (message.type === "system" && message.subtype === "compact_boundary") {
      notify("thread/compacted", { threadId: session.id });
    } else if (message.type === "rate_limit_event") {
      notify("claude/account/updated", { threadId: session.id, rateLimitInfo: message.rate_limit_info });
    } else if (message.type === "system" && message.subtype === "api_retry") {
      notify("warning", { threadId: session.id, provider: "Claude", message: `Claude API 재시도 ${message.attempt}/${message.max_retries}` });
    }
  }
}

const HANDOFF_HEADER = '<devez_provider_handoff chars="';
const HANDOFF_SEPARATOR = '">\n';
const HANDOFF_FOOTER = "\n</devez_provider_handoff>\n\n";

function prependHandoff(content, handoffContext) {
  if (!handoffContext) return content;
  const handoff = String(handoffContext);
  const prefix = `${HANDOFF_HEADER}${handoff.length}${HANDOFF_SEPARATOR}${handoff}${HANDOFF_FOOTER}`;
  const firstText = content.find((item) => item.type === "text");
  if (firstText) firstText.text = `${prefix}${firstText.text || ""}`;
  else content.unshift({ type: "text", text: prefix });
  return content;
}

function stripHandoff(text) {
  if (!text.startsWith(HANDOFF_HEADER)) return text;
  const separator = text.indexOf(HANDOFF_SEPARATOR, HANDOFF_HEADER.length);
  if (separator < 0) return text;
  const length = Number(text.slice(HANDOFF_HEADER.length, separator));
  if (!Number.isSafeInteger(length) || length < 0) return text;
  const contextStart = separator + HANDOFF_SEPARATOR.length;
  const contextEnd = contextStart + length;
  if (text.slice(contextEnd, contextEnd + HANDOFF_FOOTER.length) !== HANDOFF_FOOTER) return text;
  return text.slice(contextEnd + HANDOFF_FOOTER.length);
}

async function inputContent(input, handoffContext) {
  const content = [];
  for (const item of Array.isArray(input) ? input : []) {
    if (item.type === "text") content.push({ type: "text", text: item.text || "" });
    else if (item.type === "localImage" && item.path) {
      const bytes = await readFile(item.path);
      const extension = item.path.split(".").pop()?.toLowerCase();
      const mediaType = extension === "png" ? "image/png"
        : extension === "gif" ? "image/gif"
        : extension === "webp" ? "image/webp"
        : "image/jpeg";
      content.push({ type: "image", source: { type: "base64", media_type: mediaType, data: bytes.toString("base64") } });
    }
  }
  return prependHandoff(content.length ? content : [{ type: "text", text: "" }], handoffContext);
}

async function startPrompt(params) {
  const id = liveSessionId(params.sessionId);
  const session = sessions.get(id);
  if (!session) throw new Error(`Claude 세션을 찾을 수 없습니다: ${id}`);
  // Claude runs one turn at a time, so extra input waits its turn instead of
  // failing — the same queueing the CLI does for a prompt typed while it works.
  if (session.turn) {
    session.pendingPrompts.push(params);
    return { turn: { id: session.turn.id }, queued: true };
  }
  return runPrompt(session, params);
}

async function runPrompt(session, params) {
  const id = session.id;
  if (params.model) {
    const model = stripClaudeModel(params.model);
    await session.query.setModel(model);
    session.model = visibleModel(params.model);
  }
  const effort = supportedEffort(modelCapabilities(session.models, params.model || session.model), params.effort);
  if (effort) {
    await session.query.applyFlagSettings({ effortLevel: effort });
  }
  session.effort = effort;
  await applyPermissionMode(session, params.permissionMode);
  const content = await inputContent(params.input, params.handoffContext);
  const turnId = beginTurn(session, params.input);
  session.queue.push({
    type: "user",
    message: { role: "user", content },
    parent_tool_use_id: null,
    session_id: id,
    origin: { kind: "human" },
  });
  return { turn: { id: turnId } };
}

// A background task notification is an internal user message that starts its
// own Claude response even though the host did not submit a new prompt.
function beginTurn(session, input = []) {
  const turnId = `claude-turn-${session.turnSequence++}-${randomUUID()}`;
  session.turn = {
    id: turnId,
    sawStreamText: false,
    sawVisibleText: false,
    koreanRequest: isKoreanPrompt(input),
    openingNotice: openingNotice(input),
    openingNoticeEmitted: false,
  };
  session.lastContextUsage = null;
  notify("turn/started", { threadId: session.id, turn: { id: turnId } });
  return turnId;
}

function contentBlocks(message) {
  const content = message?.content;
  if (typeof content === "string") return [{ type: "text", text: content }];
  return Array.isArray(content) ? content : [];
}

function isInternalHistoryText(message, text) {
  if (message.isMeta || message.subtype === "local_command") return true;
  const trimmed = text.trim();
  const tag = trimmed.match(/^<([a-z0-9-]+)>/i)?.[1]?.toLowerCase();
  return trimmed === "[Request interrupted by user]"
    || [
      "bash-input",
      "bash-stdout",
      "bash-stderr",
      "command-name",
      "local-command-caveat",
      "local-command-stdout",
      "local-command-stderr",
      "task-notification",
    ].includes(tag);
}

function historyState(messages) {
  const turns = [];
  let turn = null;
  const tools = new Map();
  const tasks = new Map();
  for (const message of messages) {
    const blocks = contentBlocks(message.message);
    const userText = message.type === "user"
      ? stripHandoff(blocks.filter((block) => block.type === "text").map((block) => block.text || "").join("\n"))
      : "";
    if (userText
      && !blocks.some((block) => block.type === "tool_result")
      && !isInternalHistoryText(message, userText)) {
      turn = {
        id: `claude-turn-${message.uuid}`,
        status: "completed",
        items: [{ id: `claude-user-${message.uuid}`, type: "userMessage", content: [{ type: "text", text: userText }] }],
      };
      turns.push(turn);
    }
    if (!turn) continue;
    if (message.type === "assistant") {
      if (message.message?.model === "<synthetic>") {
        turn.synthetic = true;
        continue;
      }
      turn.synthetic = false;
      if (!turn.model && message.message?.model) {
        turn.model = visibleModel(message.message.model);
        const prompt = turn.items.find((item) => item.type === "userMessage");
        if (prompt) prompt.model = turn.model;
      }
      for (const block of blocks) {
        if (block.type === "text") turn.items.push({ id: `${message.uuid}-text`, type: "agentMessage", text: block.text || "", provider: "Claude" });
        else if (block.type === "thinking") turn.items.push({ id: `${message.uuid}-thinking`, type: "reasoning", summary: [block.thinking || ""] });
        else if (block.type === "tool_use") {
          const pending = { name: block.name, input: block.input || {}, item: toolItem({}, block.id, block.name, block.input || {}) };
          tools.set(block.id, pending);
          if (block.name === "TaskCreate") {
            prepareTaskPlanForCreate(tasks, block.input?.subject);
            tasks.set(`pending:${block.id}`, { id: `pending:${block.id}`, subject: block.input?.subject || "작업", status: "pending", turnId: turn.id });
          } else if (block.name === "TaskUpdate") {
            applyTaskUpdate(tasks, block.input || {}, turn.id);
          } else if (!["TaskList", "AskUserQuestion"].includes(block.name)) turn.items.push(pending.item);
        }
      }
    } else if (message.type === "user") {
      for (const block of blocks.filter((candidate) => candidate.type === "tool_result")) {
        const pending = tools.get(block.tool_use_id);
        if (!pending) continue;
        if (pending.name === "TaskCreate") {
          const temporary = tasks.get(`pending:${block.tool_use_id}`);
          const created = taskCreatedResult(message.tool_use_result, [block]);
          if (temporary && created?.id) {
            tasks.delete(temporary.id);
            temporary.id = String(created.id);
            temporary.subject = created.subject || temporary.subject;
            tasks.set(temporary.id, temporary);
          }
        } else if (pending.name === "TaskList" && Array.isArray(message.tool_use_result?.tasks)) {
          const known = new Map(tasks);
          tasks.clear();
          for (const task of message.tool_use_result.tasks) tasks.set(String(task.id), { id: String(task.id), subject: task.subject || "작업", status: task.status || "pending", turnId: known.get(String(task.id))?.turnId ?? turn.id });
          const current = latestTaskPlan(tasks);
          tasks.clear();
          for (const [id, task] of current) tasks.set(id, task);
        } else if (pending.item) {
          const output = toolOutput(block.content, message.tool_use_result);
          Object.assign(pending.item, pending.item.type === "commandExecution"
            ? { status: block.is_error ? "failed" : "completed", aggregatedOutput: output }
            : pending.item.type === "mcpToolCall" ? { result: message.tool_use_result ?? output }
            : pending.item.type === "dynamicToolCall" ? { contentItems: message.tool_use_result ?? [{ type: "text", text: output }] }
            : { result: message.tool_use_result ?? output });
        }
      }
    }
  }
  if (turn && tasks.size) {
    const text = [...tasks.values()].map((task, index) => `${task.status === "completed" ? "✓" : task.status === "in_progress" ? "▸" : "□"} ${numberedTaskSubject(task.subject, index)}`).join("\n");
    turn.items.push({ id: "claude-plan-latest", type: "plan", text });
  }
  return {
    tasks,
    turns: turns
    .filter((candidate) => !candidate.synthetic)
    .map(({ synthetic: _, ...candidate }) => candidate),
  };
}

function historyTurns(messages) {
  return historyState(messages).turns;
}

// A transcript lives in a folder encoded from the cwd string, so two spellings of
// the same path (Windows differs only in case) resolve to different folders and a
// session recorded under one spelling is invisible to the other. Remember the
// spelling the transcript was written with, keyed by session id.
const transcriptCwds = new Map();

function claudeProjectsDir() {
  return join(process.env.CLAUDE_CONFIG_DIR || join(homedir(), ".claude"), "projects");
}

/** Read the cwd a transcript records. Only the head is scanned: the opening
 * records carry no cwd, but a real turn shows up long before the limit. */
async function readTranscriptCwd(path, limit = 200) {
  const stream = createReadStream(path, { encoding: "utf8" });
  try {
    const reader = createInterface({ input: stream, crlfDelay: Infinity });
    try {
      let seen = 0;
      for await (const line of reader) {
        if (++seen > limit) break;
        let cwd;
        try {
          cwd = JSON.parse(line)?.cwd;
        } catch {
          continue;
        }
        if (typeof cwd === "string" && cwd) return cwd;
      }
      return null;
    } finally {
      reader.close();
    }
  } finally {
    stream.destroy();
  }
}

/** Locate the transcript of `id` under any project folder and report the cwd it
 * records, which is the spelling the SDK needs to find it again. */
async function transcriptCwd(id) {
  if (transcriptCwds.has(id)) return transcriptCwds.get(id);
  let entries;
  try {
    entries = await readdir(claudeProjectsDir(), { withFileTypes: true });
  } catch {
    return null;
  }
  for (const entry of entries) {
    if (!entry.isDirectory()) continue;
    let cwd;
    try {
      cwd = await readTranscriptCwd(join(claudeProjectsDir(), entry.name, `${id}.jsonl`));
    } catch {
      continue;
    }
    if (cwd) {
      transcriptCwds.set(id, cwd);
      return cwd;
    }
  }
  return null;
}

/** The cwd to read `id`'s transcript with: the one the transcript itself records,
 * falling back to the host's when no transcript is on disk. */
async function readableCwd(id, cwd) {
  return await transcriptCwd(id) || cwd;
}

async function dispatch(method, params = {}) {
  if (method === "model/list") return loadModelCatalog(params);
  if (method === "session/permissionMode") {
    const session = lookupSession(params.sessionId);
    // A session that has not started yet picks the mode up from its first turn.
    if (!session) return { permissionMode: permissionMode(params.permissionMode) };
    await applyPermissionMode(session, params.permissionMode);
    return { permissionMode: session.permissionMode };
  }
  if (method === "session/start") {
    const { session, account, usage } = await createSession(params);
    return {
      id: session.id,
      thread: { id: session.id, turns: [] },
      cwd: session.cwd,
      model: session.model,
      reasoningEffort: session.effort,
      account,
      usage,
    };
  }
  if (method === "session/resume") {
    const id = liveSessionId(params.sessionId);
    const existing = sessions.get(id);
    if (existing) {
      const messages = await getSessionMessages(id, { dir: existing.cwd, includeSystemMessages: true });
      if (!existing.tasks.size) existing.tasks = historyState(messages).tasks;
      return {
        id,
        thread: { id, turns: [] },
        initialTurnsPage: { data: [], nextCursor: null },
        cwd: existing.cwd,
        model: existing.model,
        reasoningEffort: existing.effort,
        account: await safeAccount(existing.query),
        usage: await safeUsage(existing.query),
        tokenUsage: historyTokenUsage(messages, existing.models, existing.model),
      };
    }
    const dir = await readableCwd(id, params.cwd);
    // The transcript itself decides whether there is anything to resume:
    // getSessionInfo only sees sessions the CLI indexed, and a bridge-run session
    // whose transcript is intact can be missing from that index.
    const messages = await getSessionMessages(id, { dir, includeSystemMessages: true });
    if (!messages.length) throw new Error(`Claude 세션을 찾을 수 없습니다: ${id}`);
    const info = await getSessionInfo(id, { dir });
    const lastModel = [...messages].reverse().find((message) => message.type === "assistant")?.message?.model;
    // The transcript's own model outranks the host's fallback, which is only what
    // a new session would have opened on.
    const { session, account, usage } = await createSession({
      ...params,
      cwd: info?.cwd || dir,
      model: params.model || lastModel || params.fallbackModel,
      effort: params.effort || params.fallbackEffort,
    }, id);
    const tokenUsage = historyTokenUsage(messages, session.models, session.model);
    // Seed the live session so the next turn keeps reporting a full context.
    session.lastContextUsage = tokenUsage?.last || null;
    session.lastContextWindow = tokenUsage?.modelContextWindow || 0;
    session.tasks = historyState(messages).tasks;
    return {
      id,
      thread: { id, turns: [] },
      initialTurnsPage: { data: [], nextCursor: null },
      cwd: session.cwd,
      model: session.model,
      reasoningEffort: session.effort,
      account,
      usage,
      tokenUsage,
    };
  }
  if (method === "session/list") {
    const found = await listSessions({
      dir: params.cwd,
      limit: params.limit || 100,
      offset: params.offset || 0,
      includeProgrammatic: true,
    });
    return {
      data: found.map((session) => ({
        id: visibleSession(session.sessionId),
        name: session.customTitle || undefined,
        preview: session.summary || session.firstPrompt || "Untitled Claude session",
        cwd: session.cwd || params.cwd || "",
        updatedAt: Math.floor((session.lastModified || 0) / 1000),
      })),
      nextCursor: null,
    };
  }
  if (method === "session/history") {
    const id = liveSessionId(params.sessionId);
    const messages = await getSessionMessages(id, { dir: await readableCwd(id, params.cwd), includeSystemMessages: true });
    return { data: historyTurns(messages), nextCursor: null };
  }
  if (method === "session/prompt") return startPrompt(params);
  if (method === "session/interrupt") {
    const session = lookupSession(params.sessionId);
    // Stopping the run drops what was waiting behind it too, so nothing the user
    // just cancelled starts on its own afterwards.
    if (session) session.pendingPrompts.length = 0;
    if (session?.turn) {
      const turn = session.turn;
      turn.interruptRequested = true;
      try {
        await session.query.interrupt();
      } catch (error) {
        if (session.turn === turn) delete turn.interruptRequested;
        throw error;
      }
    }
    return {};
  }
  if (method === "session/compact") {
    return startPrompt({ ...params, input: [{ type: "text", text: "/compact" }] });
  }
  if (method === "session/fork") {
    const source = liveSessionId(params.sessionId);
    const forked = await forkSession(source, { dir: await readableCwd(source, params.cwd) });
    const id = forked.sessionId || forked;
    const { session, account, usage } = await createSession(params, id);
    return {
      id,
      thread: { id, turns: [] },
      cwd: session.cwd,
      model: session.model,
      reasoningEffort: session.effort,
      account,
      usage,
    };
  }
  if (method === "session/close") {
    const session = lookupSession(params.sessionId);
    if (session) {
      session.queue.close();
      session.query.close();
      sessions.delete(session.id);
      for (const [from, to] of sessionAliases) {
        if (to === session.id) sessionAliases.delete(from);
      }
      if (params.delete) await deleteSession(session.id, { dir: session.cwd });
    }
    return {};
  }
  if (method === "account/usage") {
    const session = lookupSession(params.sessionId) || [...sessions.values()][0];
    if (!session) return { account: null, usage: null };
    return { account: await safeAccount(session.query), usage: await safeUsage(session.query) };
  }
  if (method === "shutdown") {
    for (const session of sessions.values()) {
      session.queue.close();
      session.query.close();
    }
    sessions.clear();
    sessionAliases.clear();
    return {};
  }
  throw new Error(`지원하지 않는 Claude 브리지 메서드: ${method}`);
}

async function runSelfTest() {
  const user = (uuid, text) => ({
    type: "user",
    uuid,
    message: { role: "user", content: [{ type: "text", text }] },
    origin: { kind: "human" },
  });
  const assistant = (uuid, model, text) => ({
    type: "assistant",
    uuid,
    message: { role: "assistant", model, content: [{ type: "text", text }] },
  });
  const turns = historyTurns([
    user("u1", "say hi"),
    assistant("a1", "claude-opus-5", "hi."),
    user("command", "<command-name>/model</command-name>"),
    user("stdout", "<local-command-stdout>Set model to sonnet</local-command-stdout>"),
    user("u2", "say hello"),
    assistant("a2", "claude-sonnet-5", "Hello."),
    user("bash", "<bash-stdout>hidden</bash-stdout>"),
    user("u3", "hay zzz"),
    assistant("a3", "claude-haiku-4-5-20251001", "hey."),
    user("synthetic-user", "duplicate"),
    assistant("synthetic", "<synthetic>", "No response requested."),
  ]);
  const prompts = turns.map((turn) => turn.items.find((item) => item.type === "userMessage"));
  const expected = [
    ["say hi", "claude:claude-opus-5"],
    ["say hello", "claude:claude-sonnet-5"],
    ["hay zzz", "claude:claude-haiku-4-5-20251001"],
  ];
  if (turns.length !== expected.length
    || prompts.some((prompt, index) => prompt?.content?.[0]?.text !== expected[index][0]
      || prompt.model !== expected[index][1])) {
    throw new Error(`Claude history self-test failed: ${JSON.stringify(turns)}`);
  }
  const taskUse = (uuid, id, name, input) => ({
    type: "assistant",
    uuid,
    message: {
      role: "assistant",
      model: "claude-opus-5",
      content: [{ type: "tool_use", id, name, input }],
    },
  });
  const taskResult = (uuid, id, toolUseResult, content) => ({
    type: "user",
    uuid,
    message: { role: "user", content: [{ type: "tool_result", tool_use_id: id, content }] },
    tool_use_result: toolUseResult,
  });
  const taskMessages = [user("plan", "작업을 진행해")];
  for (let index = 1; index <= 6; index++) {
    const id = String(24 + index);
    const toolId = `create-${id}`;
    const subject = `${index}. 작업 ${index}`;
    taskMessages.push(
      taskUse(`create-use-${id}`, toolId, "TaskCreate", { subject }),
      taskResult(`create-result-${id}`, toolId, { task: { id, subject } }, `Task #${id} created successfully: ${subject}`),
    );
  }
  taskMessages.push(
    taskUse("update-25-start", "update-25-start", "TaskUpdate", { taskId: "25", status: "in_progress" }),
    taskUse("update-25-done", "update-25-done", "TaskUpdate", { taskId: "25", status: "completed" }),
    taskUse("update-26-start", "update-26-start", "TaskUpdate", { taskId: "26", status: "in_progress" }),
  );
  const restoredTasks = historyState(taskMessages).tasks;
  if ([...restoredTasks.keys()].join(",") !== "25,26,27,28,29,30"
    || restoredTasks.get("25")?.status !== "completed"
    || restoredTasks.get("26")?.status !== "in_progress"
    || [...restoredTasks.values()].filter((task) => task.status === "in_progress").length !== 1) {
    throw new Error(`Claude task resume self-test failed: ${JSON.stringify([...restoredTasks])}`);
  }
  applyTaskUpdate(restoredTasks, { taskId: "26", status: "completed" }, "resumed-turn");
  applyTaskUpdate(restoredTasks, { taskId: "27", status: "in_progress" }, "resumed-turn");
  if (restoredTasks.size !== 6
    || restoredTasks.get("25")?.status !== "completed"
    || restoredTasks.get("26")?.status !== "completed"
    || restoredTasks.get("27")?.status !== "in_progress"
    || [...restoredTasks.values()].filter((task) => task.status === "in_progress").length !== 1) {
    throw new Error(`Claude sequential task update self-test failed: ${JSON.stringify([...restoredTasks])}`);
  }
  const skippedTasks = new Map([
    ["1", { id: "1", subject: "1. 조사", status: "pending" }],
    ["2", { id: "2", subject: "2. 분석", status: "pending" }],
    ["3", { id: "3", subject: "3. 검증", status: "pending" }],
  ]);
  const transitions = [];
  const snapshot = () => transitions.push([...skippedTasks.values()].map((task) => task.status).join(","));
  applyTaskUpdate(skippedTasks, { taskId: "3", status: "completed" }, "turn", snapshot);
  snapshot();
  if (transitions.join("|") !== [
    "in_progress,pending,pending",
    "completed,pending,pending",
    "completed,in_progress,pending",
    "completed,completed,pending",
    "completed,completed,in_progress",
    "completed,completed,completed",
  ].join("|")) {
    throw new Error(`Claude skipped task transition self-test failed: ${transitions.join("|")}`);
  }
  const mixedPlans = new Map([
    ["old-1", { subject: "1. 이전 작업", status: "completed" }],
    ["old-2", { subject: "2. 이전 검증", status: "completed" }],
    ...[...restoredTasks],
  ]);
  if ([...latestTaskPlan(mixedPlans).keys()].join(",") !== "25,26,27,28,29,30") {
    throw new Error(`Claude latest task plan self-test failed: ${JSON.stringify([...mixedPlans])}`);
  }
  prepareTaskPlanForCreate(restoredTasks, "7. 추가 작업");
  if (restoredTasks.size !== 6) throw new Error("Claude appended task unexpectedly reset the plan");
  prepareTaskPlanForCreate(restoredTasks, "1. 새 작업");
  if (restoredTasks.size !== 0) throw new Error("Claude new task plan did not reset the previous plan");
  const batchedPlanSession = {
    id: "batched-plan-self-test",
    turn: { id: "batched-plan-turn" },
    tasks: new Map(),
    planCreatePending: false,
  };
  const batchedPlanCaptured = [];
  const batchedPlanWrite = process.stdout.write;
  process.stdout.write = (chunk) => {
    batchedPlanCaptured.push(String(chunk));
    return true;
  };
  try {
    for (let index = 1; index <= 3; index++) {
      const toolUseId = `batched-create-${index}`;
      const subject = `${index}. 작업 ${index}`;
      updatePlanFromToolUse(batchedPlanSession, "TaskCreate", toolUseId, { subject });
      updatePlanFromToolResult(
        batchedPlanSession,
        { name: "TaskCreate", toolUseId },
        { tool_use_result: { task: { id: String(index), subject } } },
      );
    }
    updatePlanFromToolUse(
      batchedPlanSession,
      "TaskUpdate",
      "batched-update-1",
      { taskId: "1", status: "in_progress" },
    );
  } finally {
    process.stdout.write = batchedPlanWrite;
  }
  const batchedPlanEvents = batchedPlanCaptured
    .join("")
    .trim()
    .split("\n")
    .filter(Boolean)
    .map((line) => JSON.parse(line))
    .filter((event) => event.method === "turn/plan/updated");
  if (batchedPlanEvents.length !== 1
    || batchedPlanEvents[0].params?.plan?.length !== 3
    || batchedPlanEvents[0].params.plan[0]?.status !== "inProgress") {
    throw new Error(`Claude batched plan self-test failed: ${JSON.stringify(batchedPlanEvents)}`);
  }
  const usage = tokenBreakdown({
    input_tokens: 2,
    cache_read_input_tokens: 68_000,
    cache_creation_input_tokens: 500,
    output_tokens: 300,
  });
  if (usage.totalTokens !== 68_802 || usage.inputTokens !== 68_502) {
    throw new Error(`Claude usage self-test failed: ${JSON.stringify(usage)}`);
  }
  const windows = [
    catalogEntry({ value: "opus[1m]", resolvedModel: "claude-opus-5[1m]" }, "").contextWindow,
    catalogEntry({ value: "sonnet", resolvedModel: "claude-sonnet-5" }, "").contextWindow,
    catalogEntry({ value: "haiku", resolvedModel: "x", contextWindow: 300_000 }, "").contextWindow,
  ];
  if (windows.join(",") !== "1000000,200000,300000") {
    throw new Error(`Claude context window self-test failed: ${windows.join(",")}`);
  }
  const notification = taskNotifications({
    origin: { kind: "task-notification" },
    message: {
      content: `<task-notification>
<task-id>agent-1</task-id>
<tool-use-id>toolu_1</tool-use-id>
<status>completed</status>
<summary>Agent "Explore" finished</summary>
<result>done</result>
</task-notification>`,
    },
  });
  if (notification.length !== 1
    || notification[0].taskId !== "agent-1"
    || notification[0].toolUseId !== "toolu_1"
    || notification[0].status !== "completed"
    || notification[0].summary !== 'Agent "Explore" finished') {
    throw new Error(`Claude task notification self-test failed: ${JSON.stringify(notification)}`);
  }
  const lifecycleSession = {
    id: "self-test-session",
    turn: { id: "parent-turn", sawStreamText: false },
    turnSequence: 1,
    streamBlocks: new Map(),
    tools: new Map([[
      "toolu_1",
      {
        name: "Agent",
        input: { subagent_type: "Explore", description: "Inspect files" },
        item: { id: "toolu_1", type: "collabAgentToolCall", tool: { name: "Agent", arguments: {} } },
      },
    ]]),
    subagents: new Map([[
      "toolu_1",
      {
        id: "toolu_1",
        taskId: "",
        background: false,
        name: "Explore",
        description: "Inspect files",
        tool: "",
        startedAt: Date.now(),
      },
    ]]),
    knownSubagents: new Map(),
    lastContextUsage: null,
  };
  const captured = [];
  const stdoutWrite = process.stdout.write;
  process.stdout.write = (chunk) => {
    captured.push(String(chunk));
    return true;
  };
  try {
    processUser(lifecycleSession, {
      message: { content: [{ type: "tool_result", tool_use_id: "toolu_1", content: "launched" }] },
      tool_use_result: { isAsync: true, status: "async_launched", agentId: "agent-1" },
    });
    finishTurn(lifecycleSession, null, 1);
    if (!lifecycleSession.subagents.has("toolu_1") || lifecycleSession.turn !== null) {
      throw new Error("Claude background subagent did not survive its parent turn");
    }
    processUser(lifecycleSession, {
      origin: { kind: "task-notification" },
      message: { content: notification[0] && `<task-notification>
<task-id>agent-1</task-id><tool-use-id>toolu_1</tool-use-id>
<status>completed</status><summary>Agent finished</summary>
</task-notification>` },
    });
    if (lifecycleSession.subagents.size !== 0 || lifecycleSession.turn === null) {
      throw new Error("Claude task notification did not finish the agent in an automatic turn");
    }
    finishTurn(lifecycleSession, null, 1);

    beginTurn(lifecycleSession);
    lifecycleSession.tools.set("toolu_2", {
      name: "SendMessage",
      input: { to: "agent-1", summary: "Continue inspection" },
      item: { id: "toolu_2", type: "dynamicToolCall", tool: "SendMessage", arguments: {} },
    });
    processUser(lifecycleSession, {
      message: { content: [{ type: "tool_result", tool_use_id: "toolu_2", content: "resumed" }] },
      tool_use_result: { success: true, resumedAgentId: "agent-1" },
    });
    const resumed = lifecycleSession.subagents.get("toolu_2");
    if (!resumed?.background || resumed.taskId !== "agent-1" || resumed.name !== "Explore") {
      throw new Error(`Claude resumed subagent self-test failed: ${JSON.stringify(resumed)}`);
    }
    finishTurn(lifecycleSession, null, 1);
    processUser(lifecycleSession, {
      origin: { kind: "task-notification" },
      message: { content: `<task-notification>
<task-id>agent-1</task-id><tool-use-id>toolu_2</tool-use-id>
<status>failed</status><summary>Agent failed</summary>
</task-notification>` },
    });
    if (lifecycleSession.subagents.size !== 0 || lifecycleSession.turn === null) {
      throw new Error("Claude failed task notification did not finish the resumed agent");
    }
    finishTurn(lifecycleSession, null, 1);
  } finally {
    process.stdout.write = stdoutWrite;
  }
  const lifecycleEvents = captured
    .join("")
    .trim()
    .split("\n")
    .filter(Boolean)
    .map((line) => JSON.parse(line));
  const lifecycleMethods = lifecycleEvents.map((event) => event.method);
  if (!lifecycleMethods.includes("turn/subagents/updated")
    || lifecycleMethods.filter((method) => method === "turn/started").length < 3
    || !lifecycleEvents.some((event) => event.method === "turn/subagent/line"
      && event.params?.line?.kind === "error")) {
    throw new Error(`Claude subagent lifecycle events self-test failed: ${lifecycleMethods}`);
  }
  const openingSession = {
    id: "opening-self-test",
    model: "claude:default",
    models: [],
    turn: null,
    turnSequence: 1,
    itemSequence: 1,
    streamBlocks: new Map(),
    tools: new Map(),
    tasks: new Map(),
    subagents: new Map(),
    knownSubagents: new Map(),
    lastContextUsage: null,
    lastContextWindow: 0,
  };
  const openingCaptured = [];
  process.stdout.write = (chunk) => {
    openingCaptured.push(String(chunk));
    return true;
  };
  try {
    beginTurn(openingSession, [{ type: "text", text: "provider 메뉴를 수정해" }]);
    processAssistant(openingSession, {
      message: {
        content: [{ type: "tool_use", id: "read-1", name: "Read", input: { file_path: "src/main.rs" } }],
      },
    });
  } finally {
    process.stdout.write = stdoutWrite;
  }
  const openingEvents = openingCaptured
    .join("")
    .trim()
    .split("\n")
    .filter(Boolean)
    .map((line) => JSON.parse(line));
  const openingMessageIndex = openingEvents.findIndex((event) =>
    event.method === "item/completed"
      && event.params?.item?.type === "agentMessage"
      && event.params.item.text === "요청 내용을 확인하고 필요한 작업을 진행하겠습니다.");
  const openingToolIndex = openingEvents.findIndex((event) =>
    event.method === "item/started" && event.params?.item?.type === "dynamicToolCall");
  if (openingMessageIndex < 0 || openingToolIndex < 0 || openingMessageIndex > openingToolIndex) {
    throw new Error(`Claude opening notice order self-test failed: ${JSON.stringify(openingEvents)}`);
  }
  const languageCaptured = [];
  process.stdout.write = (chunk) => {
    languageCaptured.push(String(chunk));
    return true;
  };
  try {
    openingSession.streamBlocks.clear();
    await processStreamEvent(openingSession, {
      event: { type: "content_block_start", index: 0, content_block: { type: "text" } },
    });
    await processStreamEvent(openingSession, {
      event: { type: "content_block_delta", index: 0, delta: { text: "Now the tile view logic." } },
    });
    await processStreamEvent(openingSession, {
      event: { type: "content_block_stop", index: 0 },
    });
  } finally {
    process.stdout.write = stdoutWrite;
  }
  const languageEvents = languageCaptured
    .join("")
    .trim()
    .split("\n")
    .filter(Boolean)
    .map((line) => JSON.parse(line));
  const visibleLanguage = languageEvents
    .filter((event) => event.method === "item/agentMessage/delta")
    .map((event) => event.params?.delta || "")
    .join("");
  if (visibleLanguage !== ""
    || languageEvents.length
    || languageEvents.some((event) => JSON.stringify(event).includes("Now the tile view logic."))) {
    throw new Error(`Claude Korean progress normalization self-test failed: ${JSON.stringify(languageEvents)}`);
  }
  const keptCaptured = [];
  process.stdout.write = (chunk) => {
    keptCaptured.push(String(chunk));
    return true;
  };
  try {
    openingSession.streamBlocks.clear();
    await processStreamEvent(openingSession, {
      event: { type: "content_block_start", index: 0, content_block: { type: "text" } },
    });
    await processStreamEvent(openingSession, {
      event: { type: "content_block_delta", index: 0, delta: { text: "타일 보기 로직을 고쳤습니다." } },
    });
    await processStreamEvent(openingSession, {
      event: { type: "content_block_stop", index: 0 },
    });
  } finally {
    process.stdout.write = stdoutWrite;
  }
  const keptEvents = keptCaptured
    .join("")
    .trim()
    .split("\n")
    .filter(Boolean)
    .map((line) => JSON.parse(line));
  const keptStarted = keptEvents.findIndex((event) =>
    event.method === "item/started" && event.params?.item?.type === "agentMessage");
  const keptCompleted = keptEvents.find((event) =>
    event.method === "item/completed" && event.params?.item?.type === "agentMessage");
  if (keptStarted !== 0 || keptCompleted?.params?.item?.text !== "타일 보기 로직을 고쳤습니다.") {
    throw new Error(`Claude held Korean text self-test failed: ${JSON.stringify(keptEvents)}`);
  }
  const smoothText = "Claude가 👨‍👩‍👧‍👦 한 문장을 한꺼번에 보내도 부드럽게 표시합니다.";
  const emitted = [];
  const smooth = new SmoothTextStream((chunk) => emitted.push(chunk), 0);
  smooth.push(smoothText);
  if (emitted.length !== 0) {
    throw new Error(`Claude smooth stream did not batch its first frame: ${JSON.stringify(emitted)}`);
  }
  await smooth.finish();
  if (emitted.length < 2 || emitted.join("") !== smoothText) {
    throw new Error(`Claude smooth stream self-test failed: ${JSON.stringify(emitted)}`);
  }
  const flushed = [];
  const interrupted = new SmoothTextStream((chunk) => flushed.push(chunk), 1000);
  interrupted.push(smoothText);
  interrupted.flush();
  await interrupted.finish();
  if (flushed.join("") !== smoothText) {
    throw new Error(`Claude smooth stream flush self-test failed: ${JSON.stringify(flushed)}`);
  }
  process.stdout.write("Claude bridge self-test passed\n");
}

if (process.argv.includes("--self-test")) {
  await runSelfTest();
  process.exit(0);
}

const lines = createInterface({ input: process.stdin, crlfDelay: Infinity });
lines.on("line", async (line) => {
  if (!line.trim()) return;
  let message;
  try { message = JSON.parse(line); }
  catch (error) {
    write({ method: "warning", params: { provider: "Claude", message: `Claude 브리지 JSON 해석 실패: ${error.message}` } });
    return;
  }
  if (typeof message.id === "string" && ("result" in message || "error" in message)) {
    const pending = pendingHostRequests.get(message.id);
    if (pending) {
      pendingHostRequests.delete(message.id);
      pending.resolve(message.result ?? { decision: "decline" });
    }
    return;
  }
  if (typeof message.id !== "number" || typeof message.method !== "string") return;
  try { write({ id: message.id, result: await dispatch(message.method, message.params) }); }
  catch (error) { write({ id: message.id, error: rpcError(error) }); }
});

lines.on("close", () => {
  for (const session of sessions.values()) session.query.close();
});
