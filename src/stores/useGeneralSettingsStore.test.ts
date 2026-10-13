import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { invoke } from '@tauri-apps/api/core';
import { initializeGeneralSettings, normalizeGeneralSettings, openGeneralConfiguration, useGeneralSettingsStore } from './useGeneralSettingsStore';
import { useUIStore } from './useUIStore';

beforeEach(() => {
  (window as any).__TAURI_INTERNALS__ = {};
  localStorage.clear();
  useGeneralSettingsStore.setState({ ...normalizeGeneralSettings({}), saveError: '' });
  vi.mocked(invoke).mockReset();
});
afterEach(() => { delete (window as any).__TAURI_INTERNALS__; });

it('loads a desktop configuration, validates it, and applies language and appearance', async () => {
  vi.mocked(invoke).mockResolvedValue({ defaultPermissionMode: 'plan_mode', theme: 'dark', fontSize: 99, language: 'zh', chatFont: 'sans', workDetails: 'verbose', busySendBehavior: 'steer' });
  await initializeGeneralSettings();
  expect(useGeneralSettingsStore.getState()).toMatchObject({ defaultPermissionMode: 'plan_mode', fontSize: 32, workDetails: 'verbose', busySendBehavior: 'steer' });
  expect(document.documentElement).toHaveClass('dark');
  expect(document.documentElement.getAttribute('data-chat-font')).toBe('sans');
  expect(useUIStore.getState().language).toBe('zh');
  expect(invoke).toHaveBeenCalledTimes(1);
});

it('finishes saving before opening the desktop configuration and excludes internal state', async () => {
  vi.mocked(invoke).mockResolvedValue(null);
  useGeneralSettingsStore.getState().update({ fontSize: 18 });
  await openGeneralConfiguration();
  const calls = vi.mocked(invoke).mock.calls;
  expect(calls[calls.length - 1][0]).toBe('open_general_settings');
  const snapshot = (calls[0][1] as { settings: Record<string, unknown> }).settings;
  expect(snapshot.fontSize).toBe(18);
  expect(snapshot).not.toHaveProperty('update');
  expect(snapshot).not.toHaveProperty('saveError');
});

it('keeps local settings usable and reports an unreadable configuration', async () => {
  vi.mocked(invoke).mockRejectedValue(new Error('Invalid JSON'));
  await initializeGeneralSettings();
  expect(useGeneralSettingsStore.getState().saveError).toContain('Invalid JSON');
  expect(useGeneralSettingsStore.getState().fontSize).toBe(14);
  expect(invoke).toHaveBeenCalledTimes(1);
});
