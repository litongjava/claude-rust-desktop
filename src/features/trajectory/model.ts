import { extractTextContent } from '../../utils/messageHelpers';

export interface ProcessStep {
  id: string;
  kind: 'text' | 'thinking' | 'tool';
  text?: string;
  toolId?: string;
  startedAt?: number;
  endedAt?: number;
  requestId?: string;
}
export interface RequestSnapshot {
  id: string;
  number: number;
  startedAt: number;
  model: string;
  systemPrompt: string | null;
  systemMessages: { role: string; content: unknown }[];
  tools: { name: string; description: string; input_schema: unknown }[];
  persisted?: boolean;
}
export interface TraceRecord extends ProcessStep {
  turn: number;
  step: number;
  role: string;
  title: string;
  status: string;
  input?: unknown;
  result?: unknown;
  schema?: unknown;
  request?: RequestSnapshot;
}
export const rawText = (value: unknown): string => value == null ? '' : typeof value === 'string' ? value : JSON.stringify(value, null, 2);
export function toolInput(tool: any): any {
  if (typeof tool.input !== 'string') return tool.input || {};
  try { return JSON.parse(tool.input); } catch { return { command: tool.input }; }
}
export function toolCommand(tool: any): string {
  const input = toolInput(tool);
  return rawText(input.command ?? input.cmd ?? input.script ?? input.code ?? tool.input);
}
export function toolOutput(tool: any): string { return rawText(tool.result ?? tool.output ?? tool.error); }
export function toolCategory(name: string): string {
  if (/bash|pwsh|powershell|shell|exec_command|terminal/i.test(name)) return 'Ran commands';
  if (/glob|list|find/i.test(name)) return 'Found files';
  if (/grep|search/i.test(name)) return 'Searched';
  if (/read/i.test(name)) return 'Read files';
  if (/edit|write|patch/i.test(name)) return 'Edited files';
  return name;
}
export function finishProcess(message: any, now = Date.now()) {
  const steps: ProcessStep[] = message.processSteps || [];
  for (const s of steps) if (s.endedAt == null) s.endedAt = now;
  message.processEndedAt = now;
}
/** Capture ordered blocks at stream boundaries, independent of their presentation. */
export function recordText(message: any, kind: 'text' | 'thinking', full: string, now = Date.now(), streamedDelta?: string) {
  const offsetKey = kind === 'text' ? '_traceTextLength' : '_traceThinkingLength';
  const previous = message[offsetKey] ?? (message.processSteps || []).filter((s: ProcessStep) => s.kind === kind).reduce((n: number, s: ProcessStep) => n + (s.text?.length || 0), 0);
  const delta = streamedDelta ?? full.slice(previous <= full.length ? previous : 0);
  message[offsetKey] = full.length;
  if (!delta) return;
  const steps: ProcessStep[] = message.processSteps ||= [];
  const last = steps[steps.length - 1];
  if (last?.kind === kind && last.endedAt == null) last.text = (last.text || '') + delta;
  else {
    if (last && last.kind !== 'tool' && last.endedAt == null) last.endedAt = now;
    steps.push({ id: crypto.randomUUID(), kind, text: delta, startedAt: now, requestId: message.currentRequestId });
  }
  message.processStartedAt ??= now;
}
export function recordTool(message: any, tool: any, now = Date.now()) {
  const steps: ProcessStep[] = message.processSteps ||= [];
  if (steps.some(s => s.toolId === tool.id)) return;
  const last = steps[steps.length - 1];
  if (last && last.kind !== 'tool' && last.endedAt == null) last.endedAt = now;
  steps.push({ id: `tool:${tool.id}`, kind: 'tool', toolId: tool.id, startedAt: tool.startedAt, requestId: message.currentRequestId });
  message.processStartedAt ??= now;
}
export function messageSteps(message: any): ProcessStep[] {
  if (message.processSteps?.length) return message.processSteps;
  const steps: ProcessStep[] = [];
  if (message.thinking) steps.push({ id: 'thinking', kind: 'thinking', text: message.thinking });
  const tools = message.toolCalls || [];
  let prefix = '';
  tools.forEach((tool: any, i: number) => {
    if (tool.textBefore) {
      steps.push({ id: `before:${i}`, kind: 'text', text: tool.textBefore });
      prefix += tool.textBefore;
    }
    steps.push({ id: `tool:${tool.id || i}`, kind: 'tool', toolId: tool.id || String(i), startedAt: tool.startedAt, endedAt: tool.endedAt });
  });
  const full = extractTextContent(message.content);
  const offset = message.toolTextEndOffset;
  const final = offset > 0 && offset <= full.length ? full.slice(offset) : prefix && full.startsWith(prefix) ? full.slice(prefix.length) : full;
  if (final) steps.push({ id: 'answer', kind: 'text', text: final });
  return steps;
}
export const recordId = (messageIndex: number, stepId: string) => `message:${messageIndex}/${stepId}`;
export function buildTrace(messages: any[], loading = false): TraceRecord[] {
  let turn = 0;
  let step = 0;
  const records = messages.flatMap<TraceRecord>((message, i) => {
    if (message.role === 'user') { turn++; step = 0; }
    const running = loading && i === messages.length - 1;
    if (message.role !== 'assistant') return [{
      id: recordId(i, 'message'), kind: 'text' as const, text: extractTextContent(message.content),
      role: message.role || 'context', turn, step: 0, title: message.role || 'context', status: 'done',
      startedAt: message.created_at && Number.isFinite(Date.parse(message.created_at)) ? Date.parse(message.created_at) : undefined,
    }];
    const snapshots: RequestSnapshot[] = message.requestSnapshots || [];
    const steps = messageSteps(message).map((s) => {
      const tool = s.kind === 'tool' ? (message.toolCalls || []).find((t: any, k: number) => (t.id || String(k)) === s.toolId) : null;
      const request = snapshots.find(r => r.id === s.requestId) ?? (snapshots.length === 1 ? snapshots[0] : undefined);
      return {
        ...s, id: recordId(i, s.id), turn, step: ++step, role: tool ? 'tool' : 'assistant', request,
        title: tool?.name || (s.kind === 'thinking' ? 'Think' : 'Assistant'),
        status: tool ? tool.status === 'running' && !running ? 'interrupted' : tool.status || 'done' : running && !s.endedAt ? 'running' : 'done',
        startedAt: tool?.startedAt ?? s.startedAt, endedAt: tool?.endedAt ?? s.endedAt,
        input: tool?.input, result: tool?.result ?? tool?.output, schema: tool?.schema ?? request?.tools.find(t => t.name === tool?.name)?.input_schema,
      };
    });
    const headers: TraceRecord[] = snapshots.map(request => ({
      id: `request:${request.id}`, kind: 'text', turn, step: 0, role: 'system',
      title: 'System Prompt', status: request.persisted === false ? '未保存' : 'recorded',
      text: request.systemPrompt ?? '', input: request.systemMessages, result: request.tools, request,
      startedAt: request.startedAt,
    }));
    if (!steps.some(s => s.requestId)) return [...headers, ...steps];
    const ordered: TraceRecord[] = [];
    const emitted = new Set<string>();
    for (const s of steps) {
      const header = headers.find(h => h.request?.id === s.requestId);
      if (header && !emitted.has(header.id)) { ordered.push(header); emitted.add(header.id); }
      ordered.push(s);
    }
    return [...ordered, ...headers.filter(h => !emitted.has(h.id))];
  });
  if (records.length) {
    const first = records.findIndex(r => r.role === 'system' && r.request);
    if (first >= 0) {
      const [initial] = records.splice(first, 1);
      records.unshift({ ...initial, turn: 0, title: 'Initial System Prompt' });
    } else {
      records.unshift({ id: 'system:unrecorded', kind: 'text', role: 'system', turn: 0, step: 0, title: 'Initial System Prompt', status: '未记录' });
    }
  }
  return records;
}

