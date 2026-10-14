import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import ProviderSettings from '../components/ProviderSettings';
import { getProviders, updateProvider, type Provider } from '../api';

vi.mock('../api', () => ({
  getProviders: vi.fn(), createProvider: vi.fn(), updateProvider: vi.fn(),
  deleteProvider: vi.fn(), fetchProviderModels: vi.fn(),
}));

const provider: Provider = {
  id: 'gateway', name: 'Gateway', baseUrl: 'https://gateway.example/v1',
  apiKey: '', format: 'openai', category: 'custom', enabled: true, models: [],
};

describe('provider web search settings', () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.mocked(getProviders).mockResolvedValue([provider]);
    vi.mocked(updateProvider).mockImplementation(async (_id, updates) => ({ ...provider, ...updates }));
  });

  it('defaults existing providers to native auto and persists a gateway override', async () => {
    const user = userEvent.setup();
    render(<ProviderSettings />);
    const search = await screen.findByLabelText('联网搜索');
    expect(search).toHaveValue('auto');
    await user.selectOptions(search, 'deepseek_native');
    await user.click(screen.getAllByRole('button', { name: '保存' })[0]);
    await waitFor(() => expect(updateProvider).toHaveBeenCalledWith('gateway', expect.objectContaining({
      webSearchStrategy: 'deepseek_native', baseUrl: provider.baseUrl,
    })));
    expect(await screen.findByText('已保存到本地配置文件')).toBeInTheDocument();
    expect(search).toHaveValue('deepseek_native');
  });

  it('preserves an explicit public search choice when loading settings', async () => {
    vi.mocked(getProviders).mockResolvedValue([{ ...provider, webSearchStrategy: 'public' }]);
    render(<ProviderSettings />);
    expect(await screen.findByLabelText('联网搜索')).toHaveValue('public');
  });
});
