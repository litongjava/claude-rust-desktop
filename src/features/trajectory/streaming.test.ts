import { describe, it, expect, vi } from 'vitest';
import { createStreamCallbacks } from '../streaming/createStreamCallbacks';
vi.mock('../../api', () => ({ getConversation: vi.fn(), getContextSize: vi.fn(), getAttachmentUrl: vi.fn(), compactConversation: vi.fn() }));
vi.mock('../../api/conversations', () => ({ persistMessages: vi.fn() }));
vi.mock('../../pyodideRunner', () => ({ executeCode: vi.fn(), sendCodeResult: vi.fn() }));
vi.mock('../../hooks/useAnalytics', () => ({ trackTokensUsed: vi.fn() }));

function harness() {
  let messages: any[] = [{ role: 'user', content: 'list' }, { role: 'assistant', content: '' }];
  const ctx: any = {
    conversationId: 'test', streamRequestId: 1, isStreamSessionActive: () => true,
    setMessagesFor: (_id: string, update: any) => { messages = update(messages); },
    setMessages: (update: any) => { messages = update(messages); },
    removeStreaming: vi.fn(), clearStreamSession: vi.fn(), messagesBufferRef: { current: new Map() },
    activeRequestCountRef: { current: 1 }, viewingIdRef: { current: 'test' }, abortControllerRef: { current: null },
    setLoading: vi.fn(), setCompactStatus: vi.fn(), setContextInfo: vi.fn(), setTokenUsage: vi.fn(), setActiveTasks: vi.fn(),
    setAskUserDialog: vi.fn(), setToolPermissionDialog: vi.fn(), setPermissionApproval: vi.fn(), setPlanMode: vi.fn(), setConversationTitle: vi.fn(),
  };
  return { callbacks: createStreamCallbacks(ctx), message: () => messages[messages.length - 1] };
}
describe('stream process recording', () => {
  it.each(['onSystemEvents', 'onSystemEventsLight'] as const)('%s associates blocks with their actual request snapshot', callback => {
    const { callbacks: c, message } = harness();
    const first = { id: 'request-1', number: 1, systemPrompt: 'initial', tools: [] };
    c[callback]('request_snapshot', '', { snapshot: first, persisted: true });
    c.onDelta('First.', 'First.');
    c[callback]('request_snapshot', '', { snapshot: { ...first, id: 'request-2', number: 2 }, persisted: true });
    c.onDelta('Second.', 'First.Second.');
    expect(message().processSteps.map((s: any) => s.requestId)).toEqual(['request-1', 'request-2']);
    expect(message().requestSnapshots).toHaveLength(2);
    expect(message().requestSnapshots[0]).toMatchObject({ systemPrompt: 'initial', persisted: true });
  });
  it('records ordered tool boundaries and final parameters/results', () => {
    const { callbacks: c, message } = harness();
    c.onThinking('check', 'check');
    c.onDelta('Before.', 'Before.');
    c.onToolUse({ type: 'start', tool_use_id: 'call', tool_name: 'pwsh', tool_input: {} });
    c.onToolUse({ type: 'input', tool_use_id: 'call', tool_input: { command: 'Get-Date' } });
    c.onToolUse({ type: 'done', tool_use_id: 'call', tool_name: 'pwsh', content: 'today', is_error: false });
    c.onDelta('After.', 'Before.After.');
    c.onDoneSimple('Before.After.');
    expect(message().processSteps.map((s: any) => s.kind)).toEqual(['thinking', 'text', 'tool', 'text']);
    expect(message().toolCalls[0]).toMatchObject({ id: 'call', input: { command: 'Get-Date' }, result: 'today', status: 'done' });
    expect(message().processSteps.every((s: any) => s.endedAt != null)).toBe(true);
    expect(message().processSteps.filter((s: any) => s.kind === 'text').map((s: any) => s.text).join('')).toBe('Before.After.');
  });
  it('preserves visible errors and correctly marks a result received without its start', () => {
    const { callbacks: c, message } = harness();
    c.onDelta('Before.', 'Before.');
    c.onToolUse({ type: 'done', tool_use_id: 'late', tool_name: 'pwsh', tool_input: { command: 'fail' }, content: 'failed', is_error: true });
    c.onError('connection lost');
    expect(message().toolCalls[0]).toMatchObject({ status: 'error', input: { command: 'fail' } });
    expect(message().processSteps.at(-1).text).toContain('Error: connection lost');
    expect(message().content).toContain('Before.');
    expect(message().processEndedAt).toBeGreaterThan(0);
  });
});
