import { act, fireEvent, render, screen } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import SettingsPage from './SettingsPage';
import { useI18n } from '../hooks/useI18n';
import { useUIStore } from '../stores/useUIStore';
import en from '../locales/en.json';
import zh from '../locales/zh.json';

vi.mock('../api', () => ({
  getUserUsage: vi.fn(async () => ({})),
  getSessions: vi.fn(async () => ({ sessions: [] })),
  getProviderModels: vi.fn(async () => []),
}));
vi.mock('./ProviderSettings', () => ({ default: () => null }));
vi.mock('./McpSettingsPage', () => ({ default: () => null }));
vi.mock('./TaskList', () => ({ default: () => null }));
vi.mock('./NewTaskModal', () => ({ default: () => null }));

function OtherLanguageConsumer() {
  const { t } = useI18n();
  return <p data-testid="other-consumer">{t('common.newChat')}</p>;
}

async function renderSettings() {
  let view!: ReturnType<typeof render>;
  await act(async () => {
    view = render(
      <MemoryRouter>
        <SettingsPage />
        <OtherLanguageConsumer />
      </MemoryRouter>,
    );
  });
  return view;
}

beforeEach(() => {
  localStorage.clear();
  localStorage.setItem('user_mode', 'selfhosted');
  useUIStore.getState().setLanguage('en');
});

describe('settings interface language', () => {
  it('displays the active application language instead of a separate local preference', async () => {
    localStorage.setItem('language', 'zh');
    await renderSettings();

    expect(screen.getByRole('button', { name: /^Interface Language/ })).toHaveTextContent('English (United States)');
    expect(screen.getByRole('heading', { name: en.settings.title })).toBeInTheDocument();
  });

  it('updates translations across consumers immediately, persists the choice, and supports switching back', async () => {
    const view = await renderSettings();
    fireEvent.click(screen.getByRole('button', { name: /^Interface Language/ }));
    fireEvent.click(screen.getByRole('button', { name: /简体中文（中国大陆）/ }));

    expect(screen.getByRole('heading', { name: zh.settings.title })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /^界面语言/ })).toHaveTextContent('简体中文（中国大陆）');
    expect(screen.getByTestId('other-consumer')).toHaveTextContent(zh.common.newChat);
    expect(localStorage.getItem('app_language')).toBe('zh');
    expect(document.documentElement.lang).toBe('zh-CN');

    view.unmount();
    await renderSettings();
    expect(screen.getByRole('button', { name: /^界面语言/ })).toHaveTextContent('简体中文（中国大陆）');

    fireEvent.click(screen.getByRole('button', { name: /^界面语言/ }));
    fireEvent.click(screen.getByRole('button', { name: /English \(United States\)/ }));
    expect(screen.getByRole('heading', { name: en.settings.title })).toBeInTheDocument();
    expect(screen.getByTestId('other-consumer')).toHaveTextContent(en.common.newChat);
    expect(localStorage.getItem('app_language')).toBe('en');
    expect(document.documentElement.lang).toBe('en-US');
  });
});
