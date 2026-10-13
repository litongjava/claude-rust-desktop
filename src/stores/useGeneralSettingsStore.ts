import { create } from 'zustand';
import { invoke } from '@tauri-apps/api/core';
import { useUIStore } from './useUIStore';

export type WorkDetails = 'compact' | 'standard' | 'detailed' | 'verbose';
export type PermissionMode = 'ask_permissions' | 'accept_edits' | 'plan_mode' | 'bypass_permissions';
export interface GeneralSettings {
  defaultPermissionMode: PermissionMode;
  theme: 'light' | 'dark' | 'system';
  fontSize: number;
  workDetails: WorkDetails;
  showCodingView: boolean;
  chatLinks: 'sidebar' | 'browser';
  busySendBehavior: 'queue' | 'steer';
  sendKey: string;
  newlineKey: string;
}

const permissions = ['ask_permissions', 'accept_edits', 'plan_mode', 'bypass_permissions'];
const sendKeys = ['enter', 'ctrl_enter', 'cmd_enter', 'alt_enter'];
const newlineKeys = [...sendKeys, 'shift_enter'];
const defaults: GeneralSettings = {
  defaultPermissionMode: 'accept_edits', theme: 'light', fontSize: 14,
  workDetails: 'detailed', showCodingView: true, chatLinks: 'sidebar',
  busySendBehavior: 'queue', sendKey: 'enter', newlineKey: 'shift_enter',
};

export function normalizeGeneralSettings(value: unknown): GeneralSettings {
  const v = value && typeof value === 'object' ? value as Partial<GeneralSettings> : {};
  return {
    defaultPermissionMode: permissions.includes(v.defaultPermissionMode || '') ? v.defaultPermissionMode! : defaults.defaultPermissionMode,
    theme: ['light', 'dark', 'system'].includes(v.theme || '') ? v.theme! : defaults.theme,
    fontSize: typeof v.fontSize === 'number' && Number.isFinite(v.fontSize) ? Math.min(32, Math.max(10, Math.round(v.fontSize))) : defaults.fontSize,
    workDetails: ['compact', 'standard', 'detailed', 'verbose'].includes(v.workDetails || '') ? v.workDetails! : defaults.workDetails,
    showCodingView: typeof v.showCodingView === 'boolean' ? v.showCodingView : defaults.showCodingView,
    chatLinks: v.chatLinks === 'browser' ? 'browser' : 'sidebar',
    busySendBehavior: v.busySendBehavior === 'steer' ? 'steer' : 'queue',
    sendKey: sendKeys.includes(v.sendKey || '') ? v.sendKey! : defaults.sendKey,
    newlineKey: newlineKeys.includes(v.newlineKey || '') && v.newlineKey !== v.sendKey ? v.newlineKey! : 'shift_enter',
  };
}

function loadLocal(): GeneralSettings {
  try {
    const saved = localStorage.getItem('general_settings');
    if (saved) return normalizeGeneralSettings(JSON.parse(saved));
    return normalizeGeneralSettings({
      ...defaults, defaultPermissionMode: localStorage.getItem('permission_mode'),
      theme: localStorage.getItem('theme') === 'auto' ? 'system' : localStorage.getItem('theme') || 'light',
      sendKey: localStorage.getItem('sendKey') || 'enter',
      newlineKey: localStorage.getItem('newlineKey') || 'shift_enter',
    });
  } catch { return { ...defaults }; }
}

export function applyGeneralSettings(settings: GeneralSettings) {
  const root = document.documentElement;
  const dark = settings.theme === 'dark' || (settings.theme === 'system' && window.matchMedia('(prefers-color-scheme: dark)').matches);
  root.classList.toggle('dark', dark);
  root.setAttribute('data-theme', dark ? 'dark' : 'light');
  root.style.setProperty('--conversation-font-size', `${settings.fontSize}px`);
  root.setAttribute('data-chat-font', localStorage.getItem('chat_font') || 'default');
  localStorage.setItem('theme', settings.theme);
  localStorage.setItem('sendKey', settings.sendKey);
  localStorage.setItem('newlineKey', settings.newlineKey);
  localStorage.setItem('general_settings', JSON.stringify(settings));
}

let fileWrites = Promise.resolve();
let loadingFile = false;
function saveFile(settings: GeneralSettings) {
  if (loadingFile || !(window as any).__TAURI_INTERNALS__) return;
  const snapshot = { ...normalizeGeneralSettings(settings), language: useUIStore.getState().language, chatFont: localStorage.getItem('chat_font') || 'default' };
  fileWrites = fileWrites.then(async () => {
    await invoke('write_general_settings', { settings: snapshot });
    useGeneralSettingsStore.setState({ saveError: '' });
  }).catch(error => { useGeneralSettingsStore.setState({ saveError: String(error) }); });
}

export const useGeneralSettingsStore = create<GeneralSettings & {
  saveError: string;
  update: (patch: Partial<GeneralSettings>) => void;
}>((set, get) => ({
  ...loadLocal(), saveError: '',
  update: patch => {
    const settings = normalizeGeneralSettings({ ...get(), ...patch });
    applyGeneralSettings(settings);
    set(settings);
    saveFile(settings);
  },
}));

export async function initializeGeneralSettings() {
  loadingFile = true;
  if ((window as any).__TAURI_INTERNALS__) {
    try {
      const saved = await invoke<GeneralSettings & { language?: string; chatFont?: string } | null>('read_general_settings');
      if (saved) {
        useGeneralSettingsStore.setState(normalizeGeneralSettings(saved));
        if (saved.language) useUIStore.getState().setLanguage(saved.language);
        if (saved.chatFont && ['default', 'sans', 'system', 'dyslexic'].includes(saved.chatFont)) localStorage.setItem('chat_font', saved.chatFont);
      }
    } catch (error) { useGeneralSettingsStore.setState({ saveError: String(error) }); }
  }
  applyGeneralSettings(useGeneralSettingsStore.getState());
  loadingFile = false;
}

export async function openGeneralConfiguration() {
  if (!(window as any).__TAURI_INTERNALS__) throw new Error('Configuration files are available in the desktop app.');
  saveFile(useGeneralSettingsStore.getState());
  await fileWrites;
  if (useGeneralSettingsStore.getState().saveError) throw new Error(useGeneralSettingsStore.getState().saveError);
  await invoke('open_general_settings');
  window.addEventListener('focus', () => { void initializeGeneralSettings(); }, { once: true });
}

useUIStore.subscribe((state, previous) => {
  if (state.language !== previous.language) saveFile(useGeneralSettingsStore.getState());
});

if (typeof window !== 'undefined') {
  window.matchMedia('(prefers-color-scheme: dark)').addEventListener('change', () => {
    if (useGeneralSettingsStore.getState().theme === 'system') applyGeneralSettings(useGeneralSettingsStore.getState());
  });
}