export function processMetadata(message: any) {
  return { processSteps: message.processSteps, processStartedAt: message.processStartedAt,
    processEndedAt: message.processEndedAt, toolTextEndOffset: message.toolTextEndOffset, requestSnapshots: message.requestSnapshots };
}
function canonical(value: any): string {
  if (Array.isArray(value)) return `[${value.map(canonical).join(',')}]`;
  if (value && typeof value === 'object') return `{${Object.keys(value).sort().map(key => `${JSON.stringify(key)}:${canonical(value[key])}`).join(',')}}`;
  return JSON.stringify(value);
}
/** Server history remains authoritative; only enrich matching messages with locally captured events. */
export function restoreProcessMetadata(message: any, index: number, cached: any[]) {
  const local = cached.find(m => m.id === message.id) || cached[index];
  if (!local || local.role !== message.role || local.content !== message.content) return message;
  const serverTools = message.toolCalls || [];
  const localTools = local.toolCalls || [];
  const compatible = !serverTools.length || serverTools.length === localTools.length && serverTools.every((t: any, i: number) => t.name === localTools[i].name && canonical(toolInput(t)) === canonical(toolInput(localTools[i])));
  if (!compatible) return message;
  return { ...message, ...processMetadata(local), requestSnapshots: message.requestSnapshots ?? local.requestSnapshots,
    toolCalls: localTools.length ? localTools.map((t: any, i: number) => ({ ...serverTools[i], ...t })) : serverTools };
}
