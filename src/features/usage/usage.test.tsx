import React from 'react';
import { describe, expect, it } from 'vitest';
import { fireEvent, render, screen, within } from '@testing-library/react';
import type { RequestSnapshot } from '../trajectory/model';
import { aggregateUsage, contextBreakdown, sessionStatistics } from './model';
import SessionUsageBar from './SessionUsageBar';

const request = (override: Partial<RequestSnapshot> = {}): RequestSnapshot => ({ id: 'request-1', number: 1, model: 'model', startedAt: 1000, tools: [], systemPrompt: '', systemMessages: [], ...override });
describe('session usage', () => {
  it('normalizes Anthropic cache-exclusive input and OpenAI cache-inclusive input', () => {
    const usage = aggregateUsage([{ requestSnapshots: [request({ apiFormat: 'anthropic', inputTokens: 100, cacheReadTokens: 300, cacheWriteTokens: 20, outputTokens: 50 }), request({ id: 'request-2', apiFormat: 'openai', inputTokens: 420, cacheReadTokens: 300, cacheWriteTokens: 20, outputTokens: 50 })] }]);
    expect(usage).toMatchObject({ uncachedInput: 200, cachedInput: 600, cacheWrite: 40, output: 100, total: 940, available: true });
    expect(usage.cacheHit).toBeCloseTo(600 / 840 * 100);
  });
  it('counts a request once after updates and keeps sessions independent', () => {
    expect(aggregateUsage([{ requestSnapshots: [request({ outputTokens: 1 }), request({ outputTokens: 12 })] }]).output).toBe(12);
    expect(aggregateUsage([])).toMatchObject({ total: 0, available: false, cacheHit: null });
  });
  it('uses weighted generation throughput and union of parallel tool times', () => {
    const stats = sessionStatistics([{ role: 'user' }, { role: 'assistant', requestSnapshots: [request({ outputTokens: 100, generationMs: 1000, totalDurationMs: 1500, ttftMs: 500 }), request({ id: 'second', outputTokens: 100, generationMs: 3000, totalDurationMs: 4000, ttftMs: 1000 })], toolCalls: [{ startedAt: 5000, endedAt: 7000 }, { startedAt: 6000, endedAt: 8000 }] }]);
    expect(stats).toMatchObject({ turns: 1, steps: 2, llmMs: 5500, toolMs: 3000, ttftMs: 750, tps: 50 });
  });
  it('does not fabricate missing measurements or cache hit rates', () => {
    expect(sessionStatistics([{ role: 'assistant', requestSnapshots: [request()], toolCalls: [{ name: 'legacy' }] }])).toMatchObject({ llmMs: null, toolMs: null, ttftMs: null, tps: null });
    expect(aggregateUsage([{ requestSnapshots: [request({ inputTokens: 100, outputTokens: 5 })] }]).cacheHit).toBeNull();
  });
});
describe('context window', () => {
  it('uses latest context rather than cumulative session input after compaction', () => {
    const context = contextBreakdown([{ requestSnapshots: [request({ inputTokens: 10000, outputTokens: 100 }), request({ id: 'compacted', inputTokens: 900, outputTokens: 100, systemPrompt: '<skills>use this skill</skills>prompt', tools: [{ name: 'mcp__search', description: 'Search', input_schema: {} }, { name: 'Read', description: 'Read file', input_schema: {} }] })] }], 2000);
    expect(context.used).toBe(1000);
    expect(context.percent).toBe(50);
    expect(context.free).toBe(1000);
    expect(context.rows.reduce((sum, row) => sum + row.tokens, 0)).toBe(1000);
    expect(context.rows.find(row => row.label === 'Skills')!.tokens).toBeGreaterThan(0);
    expect(context.tools.filter(t => t.mcp)).toHaveLength(1);
  });
  it('marks estimates and unknown capacity, with finite nonnegative breakdown', () => {
    const estimated = contextBreakdown([{ role: 'user', content: '你好 hello' }]);
    expect(estimated.estimated).toBe(true);
    expect(estimated.percent).toBeNull();
    expect(estimated.limit).toBeNull();
    const small = contextBreakdown([{ requestSnapshots: [request({ inputTokens: 1, systemPrompt: 'system'.repeat(500) })] }], 1);
    expect(small.rows.reduce((sum, row) => sum + row.tokens, 0)).toBe(1);
    expect(small.rows.every(row => row.tokens >= 0)).toBe(true);
  });
  it('uses recorded engine capacity and completion reserve when catalog omits them', () => {
    const context = contextBreakdown([{ requestSnapshots: [request({ inputTokens: 1000, outputTokens: 50, contextWindow: 80000, reservedForCompletion: 8192 })] }]);
    expect(context).toMatchObject({ used: 1050, limit: 80000, buffer: 8192, free: 70758 });
  });
  it('opens all three statistics popovers and expands detailed context', () => {
    render(<SessionUsageBar messages={[{ role: 'user', content: 'hello' }, { role: 'assistant', requestSnapshots: [request({ inputTokens: 100, outputTokens: 20 })] }]} contextLimit={1000} />);
    fireEvent.click(screen.getByRole('button', { name: 'Session statistics' }));
    expect(screen.getByText('Avg time to first token (TTFT)')).toBeInTheDocument();
    fireEvent.keyDown(document, { key: 'Escape' });
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Token usage' }));
    expect(within(screen.getByRole('dialog', { name: 'Token usage' })).getByText('120 tok')).toBeInTheDocument();
    fireEvent.mouseDown(document.body);
    fireEvent.click(screen.getByRole('button', { name: 'Context window' }));
    fireEvent.click(screen.getByRole('button', { name: 'See detailed breakdown' }));
    expect(screen.getByText('System prompt and instructions')).toBeInTheDocument();
  });
});
