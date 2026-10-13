import { act, fireEvent, render, screen, within } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { buildTrace, finishProcess, messageSteps, recordText, recordTool, restoreProcessMetadata } from './model';
import WorkDetails from './WorkDetails';
import TrajectoryView from './TrajectoryView';
import { useGeneralSettingsStore } from '../../stores/useGeneralSettingsStore';
import { useTabStore } from '../../stores/useTabStore';

vi.mock('../../components/MarkdownRenderer', () => ({ default: ({ content }: { content: string }) => <p>{content}</p> }));
vi.mock('../../components/ToolDiffView', () => ({ default: () => null, shouldUseDiffView: () => false }));
const writeText = vi.fn(async () => {});
const output = '完整输出\n' + 'x'.repeat(9000) + '\nEND';
function sample() {
  const message: any = { id: 'a', role: 'assistant', content: '先检查。完成。', toolCalls: [{ id: 't1', name: 'pwsh', input: { command: 'Get-ChildItem\nGet-Date', description: '列出文件' }, result: output, status: 'done', startedAt: 2000, endedAt: 3000 }] };
  recordText(message, 'thinking', '需要检查目录。', 1000);
  recordText(message, 'text', '先检查。', 1500);
  recordTool(message, message.toolCalls[0], 2000);
  message.processSteps.at(-1).endedAt = 3000;
  recordText(message, 'text', message.content, 3100);
  finishProcess(message, 3500);
  return message;
}
beforeEach(() => {
  writeText.mockClear();
  Object.defineProperty(navigator, 'clipboard', { value: { writeText }, configurable: true });
  useGeneralSettingsStore.setState({ workDetails: 'standard', showCodingView: true });
});

describe('ordered process events', () => {
  it('keeps captured system prompts authoritative and numbers steps across assistant messages', () => {
    const snapshot = { id: 'request', number: 1, startedAt: 100, model: 'test', systemPrompt: 'Actual project instructions', systemMessages: [], tools: [] };
    const assistant = { role: 'assistant', content: 'one', requestSnapshots: [snapshot] };
    const trace = buildTrace([{ role: 'user', content: 'question' }, assistant, { role: 'assistant', content: 'two' }]);
    expect(trace[0]).toMatchObject({ role: 'system', title: 'Initial System Prompt', text: snapshot.systemPrompt });
    expect(trace.filter(r => r.role === 'assistant').map(r => r.step)).toEqual([1, 2]);
    const restored = restoreProcessMetadata(assistant, 0, [{ ...assistant, requestSnapshots: [{ ...snapshot, systemPrompt: 'stale' }] }]);
    expect(restored.requestSnapshots[0].systemPrompt).toBe(snapshot.systemPrompt);
  });
  it('merges unchanged system prompt rows while preserving each step request and prompt changes', () => {
    const snapshot = { id: 'r1', number: 1, startedAt: 100, model: 'test', systemPrompt: null, systemMessages: [{ role: 'system', content: [{ type: 'text', text: '实际系统指令' }] }], tools: [] };
    const snapshots = [snapshot, { ...snapshot, id: 'r2', number: 2 }, { ...snapshot, id: 'r3', number: 3, systemPrompt: 'Changed project instructions' }];
    const trace = buildTrace([{ role: 'user', content: 'test' }, { role: 'assistant', requestSnapshots: snapshots,
      processSteps: snapshots.map(request => ({ id: request.id, kind: 'text', text: request.id, requestId: request.id })) }]);
    expect(trace.filter(r => r.role === 'system').map(r => r.text)).toEqual(['实际系统指令', 'Changed project instructions\n\n实际系统指令']);
    expect(trace.filter(r => r.role === 'assistant').map(r => r.request?.id)).toEqual(['r1', 'r2', 'r3']);
  });
  it('keeps thinking, commentary, tool and final answer in stream order without repeated text', () => {
    const message = sample();
    expect(messageSteps(message).map(s => s.kind)).toEqual(['thinking', 'text', 'tool', 'text']);
    expect(messageSteps(message).filter(s => s.kind === 'text').map(s => s.text).join('')).toBe(message.content);
    const records = buildTrace([{ role: 'user', content: '查看目录' }, message]);
    expect(records.find(r => r.role === 'tool')).toMatchObject({ id: 'message:1/tool:t1', turn: 1, step: 3, startedAt: 2000, endedAt: 3000, result: output });
  });
  it('keeps parallel calls separate and preserves their IDs on input updates', () => {
    const message: any = {};
    recordTool(message, { id: 'one', startedAt: 100 }, 100);
    recordTool(message, { id: 'two', startedAt: 120 }, 120);
    recordTool(message, { id: 'one', startedAt: 100 }, 130);
    expect(message.processSteps).toHaveLength(2);
    expect(message.processSteps[0].endedAt).toBeUndefined();
  });
  it('resumes cached text without duplicating it and accepts thinking deltas after a provider resets its block', () => {
    const message: any = { processSteps: [{ id: 'text', kind: 'text', text: 'before' }] };
    recordText(message, 'text', 'before after', 100);
    expect(message.processSteps[0].text).toBe('before after');
    recordText(message, 'thinking', 'first', 110, 'first');
    recordTool(message, { id: 'tool' }, 120);
    recordText(message, 'thinking', 'a longer new block', 130, 'a longer new block');
    expect(message.processSteps.at(-1).text).toBe('a longer new block');
  });
  it('restores events when SQLite IDs change but refuses stale content', () => {
    const local = sample();
    const server = { ...local, id: 'sqlite-id', processSteps: undefined, toolCalls: local.toolCalls.map((t: any) => ({ ...t, id: 'sqlite-tool-id' })) };
    expect(restoreProcessMetadata(server, 0, [local]).toolCalls[0].id).toBe('t1');
    expect(restoreProcessMetadata(server, 0, [local]).processSteps).toEqual(local.processSteps);
    expect(restoreProcessMetadata({ ...server, content: 'edited' }, 0, [local]).processSteps).toBeUndefined();
  });
  it('does not invent history timings and removes duplicated tool commentary', () => {
    const message = { role: 'assistant', content: '检查。完成。', toolCalls: [{ id: 'a', name: 'pwsh', textBefore: '检查。', status: 'done' }] };
    expect(messageSteps(message).filter(s => s.kind === 'text').map(s => s.text)).toEqual(['检查。', '完成。']);
    expect(buildTrace([message]).find(r => r.role === 'tool')?.startedAt).toBeUndefined();
  });
});

