import { useEffect, useState } from 'react';
import { Check, Copy } from 'lucide-react';
import { copyToClipboard } from '../../utils/clipboard';

export default function CopyButton({ text, label = '复制' }: { text: string; label?: string }) {
  const [state, setState] = useState<'idle' | 'done' | 'error'>('idle');
  useEffect(() => { if (state !== 'idle') { const id = setTimeout(() => setState('idle'), 1800); return () => clearTimeout(id); } }, [state]);
  return <button type="button" aria-label={label} title={label} className="inline-flex shrink-0 items-center gap-1 rounded px-2 py-1 text-xs text-claude-textSecondary hover:bg-claude-hover focus-visible:ring-2 focus-visible:ring-blue-400"
    onClick={async event => { event.stopPropagation(); setState(await copyToClipboard(text) ? 'done' : 'error'); }}>
    {state === 'done' ? <Check size={13} /> : <Copy size={13} />}<span aria-live="polite">{state === 'done' ? '已复制' : state === 'error' ? '复制失败' : label}</span>
  </button>;
}
