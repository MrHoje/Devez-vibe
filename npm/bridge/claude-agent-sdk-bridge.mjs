#!/usr/bin/env node

import { randomUUID } from "node:crypto";
import { readFile } from "node:fs/promises";
import { createInterface } from "node:readline";
import {
  deleteSession,
  forkSession,
  getSessionInfo,
  getSessionMessages,
  listSessions,
  query,
} from "@anthropic-ai/claude-agent-sdk";

const VERSION = process.env.DEVEZ_VIBE_VERSION || "dev";
const sessions = new Map();
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

function catalogEntry(model) {
  const value = String(model.value || "");
  const resolved = String(model.resolvedModel || value);
  const efforts = model.supportsEffort && Array.isArray(model.supportedEffortLevels)
    ? model.supportedEffortLevels
    : [];
  const contextWindow = Number(model.contextWindow || model.contextWindowSize || 0);
  return {
    id: visibleModel(resolved),
    model: visibleModel(value),
    displayName: compactClaudeModelName(model),
    defaultReasoningEffort: efforts.includes("high") ? "high" : efforts.at(-1) || "",
    supportedReasoningEfforts: efforts.map((reasoningEffort) => ({ reasoningEffort })),
    isDefault: false,
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
    if (params.claudePath) options.pathToClaudeCodeExecutable = params.claudePath;
    const agentQuery = query({ prompt: input, options });
    const consumer = (async () => {
      try { for await (const _message of agentQuery) { /* initialization only */ } }
      catch { /* the caller receives the supportedModels error */ }
    })();
    try {
      const models = await agentQuery.supportedModels();
      return {
        data: models
          .filter((model) => model.value && model.value !== "default")
          .map(catalogEntry),
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

function makeOptions(params, sessionId, resume) {
  const options = {
    cwd: params.cwd || process.cwd(),
    includePartialMessages: true,
    permissionMode: "default",
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
  if (params.claudePath) options.pathToClaudeCodeExecutable = params.claudePath;
  if (resume) options.resume = resume;
  else options.sessionId = sessionId;
  options.canUseTool = (toolName, input, permission) =>
    requestToolPermission(toolName, input, permission);
  return options;
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

  // Devez Vibe runs in its existing full-access profile. Claude's callback is
  // still kept so AskUserQuestion and explicit user-authored ask rules reach UI.
  if (!permission.matchedAskRule) {
    return { behavior: "allow", updatedInput: input };
  }

  let method = "item/permissions/requestApproval";
  let params = {
    reason: permission.decisionReason || permission.description || permission.title,
    permissions: { tool: toolName, blockedPath: permission.blockedPath },
  };
  if (toolName === "Bash") {
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
    models: [],
    queue,
    query: null,
    turn: null,
    turnSequence: 1,
    itemSequence: 1,
    streamBlocks: new Map(),
    tools: new Map(),
    tasks: new Map(),
  };
  const agentQuery = query({
    prompt: queue,
    options: makeOptions(params, id, resumeId),
  });
  session.query = agentQuery;
  sessions.set(id, session);
  const consumer = consume(session).catch((error) => {
    notify("error", {
      threadId: id,
      provider: "Claude",
      error: { message: error instanceof Error ? error.message : String(error) },
      willRetry: false,
    });
    if (session.turn) finishTurn(session, error);
  });
  session.consumer = consumer;
  const initialization = await agentQuery.initializationResult();
  session.models = Array.isArray(initialization.models) ? initialization.models : [];
  session.effort = supportedEffort(modelCapabilities(session.models, params.model), params.effort);
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

function processStreamEvent(session, message) {
  if (!session.turn || message.parent_tool_use_id) return;
  const event = message.event || {};
  if (event.type === "message_start") session.streamBlocks.clear();
  if (event.type === "content_block_start") {
    const block = event.content_block || {};
    if (block.type !== "text" && block.type !== "thinking") return;
    const id = nextItemId(session, block.type);
    const item = block.type === "text"
      ? { id, type: "agentMessage", text: "", provider: "Claude" }
      : { id, type: "reasoning", summary: [] };
    session.streamBlocks.set(event.index, { id, type: block.type, text: "" });
    emitItem(session, "started", item);
    return;
  }
  if (event.type === "content_block_delta") {
    const current = session.streamBlocks.get(event.index);
    if (!current) return;
    const delta = event.delta?.text || event.delta?.thinking || "";
    if (!delta) return;
    current.text += delta;
    emitDelta(
      session,
      current.type === "text" ? "item/agentMessage/delta" : "item/reasoning/summaryTextDelta",
      current.id,
      delta,
    );
    return;
  }
  if (event.type === "content_block_stop") {
    const current = session.streamBlocks.get(event.index);
    if (!current) return;
    const item = current.type === "text"
      ? { id: current.id, type: "agentMessage", text: current.text, provider: "Claude" }
      : { id: current.id, type: "reasoning", summary: [current.text] };
    emitItem(session, "completed", item);
    session.streamBlocks.delete(event.index);
  }
}

function processAssistant(session, message) {
  if (!session.turn || message.parent_tool_use_id) return;
  const content = Array.isArray(message.message?.content) ? message.message.content : [];
  for (const block of content) {
    if (block.type === "tool_use") processToolUse(session, block);
  }
  if (!session.streamBlocks.size && !session.turn.sawStreamText) {
    for (const block of content) {
      if (block.type !== "text" && block.type !== "thinking") continue;
      const id = nextItemId(session, block.type);
      const item = block.type === "text"
        ? { id, type: "agentMessage", text: block.text || "", provider: "Claude" }
        : { id, type: "reasoning", summary: [block.thinking || ""] };
      emitItem(session, "started", item);
      emitItem(session, "completed", item);
    }
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
    session.tools.set(block.id, { name, input, suppressed: true });
    return;
  }
  const item = toolItem(session, block.id, name, input);
  session.tools.set(block.id, { name, input, item });
  emitItem(session, "started", item);
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

function updatePlanFromToolUse(session, name, toolUseId, input) {
  if (name === "TaskCreate") {
    session.tasks.set(`pending:${toolUseId}`, {
      id: `pending:${toolUseId}`,
      subject: input.subject || input.description || "작업",
      status: "pending",
    });
  } else if (name === "TaskUpdate") {
    const task = session.tasks.get(String(input.taskId));
    if (task) {
      if (input.subject) task.subject = input.subject;
      if (input.status) task.status = input.status;
    }
  }
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
    session.tasks.clear();
    for (const task of value.tasks || value) {
      session.tasks.set(String(task.id), {
        id: String(task.id),
        subject: task.subject || task.description || "작업",
        status: task.status || "pending",
      });
    }
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
  notify("turn/plan/updated", {
    threadId: session.id,
    turnId: session.turn?.id,
    plan: [...session.tasks.values()].map((task, index) => ({
      step: numberedTaskSubject(task.subject, index),
      status: planStatus(task.status),
    })),
  });
}

function numberedTaskSubject(subject, index) {
  const text = String(subject || "작업").trim();
  return /^\d+\.\s/.test(text) ? text : `${index + 1}. ${text}`;
}

function processUser(session, message) {
  const content = Array.isArray(message.message?.content) ? message.message.content : [];
  for (const block of content) {
    if (block.type !== "tool_result") continue;
    const pending = session.tools.get(block.tool_use_id);
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
      last: totals,
      modelContextWindow: totals.contextWindow || undefined,
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
}

function finishTurn(session, error, durationMs) {
  if (!session.turn) return;
  const turn = { id: session.turn.id, status: error ? "failed" : "completed" };
  if (error) turn.error = { message: error instanceof Error ? error.message : error.message || String(error) };
  if (durationMs != null) turn.durationMs = durationMs;
  notify("turn/completed", { threadId: session.id, turn });
  session.turn = null;
  session.streamBlocks.clear();
}

async function consume(session) {
  for await (const message of session.query) {
    if (message.type === "stream_event") {
      if (message.event?.type === "content_block_delta" && (message.event?.delta?.text || message.event?.delta?.thinking)) {
        if (session.turn) session.turn.sawStreamText = true;
      }
      processStreamEvent(session, message);
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
  const id = rawSession(params.sessionId);
  const session = sessions.get(id);
  if (!session) throw new Error(`Claude 세션을 찾을 수 없습니다: ${id}`);
  if (session.turn) throw new Error("Claude가 이전 작업을 아직 실행 중입니다.");
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
  const turnId = `claude-turn-${session.turnSequence++}-${randomUUID()}`;
  session.turn = { id: turnId, sawStreamText: false };
  notify("turn/started", { threadId: id, turn: { id: turnId } });
  session.queue.push({
    type: "user",
    message: { role: "user", content: await inputContent(params.input, params.handoffContext) },
    parent_tool_use_id: null,
    session_id: id,
    origin: { kind: "human" },
  });
  return { turn: { id: turnId } };
}

function contentBlocks(message) {
  const content = message?.content;
  if (typeof content === "string") return [{ type: "text", text: content }];
  return Array.isArray(content) ? content : [];
}

function historyTurns(messages) {
  const turns = [];
  let turn = null;
  const tools = new Map();
  const tasks = new Map();
  for (const message of messages) {
    const blocks = contentBlocks(message.message);
    const userText = message.type === "user"
      ? stripHandoff(blocks.filter((block) => block.type === "text").map((block) => block.text || "").join("\n"))
      : "";
    if (userText && !blocks.some((block) => block.type === "tool_result")) {
      turn = {
        id: `claude-turn-${message.uuid}`,
        status: "completed",
        items: [{ id: `claude-user-${message.uuid}`, type: "userMessage", content: [{ type: "text", text: userText }] }],
      };
      turns.push(turn);
    }
    if (!turn) continue;
    if (message.type === "assistant") {
      for (const block of blocks) {
        if (block.type === "text") turn.items.push({ id: `${message.uuid}-text`, type: "agentMessage", text: block.text || "", provider: "Claude" });
        else if (block.type === "thinking") turn.items.push({ id: `${message.uuid}-thinking`, type: "reasoning", summary: [block.thinking || ""] });
        else if (block.type === "tool_use") {
          const pending = { name: block.name, input: block.input || {}, item: toolItem({}, block.id, block.name, block.input || {}) };
          tools.set(block.id, pending);
          if (block.name === "TaskCreate") tasks.set(`pending:${block.id}`, { id: `pending:${block.id}`, subject: block.input?.subject || "작업", status: "pending" });
          else if (block.name === "TaskUpdate") {
            const task = tasks.get(String(block.input?.taskId));
            if (task) Object.assign(task, block.input?.subject ? { subject: block.input.subject } : {}, block.input?.status ? { status: block.input.status } : {});
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
          tasks.clear();
          for (const task of message.tool_use_result.tasks) tasks.set(String(task.id), { id: String(task.id), subject: task.subject || "작업", status: task.status || "pending" });
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
  return turns;
}

async function dispatch(method, params = {}) {
  if (method === "model/list") return loadModelCatalog(params);
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
    const id = rawSession(params.sessionId);
    const existing = sessions.get(id);
    if (existing) {
      return {
        id,
        thread: { id, turns: [] },
        initialTurnsPage: { data: [], nextCursor: null },
        cwd: existing.cwd,
        model: existing.model,
        reasoningEffort: existing.effort,
        account: await safeAccount(existing.query),
        usage: await safeUsage(existing.query),
      };
    }
    const info = await getSessionInfo(id, { dir: params.cwd });
    if (!info) throw new Error(`Claude 세션을 찾을 수 없습니다: ${id}`);
    const messages = await getSessionMessages(id, { dir: params.cwd, includeSystemMessages: true });
    const lastModel = [...messages].reverse().find((message) => message.type === "assistant")?.message?.model;
    const { session, account, usage } = await createSession({ ...params, cwd: info.cwd || params.cwd, model: params.model || lastModel }, id);
    return {
      id,
      thread: { id, turns: [] },
      initialTurnsPage: { data: [], nextCursor: null },
      cwd: session.cwd,
      model: session.model,
      reasoningEffort: session.effort,
      account,
      usage,
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
    const id = rawSession(params.sessionId);
    const messages = await getSessionMessages(id, { dir: params.cwd, includeSystemMessages: true });
    return { data: historyTurns(messages), nextCursor: null };
  }
  if (method === "session/prompt") return startPrompt(params);
  if (method === "session/interrupt") {
    const session = sessions.get(rawSession(params.sessionId));
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
    const source = rawSession(params.sessionId);
    const forked = await forkSession(source, { dir: params.cwd });
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
    const session = sessions.get(rawSession(params.sessionId));
    if (session) {
      session.queue.close();
      session.query.close();
      sessions.delete(session.id);
      if (params.delete) await deleteSession(session.id, { dir: session.cwd });
    }
    return {};
  }
  if (method === "account/usage") {
    const session = sessions.get(rawSession(params.sessionId)) || [...sessions.values()][0];
    if (!session) return { account: null, usage: null };
    return { account: await safeAccount(session.query), usage: await safeUsage(session.query) };
  }
  if (method === "shutdown") {
    for (const session of sessions.values()) {
      session.queue.close();
      session.query.close();
    }
    sessions.clear();
    return {};
  }
  throw new Error(`지원하지 않는 Claude 브리지 메서드: ${method}`);
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
