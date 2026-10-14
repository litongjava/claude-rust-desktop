import { useEffect, useState, type FormEvent } from 'react';
import { getSearchSettings, updateSearchSettings, type SearchSettings as SearchSettingsData } from '../api';

const inputClass = 'w-full bg-transparent border border-claude-border rounded-lg px-3 py-2 text-[13px] text-claude-text outline-none focus:border-[#387ee0]';
const defaults: SearchSettingsData = { baseUrl: 'https://api.deepseek.com/anthropic/v1', model: 'deepseek-v4-flash', hasApiKey: false };

export default function SearchSettings() {
  const [settings, setSettings] = useState(defaults);
  const [apiKey, setApiKey] = useState('');
  const [clearApiKey, setClearApiKey] = useState(false);
  const [loaded, setLoaded] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [notice, setNotice] = useState('');

  const load = async () => {
    setBusy(true); setError('');
    try {
      const result = await getSearchSettings();
      setSettings({ baseUrl: result.baseUrl, model: result.model, hasApiKey: result.hasApiKey });
      setLoaded(true);
    } catch (err) { setError((err as Error).message); }
    finally { setBusy(false); }
  };
  useEffect(() => { void load(); }, []);

  const save = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    setBusy(true); setError(''); setNotice('');
    try {
      const result = await updateSearchSettings({
        baseUrl: settings.baseUrl.trim(), model: settings.model.trim(),
        ...(clearApiKey ? { clearApiKey: true } : apiKey.trim() ? { apiKey: apiKey.trim() } : {}),
      });
      setSettings({ baseUrl: result.baseUrl, model: result.model, hasApiKey: result.hasApiKey });
      setApiKey(''); setClearApiKey(false);
      setNotice('联网搜索设置已保存');
    } catch (err) { setError((err as Error).message); }
    finally { setBusy(false); }
  };

  return <section className="max-w-2xl space-y-5">
    <div>
      <h2 className="text-xl font-semibold text-claude-text">DeepSeek 联网搜索</h2>
      <p className="mt-2 text-sm text-claude-textSecondary">DeepSeek 对话模型使用此独立搜索服务；未配置会提示设置，不使用聊天中转密钥。搜索会单独调用所选模型，并可能产生额外费用。</p>
    </div>
    {error && <div role="alert" className="rounded-lg bg-red-500/10 text-red-500 p-3 text-sm">{error}</div>}
    {!loaded && !busy && <button type="button" onClick={() => void load()} className="text-sm text-claude-text underline">重新加载</button>}
    <form onSubmit={save} className="space-y-5 rounded-2xl border border-claude-border bg-claude-input p-6">
      <fieldset disabled={!loaded || busy} className="space-y-5 disabled:opacity-60">
        <label className="block text-xs text-claude-textSecondary">搜索 API 地址<input required type="url" aria-label="搜索 API 地址" value={settings.baseUrl} onChange={event => { setSettings({ ...settings, baseUrl: event.target.value }); setNotice(''); }} className={`${inputClass} mt-1 font-mono`} placeholder={defaults.baseUrl} /></label>
        <label className="block text-xs text-claude-textSecondary">搜索模型<input required aria-label="搜索模型" value={settings.model} onChange={event => { setSettings({ ...settings, model: event.target.value }); setNotice(''); }} className={`${inputClass} mt-1`} placeholder={defaults.model} /></label>
        <div>
          <label className="block text-xs text-claude-textSecondary">搜索 API 密钥<input type="password" aria-label="搜索 API 密钥" autoComplete="new-password" value={apiKey} disabled={clearApiKey} onChange={event => { setApiKey(event.target.value); setNotice(''); }} className={`${inputClass} mt-1`} placeholder={settings.hasApiKey ? '已配置，留空保留现有密钥' : '手动输入独立的 DeepSeek API 密钥'} /></label>
          <p className="mt-2 text-xs text-claude-textSecondary">{settings.hasApiKey ? '已配置密钥，保存后不会在此显示。' : '尚未配置搜索密钥。'}</p>
          {settings.hasApiKey && <label className="mt-3 flex items-center gap-2 text-xs text-claude-textSecondary"><input type="checkbox" checked={clearApiKey} onChange={event => { setClearApiKey(event.target.checked); setApiKey(''); setNotice(''); }} />保存时清除搜索密钥</label>}
        </div>
        <div className="flex items-center justify-end gap-3 border-t border-claude-border pt-4">
          <span role="status" className="flex-1 text-xs text-claude-textSecondary">{notice}</span>
          <button type="submit" className="rounded-lg bg-claude-text px-4 py-2 text-sm text-claude-bg">{busy ? '保存中…' : '保存联网搜索设置'}</button>
        </div>
      </fieldset>
    </form>
  </section>;
}
