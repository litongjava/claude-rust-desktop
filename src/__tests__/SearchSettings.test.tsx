import { act, render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { MemoryRouter } from 'react-router-dom';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import SearchSettings from '../components/SearchSettings';
import SettingsPage from '../components/SettingsPage';
import { getProviders, getSearchSettings, updateSearchSettings, testSearchSettings, type SearchSettings as SearchSettingsData, type SearchSettingsTestResult } from '../api';

vi.mock('../api', () => ({
  getSearchSettings: vi.fn(), updateSearchSettings: vi.fn(), testSearchSettings: vi.fn(), getProviders: vi.fn(),
  getUserProfile: vi.fn().mockResolvedValue({}), getUserUsage: vi.fn().mockResolvedValue({}),
  getSessions: vi.fn().mockResolvedValue({ sessions: [] }), getProviderModels: vi.fn().mockResolvedValue([]),
}));
vi.mock('../components/ProviderSettings', () => ({ default: () => null }));
vi.mock('../components/McpSettingsPage', () => ({ default: () => null }));
vi.mock('../components/TaskList', () => ({ default: () => null }));
vi.mock('../components/NewTaskModal', () => ({ default: () => null }));
vi.mock('../components/GeneralPreferences', () => ({ default: () => null }));

const existing: SearchSettingsData = {
  baseUrl: 'https://search.example/anthropic/v1', model: 'deepseek-flash', hasApiKey: true,
};
const searchSuccess: SearchSettingsTestResult = {
  ok: true, model: 'deepseek-flash', resultCount: 1, elapsedMs: 1250,
  results: [{ title: 'DeepSeek documentation', url: 'https://api-docs.deepseek.com/', snippet: 'Official documentation' }],
};

describe('independent DeepSeek search settings', () => {
  beforeEach(() => {
    localStorage.clear();
    vi.clearAllMocks();
    vi.mocked(getSearchSettings).mockResolvedValue(existing);
    vi.mocked(testSearchSettings).mockResolvedValue(searchSuccess);
    vi.mocked(updateSearchSettings).mockImplementation(async patch => ({
      baseUrl: patch.baseUrl, model: patch.model, hasApiKey: !patch.clearApiKey,
    }));
  });

  it('never loads the saved secret into the password field and preserves it on a blank save', async () => {
    const user = userEvent.setup();
    render(<SearchSettings />);
    await waitFor(() => expect(screen.getByLabelText('搜索 API 地址')).toHaveValue(existing.baseUrl));
    expect(screen.getByLabelText('搜索 API 密钥')).toHaveValue('');
    expect(screen.getByLabelText('搜索 API 密钥')).toHaveAttribute('type', 'password');
    await user.clear(screen.getByLabelText('搜索模型'));
    await user.type(screen.getByLabelText('搜索模型'), 'deepseek-custom');
    await user.click(screen.getByRole('button', { name: '保存联网搜索设置' }));
    await waitFor(() => expect(updateSearchSettings).toHaveBeenCalledWith({ baseUrl: existing.baseUrl, model: 'deepseek-custom' }));
    expect(getProviders).not.toHaveBeenCalled();
    expect(localStorage.setItem).not.toHaveBeenCalled();
  });

  it('sends a manually entered key only on save and clears the field afterwards', async () => {
    const user = userEvent.setup();
    render(<SearchSettings />);
    await waitFor(() => expect(screen.getByLabelText('搜索 API 地址')).toHaveValue(existing.baseUrl));
    await user.type(screen.getByLabelText('搜索 API 密钥'), 'manual-search-secret');
    expect(updateSearchSettings).not.toHaveBeenCalled();
    await user.click(screen.getByRole('button', { name: '保存联网搜索设置' }));
    await waitFor(() => expect(updateSearchSettings).toHaveBeenCalledWith({
      baseUrl: existing.baseUrl, model: existing.model, apiKey: 'manual-search-secret',
    }));
    await waitFor(() => expect(screen.getByLabelText('搜索 API 密钥')).toHaveValue(''));
    expect(localStorage.setItem).not.toHaveBeenCalled();
  });

  it('clears the saved key only when explicitly selected and saved', async () => {
    const user = userEvent.setup();
    render(<SearchSettings />);
    await user.click(await screen.findByLabelText('保存时清除搜索密钥'));
    expect(updateSearchSettings).not.toHaveBeenCalled();
    await user.click(screen.getByRole('button', { name: '保存联网搜索设置' }));
    await waitFor(() => expect(updateSearchSettings).toHaveBeenCalledWith({
      baseUrl: existing.baseUrl, model: existing.model, clearApiKey: true,
    }));
    expect(await screen.findByText('尚未配置搜索密钥。')).toBeInTheDocument();
  });

  it('opens the independent panel from system settings navigation', async () => {
    localStorage.setItem('user_mode', 'selfhosted');
    const user = userEvent.setup();
    render(<MemoryRouter future={{ v7_startTransition: true, v7_relativeSplatPath: true }}><SettingsPage /></MemoryRouter>);
    await user.click(screen.getByRole('button', { name: '联网搜索' }));
    expect(await screen.findByRole('heading', { name: 'DeepSeek 联网搜索' })).toBeInTheDocument();
    await waitFor(() => expect(getSearchSettings).toHaveBeenCalledTimes(1));
  });

  it('tests unsaved address, model and manual key without saving or caching credentials', async () => {
    let finish!: (result: SearchSettingsTestResult) => void;
    vi.mocked(testSearchSettings).mockReturnValue(new Promise(resolve => { finish = resolve; }));
    const user = userEvent.setup();
    render(<SearchSettings />);
    await waitFor(() => expect(screen.getByLabelText('搜索 API 地址')).toHaveValue(existing.baseUrl));
    expect(screen.getByLabelText('搜索模型')).toHaveAttribute('placeholder', 'deepseek-flash');
    await user.clear(screen.getByLabelText('搜索 API 地址'));
    await user.type(screen.getByLabelText('搜索 API 地址'), 'https://test.example/v1');
    await user.clear(screen.getByLabelText('搜索模型'));
    await user.type(screen.getByLabelText('搜索模型'), 'deepseek-test');
    await user.type(screen.getByLabelText('搜索 API 密钥'), 'unsaved-test-key');
    await user.click(screen.getByRole('button', { name: '测试搜索' }));
    expect(testSearchSettings).toHaveBeenCalledWith({ baseUrl: 'https://test.example/v1', model: 'deepseek-test', apiKey: 'unsaved-test-key' });
    expect(screen.getByRole('button', { name: '测试中…' })).toBeDisabled();
    expect(screen.getByRole('button', { name: '保存联网搜索设置' })).toBeDisabled();
    await act(async () => { finish({ ...searchSuccess, model: 'deepseek-test' }); });
    expect(await screen.findByText('搜索测试成功：1 条结果 · 1.3 秒 · deepseek-test')).toBeInTheDocument();
    expect(screen.getByRole('link', { name: 'DeepSeek documentation' })).toHaveAttribute('href', 'https://api-docs.deepseek.com/');
    expect(updateSearchSettings).not.toHaveBeenCalled();
    expect(localStorage.setItem).not.toHaveBeenCalled();
    expect(screen.getByLabelText('搜索 API 密钥')).toHaveValue('unsaved-test-key');
    await user.type(screen.getByLabelText('搜索模型'), '-changed');
    expect(screen.queryByText(/搜索测试成功/)).not.toBeInTheDocument();
  });

  it('tests with the saved key when the password input is blank', async () => {
    const user = userEvent.setup();
    render(<SearchSettings />);
    await waitFor(() => expect(screen.getByLabelText('搜索 API 地址')).toHaveValue(existing.baseUrl));
    await user.click(screen.getByRole('button', { name: '测试搜索' }));
    await waitFor(() => expect(testSearchSettings).toHaveBeenCalledWith({ baseUrl: existing.baseUrl, model: existing.model }));
    expect(updateSearchSettings).not.toHaveBeenCalled();
    expect(screen.getByLabelText('搜索 API 密钥')).toHaveValue('');
  });

  it('tests explicit key removal and displays a failed search without saving', async () => {
    vi.mocked(testSearchSettings).mockRejectedValue(new Error('请配置搜索 API 密钥'));
    const user = userEvent.setup();
    render(<SearchSettings />);
    await user.click(await screen.findByLabelText('保存时清除搜索密钥'));
    await user.click(screen.getByRole('button', { name: '测试搜索' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('请配置搜索 API 密钥');
    expect(testSearchSettings).toHaveBeenCalledWith({ baseUrl: existing.baseUrl, model: existing.model, clearApiKey: true });
    expect(updateSearchSettings).not.toHaveBeenCalled();
    expect(screen.getByRole('button', { name: '测试搜索' })).toBeEnabled();
  });
});
