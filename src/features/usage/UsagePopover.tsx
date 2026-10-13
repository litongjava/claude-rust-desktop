import React, { useEffect, useId, useRef, useState } from 'react';
import { createPortal } from 'react-dom';

export default function UsagePopover({ label, trigger, children, align = 'center' }: {
  label: string; trigger: React.ReactNode; children: React.ReactNode; align?: 'left' | 'center' | 'right';
}) {
  const [open, setOpen] = useState(false);
  const [position, setPosition] = useState({ left: 0, top: 0, above: true });
  const button = useRef<HTMLButtonElement>(null);
  const panel = useRef<HTMLDivElement>(null);
  const id = useId();
  useEffect(() => {
    if (!open) return;
    const place = () => {
      const rect = button.current?.getBoundingClientRect();
      if (!rect) return;
      const width = Math.min(440, window.innerWidth - 24);
      const desired = align === 'left' ? rect.left : align === 'right' ? rect.right - width : rect.left + rect.width / 2 - width / 2;
      const above = rect.top > Math.min(panel.current?.offsetHeight || 340, window.innerHeight * .65) + 12;
      setPosition({ left: Math.max(12, Math.min(desired, window.innerWidth - width - 12)), top: above ? window.innerHeight - rect.top + 8 : rect.bottom + 8, above });
    };
    const dismiss = (event: MouseEvent) => { if (!panel.current?.contains(event.target as Node) && !button.current?.contains(event.target as Node)) setOpen(false); };
    const key = (event: KeyboardEvent) => { if (event.key === 'Escape') { event.stopPropagation(); setOpen(false); button.current?.focus(); } };
    place();
    document.addEventListener('mousedown', dismiss);
    document.addEventListener('keydown', key);
    window.addEventListener('resize', place);
    window.addEventListener('scroll', place, true);
    return () => { document.removeEventListener('mousedown', dismiss); document.removeEventListener('keydown', key); window.removeEventListener('resize', place); window.removeEventListener('scroll', place, true); };
  }, [open, align]);
  return <>
    <button ref={button} type="button" aria-label={label} aria-expanded={open} aria-controls={open ? id : undefined} aria-haspopup="dialog" title={label}
      onClick={() => setOpen(value => !value)} className={`inline-flex items-center justify-center gap-1.5 rounded-lg px-2 py-1 text-claude-textSecondary hover:bg-claude-hover hover:text-claude-text transition-colors ${open ? 'bg-claude-hover' : ''}`}>{trigger}</button>
    {open && createPortal(<div ref={panel} id={id} role="dialog" aria-label={label} style={{ position: 'fixed', left: position.left, ...(position.above ? { bottom: position.top } : { top: position.top }), width: 'min(440px, calc(100vw - 24px))', maxHeight: '70vh' }}
      className="z-[220] overflow-auto rounded-2xl border border-claude-border bg-claude-input p-5 text-[14px] text-claude-text shadow-xl font-sans">{children}</div>, document.body)}
  </>;
}
