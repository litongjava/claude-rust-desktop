import { act, fireEvent, render, screen } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import SettingsPage from './SettingsPage';
import { useI18n } from '../hooks/useI18n';
import { useUIStore } from '../stores/useUIStore';
import en from '../locales/en.json';
import zh from '../locales/zh.json';
import { normalizeGeneralSettings, useGeneralSettingsStore } from '../stores/useGeneralSettingsStore';
import { useChatStore } from '../stores/useChatStore';

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
  useGeneralSettingsStore.setState({ ...normalizeGeneralSettings({}), saveError: '' });
});

describe('general preferences', () => {
  it('places Account and About before General and separates their content', async () => {
    await renderSettings();
    const account = screen.getByRole('button', { name: en.settings.account });
    const about = screen.getByRole('button', { name: en.settings.about });
    const general = screen.getByRole('button', { name: en.settings.general });
    expect(account.compareDocumentPosition(about) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(about.compareDocumentPosition(general) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(screen.queryByRole('heading', { name: en.settings.personalInfo })).not.toBeInTheDocument();
    const fontSize = screen.getByText(en.general.fontSize);
    const chatFont = screen.getByRole('heading', { name: en.settings.chatFont });
    expect(fontSize.compareDocumentPosition(chatFont) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(chatFont.compareDocumentPosition(screen.getByText(en.general.workDetails)) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    fireEvent.click(account);
    expect(screen.getByRole('heading', { name: en.settings.personalInfo })).toBeInTheDocument();
    expect(screen.getByRole('heading', { name: en.settings.userMode })).toBeInTheDocument();
    expect(screen.queryByRole('spinbutton', { name: 'Font size' })).not.toBeInTheDocument();
    fireEvent.click(about);
    expect(screen.getByText('v3.1.0')).toBeInTheDocument();
    expect(screen.queryByRole('heading', { name: en.settings.personalInfo })).not.toBeInTheDocument();
  });

  it('persists all four work detail choices and Queue / Steer across remounts', async () => {
    const view = await renderSettings();
    const details = screen.getByRole('combobox', { name: 'Work details' });
    expect(Array.from(details.querySelectorAll('option')).map(option => option.textContent)).toEqual(['Compact', 'Standard', 'Detailed', 'Verbose']);
    fireEvent.change(details, { target: { value: 'verbose' } });
    fireEvent.change(screen.getByRole('combobox', { name: 'Send behavior while busy' }), { target: { value: 'steer' } });
    expect(JSON.parse(localStorage.getItem('general_settings')!)).toMatchObject({ workDetails: 'verbose', busySendBehavior: 'steer' });
    view.unmount();
    await renderSettings();
    expect(screen.getByRole('combobox', { name: 'Work details' })).toHaveValue('verbose');
    expect(screen.getByRole('combobox', { name: 'Send behavior while busy' })).toHaveValue('steer');
  });

  it('changes the default permission without modifying an active session', async () => {
    useChatStore.getState().setPermissionMode('ask_permissions');
    await renderSettings();
    fireEvent.click(screen.getByRole('button', { name: 'Accept edits' }));
    fireEvent.click(screen.getByRole('button', { name: /Plan mode/ }));
    expect(useGeneralSettingsStore.getState().defaultPermissionMode).toBe('plan_mode');
    expect(useChatStore.getState().permissionMode).toBe('ask_permissions');
  });

  it('applies appearance and conversation size immediately and clamps invalid sizes', async () => {
    await renderSettings();
    fireEvent.click(screen.getByRole('button', { name: 'Dark' }));
    expect(document.documentElement).toHaveClass('dark');
    const font = screen.getByRole('spinbutton', { name: 'Font size' });
    fireEvent.change(font, { target: { value: '18' } });
    expect(document.documentElement.style.getPropertyValue('--conversation-font-size')).toBe('18px');
    fireEvent.change(font, { target: { value: '100' } });
    fireEvent.blur(font);
    expect(font).toHaveValue(32);
    fireEvent.click(screen.getByRole('switch', { name: 'Show coding view' }));
    expect(useGeneralSettingsStore.getState().showCodingView).toBe(false);
  });

  it('edits input shortcuts without allowing send and newline to conflict', async () => {
    await renderSettings();
    fireEvent.click(screen.getByRole('button', { name: 'Edit shortcuts' }));
    const controls = screen.getByRole('dialog', { name: 'Keyboard shortcuts' }).querySelectorAll('select');
    fireEvent.change(controls[0], { target: { value: 'ctrl_enter' } });
    fireEvent.change(controls[1], { target: { value: 'enter' } });
    expect(localStorage.getItem('sendKey')).toBe('ctrl_enter');
    expect(localStorage.getItem('newlineKey')).toBe('enter');
    fireEvent.change(controls[0], { target: { value: 'enter' } });
    expect(localStorage.getItem('newlineKey')).toBe('shift_enter');
  });
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