describe('work details and inspect', () => {
  it('shows turn summaries below Step 0 only when folded, and opens details without folding steps', () => {
    const snapshot = { id: 'request', number: 1, startedAt: 100, model: 'test', systemPrompt: null, systemMessages: [{ role: 'system', content: 'Actual prompt' }], tools: [] };
    render(<TrajectoryView messages={[
      { role: 'user', content: 'First question' }, { role: 'assistant', content: '**First answer**', requestSnapshots: [snapshot] },
      { role: 'user', content: 'Second question' }, { role: 'assistant', content: 'Second answer' },
    ]} loading={false} focusId="message:1/answer" />);
    expect(screen.getAllByRole('tab').map(t => t.textContent)).toEqual(['Summary', 'Preview', 'Raw']);
    fireEvent.click(screen.getByRole('tab', { name: 'Raw' }));
    expect(within(screen.getByRole('tabpanel')).getByText('**First answer**')).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Turn 1' })).not.toBeInTheDocument();
    const list = screen.getByLabelText('过程列表');
    fireEvent.click(within(list).getByText('Second answer'));
    expect(within(list).getByText('**First answer**')).toBeInTheDocument();
    expect(within(screen.getByRole('tabpanel')).getByText('Second answer')).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Turns' }));
    expect(screen.getByRole('button', { name: 'Turn 1' })).toHaveAttribute('aria-expanded', 'false');
    expect(screen.getByRole('button', { name: 'Turn 2' })).toHaveAttribute('aria-expanded', 'false');
    const userRow = within(list).getByText('First question').closest('button')!;
    expect(userRow.compareDocumentPosition(screen.getByRole('button', { name: 'Turn 1' })) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(within(list).queryByText('**First answer**')).not.toBeInTheDocument();
    expect(within(list).getByText('First question')).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Turn 1' }));
    expect(screen.queryByRole('button', { name: 'Turn 1' })).not.toBeInTheDocument();
    expect(within(list).getByText('**First answer**')).toBeInTheDocument();
    fireEvent.click(within(list).getByText('Initial System Prompt'));
    expect(screen.getAllByRole('tab').map(t => t.textContent)).toEqual(['System Prompt', 'Tools']);
    expect(within(screen.getByRole('tabpanel')).getByText('Actual prompt')).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Turn 2' })).not.toBeInTheDocument();
  });
  it.each(['standard', 'detailed', 'verbose'] as const)('%s copies the full command and output independently', async level => {
    useGeneralSettingsStore.setState({ workDetails: level });
    render(<WorkDetails message={sample()} index={1} live={false} />);
    if (level !== 'verbose') fireEvent.click(screen.getByRole('button', { name: /列出文件/ }));
    const commandCopy = screen.getByRole('button', { name: '复制命令' });
    const outputCopy = screen.getByRole('button', { name: '复制输出' });
    expect(commandCopy).toHaveTextContent('');
    expect(outputCopy).toHaveTextContent('');
    await act(async () => { fireEvent.click(commandCopy); });
    expect(writeText).toHaveBeenLastCalledWith('Get-ChildItem\nGet-Date');
    await act(async () => { fireEvent.click(screen.getByRole('button', { name: '复制输出' })); });
    expect(writeText).toHaveBeenLastCalledWith(output);
    expect(screen.getAllByText('已复制')).toHaveLength(2);
    expect(commandCopy).toHaveAttribute('title', '已复制');
    expect(outputCopy).toHaveAttribute('title', '已复制');
    expect(screen.getAllByText('已复制').every(element => element.classList.contains('sr-only'))).toBe(true);
  });
  it.each([
    { status: 'done', result: { exit_code: 0 }, failed: false },
    { status: 'error', result: 'failed', failed: true },
    { status: 'done', result: { exit_code: 1 }, failed: true },
    { status: 'done', result: JSON.stringify({ exit_code: 2, stderr: 'failed' }), failed: true },
  ])('shows command outcome for $status with result $result', ({ status, result, failed }) => {
    const message = sample();
    message.toolCalls[0] = { ...message.toolCalls[0], status, result };
    const view = render(<WorkDetails message={message} index={1} live={false} />);
    expect(screen.getByRole('img', { name: failed ? '运行失败' : '运行成功' })).toHaveClass(failed ? 'bg-red-500' : 'bg-green-500');
    expect(screen.getByRole('button', { name: /列出文件/ }).classList.contains('text-red-500')).toBe(failed);
    expect(buildTrace([message]).find(r => r.role === 'tool')?.status).toBe(failed ? 'error' : 'done');
    view.unmount();
    message.toolCalls.push({ id: 't2', name: 'pwsh', status: 'done', result: { exit_code: 0 } });
    message.processSteps = undefined;
    render(<WorkDetails message={message} index={1} live={false} />);
    const group = screen.getByRole('button', { name: /Ran commands · 2/ });
    expect(within(group).getByRole('img', { name: failed ? '运行失败' : '运行成功' })).toBeInTheDocument();
    expect(group.classList.contains('text-red-500')).toBe(failed);
  });
  it('compact groups consecutive shell calls and keeps commentary visible', () => {
    useGeneralSettingsStore.setState({ workDetails: 'compact' });
    render(<WorkDetails index={0} live={false} message={{ role: 'assistant', content: '完成', toolCalls: [{ id: 'a', name: 'pwsh', textBefore: '检查目录' }, { id: 'b', name: 'Bash' }] }} />);
    expect(screen.getByText('检查目录')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /Ran commands · 2/ })).toHaveAttribute('aria-expanded', 'false');
    expect(screen.queryByText('Think')).not.toBeInTheDocument();
  });
  it('offers Inspect only under individual successful calls, including expanded groups', () => {
    const onInspect = vi.fn();
    const message = { role: 'assistant', content: '完成', toolCalls: [
      { id: 'a', name: 'pwsh', status: 'done' }, { id: 'b', name: 'Bash', status: 'done' },
      { id: 'c', name: 'Write', status: 'done', result: { success: false, error: 'Failed to write' } }, { id: 'd', name: 'Read', status: 'running' },
    ] };
    render(<WorkDetails index={0} live={true} message={message} onInspect={onInspect} />);
    expect(screen.queryByRole('button', { name: 'Inspect' })).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: /Ran commands · 2/ }));
    const buttons = screen.getAllByRole('button', { name: 'Inspect' });
    expect(buttons).toHaveLength(2);
    fireEvent.click(buttons[1]);
    expect(onInspect).toHaveBeenCalledWith('message:0/tool:b');
  });
  it.each(['object', 'json'] as const)('renders and copies stdout/stderr from %s results without execution metadata', async format => {
    const result = { exit_code: 1, shell: 'cmd', stdout: '名称    内存\r\nQQ      0.67\r\n', stderr: '错误信息\r\n  行 17\r\n', success: false };
    const message = sample();
    message.toolCalls[0].result = format === 'json' ? JSON.stringify(result) : result;
    const view = render(<WorkDetails message={message} index={1} live={false} />);
    fireEvent.click(screen.getByRole('button', { name: /列出文件/ }));
    expect(screen.getByLabelText('stdout')).toHaveTextContent('名称');
    expect(screen.getByLabelText('stdout').querySelector('pre')?.textContent).toBe('名称    内存\nQQ      0.67\n');
    expect(screen.getByLabelText('stderr').querySelector('pre')?.textContent).toBe('错误信息\n  行 17\n');
    expect(screen.queryByText(/exit_code|"success"|"shell"/)).not.toBeInTheDocument();
    await act(async () => { fireEvent.click(screen.getByRole('button', { name: '复制输出' })); });
    expect(writeText).toHaveBeenLastCalledWith('名称    内存\nQQ      0.67\n\n错误信息\n  行 17\n');
    view.unmount();
    render(<TrajectoryView messages={[message]} loading={false} focusId="message:0/tool:t1" />);
    expect(within(screen.getByRole('tabpanel')).getByLabelText('stderr')).toHaveTextContent('错误信息');
    fireEvent.click(screen.getByRole('tab', { name: 'Result' }));
    expect(within(screen.getByRole('tabpanel')).queryByText(/exit_code|"success"|"shell"/)).not.toBeInTheDocument();
  });
  it('Inspect selects the exact tool record and shows payload, result and timing', () => {
    const message = sample();
    const onInspect = vi.fn();
    const view = render(<WorkDetails message={message} index={1} live={false} onInspect={onInspect} />);
    const toolBlock = screen.getByRole('button', { name: /列出文件/ }).parentElement!;
    const inspect = within(toolBlock).getByRole('button', { name: 'Inspect' });
    expect(inspect.className).toContain('opacity-0');
    fireEvent.click(inspect);
    expect(onInspect).toHaveBeenCalledWith('message:1/tool:t1');
    view.unmount();
    render(<TrajectoryView messages={[{ role: 'user', content: '查看目录' }, message]} loading={false} focusId={onInspect.mock.calls[0][0]} />);
    const panel = screen.getByRole('tabpanel');
    expect(within(panel).getByText((_text, element) => element?.tagName === 'PRE' && element.textContent === output)).toBeInTheDocument();
    fireEvent.click(screen.getByRole('tab', { name: 'Timing' }));
    expect(within(panel).getByText('1000 ms')).toBeInTheDocument();
    fireEvent.click(screen.getByRole('tab', { name: 'Schema' }));
    expect(within(panel).getByText('未记录')).toBeInTheDocument();
    fireEvent.change(screen.getByRole('textbox', { name: 'Search trajectory' }), { target: { value: 'absent-result' } });
    expect(screen.getByText('没有匹配的过程')).toBeInTheDocument();
  });
  it('keeps tool details available when coding view is disabled, hiding only Inspect', () => {
    useGeneralSettingsStore.setState({ showCodingView: false });
    render(<WorkDetails message={sample()} index={0} live={false} onInspect={vi.fn()} />);
    expect(screen.getByRole('button', { name: /列出文件/ })).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Inspect' })).not.toBeInTheDocument();
  });
  it('replaces the active conversation instead of accumulating tabs', () => {
    useTabStore.setState({ openTabs: [], activeTabId: null });
    useTabStore.getState().openTab({ conversationId: 'one', title: 'One' });
    useTabStore.getState().openTab({ conversationId: 'two', title: 'Two' });
    expect(useTabStore.getState().openTabs.map(t => t.conversationId)).toEqual(['two']);
  });
});
