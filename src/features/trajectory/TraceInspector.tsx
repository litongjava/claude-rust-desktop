import { useState } from 'react';
import { X } from 'lucide-react';
import MarkdownRenderer from '../../components/MarkdownRenderer';
import CopyButton from './CopyButton';
import { rawText, type TraceRecord } from './model';

export default function TraceInspector({ record: r, onClose }: { record: TraceRecord; onClose: () => void }) {
  const tabs = r.role === 'tool' ? ['Summary', 'Payload', 'Result', 'Schema', 'Timing']
    : r.role === 'system' ? ['System Prompt', 'Tools'] : ['Summary', 'Preview', 'Raw'];
  const [chosen, setChosen] = useState(tabs[0]);
  const tab = tabs.includes(chosen) ? chosen : tabs[0];
  const details = (label: string, value: unknown, copyLabel = '复制内容') => <section className="mb-4">
    <div className="mb-2 flex items-center justify-between text-claude-textSecondary"><span>{label}</span>{value != null && <CopyButton text={rawText(value)} label={copyLabel} />}</div>
    <pre className="overflow-auto whitespace-pre-wrap break-words font-mono text-xs leading-relaxed">{value == null ? '未记录' : rawText(value) || '(空)'}</pre>
  </section>;
  const timing = <dl className="mb-4 grid grid-cols-[auto_1fr] gap-x-4 gap-y-2 text-xs">
    <dt>Started</dt><dd>{r.startedAt != null ? new Date(r.startedAt).toLocaleString() : '未记录'}</dd>
    <dt>Duration</dt><dd>{r.startedAt != null && r.endedAt != null ? `${r.endedAt - r.startedAt} ms` : '未记录'}</dd>
    <dt>Source</dt><dd>{r.startedAt != null ? r.role === 'system' ? '引擎请求时间' : '本地接收事件时间' : '未记录'}</dd>
  </dl>;
  const preview = <div className="min-w-0 break-words"><MarkdownRenderer content={r.text || ''} isStreaming={r.status === 'running'} /></div>;
  return <aside className="flex min-h-0 max-h-[60%] w-full shrink-0 flex-col border-t border-claude-border lg:max-h-none lg:w-[42%] lg:border-l lg:border-t-0" aria-label="过程详情">
    <div className="flex items-center gap-2 border-b border-claude-border px-3 py-2 text-xs"><b>{r.role.toUpperCase()}</b><span className="text-claude-textSecondary">Turn {r.turn} · Step {r.step}</span><button type="button" aria-label="关闭详情" className="ml-auto" onClick={onClose}><X size={16} /></button></div>
    <div className="flex overflow-x-auto border-b border-claude-border" role="tablist">{tabs.map(t => <button type="button" key={t} role="tab" aria-selected={tab === t} onClick={() => setChosen(t)} className={`whitespace-nowrap px-3 py-2 text-xs ${tab === t ? 'border-b-2 border-blue-500 text-blue-500' : 'text-claude-textSecondary'}`}>{t}</button>)}</div>
    <div className="min-h-0 flex-1 overflow-auto p-3 text-sm" role="tabpanel">
      {tab === 'Summary' && <>
        <dl className="mb-4 grid grid-cols-[auto_1fr] gap-x-4 gap-y-2 text-xs"><dt>Status</dt><dd>{r.status}</dd><dt>Source</dt><dd>{r.request ? `Request #${r.request.number} · ${r.request.model}` : '未记录'}</dd></dl>
        {r.role === 'tool' ? <>{details('Payload', r.input)}{details('Result', r.result, '复制输出')}</> : <>{preview}<div className="my-4 border-t border-claude-border" />{timing}</>}
      </>}
      {tab === 'Preview' && preview}
      {tab === 'Raw' && details('Raw', r.text)}
      {tab === 'Payload' && details('Payload', r.input, '复制参数')}
      {tab === 'Result' && details('Result', r.result, '复制输出')}
      {tab === 'Schema' && details('Schema', r.schema, '复制 Schema')}
      {tab === 'Timing' && timing}
      {tab === 'System Prompt' && <>
        <p className="mb-3 text-xs text-claude-textSecondary">{r.request ? `Request #${r.request.number} · ${r.request.model} · ${r.request.persisted === false ? '数据库保存失败' : 'SQLite 请求记录'}` : '此历史会话未记录系统提示词，无法还原。'}</p>
        {details('System Prompt', r.request ? r.request.systemPrompt || '(本次未传入独立系统提示词)' : undefined)}
        {!!r.request?.systemMessages?.length && details('Additional system context', r.request.systemMessages)}
      </>}
      {tab === 'Tools' && details('Tools', r.request?.tools, '复制工具定义')}
    </div>
  </aside>;
}
