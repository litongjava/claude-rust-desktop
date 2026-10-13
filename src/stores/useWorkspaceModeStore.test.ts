import { beforeEach, describe, expect, it } from 'vitest';
import { conversationWorkspaceMode, useWorkspaceModeStore } from './useWorkspaceModeStore';
import { createLocalConversation, getLocalConversations } from '../services/localStorageService';

beforeEach(() => {
  localStorage.clear();
  useWorkspaceModeStore.setState({ mode: 'chat', recentsCollapsed: { chat: false, cowork: false, code: false }, lastCodeProjectId: null });
});

describe('workspace histories', () => {
  it('keeps equal-titled local conversations separate and assigns legacy history only to Chat', () => {
    for (const mode of ['chat', 'cowork', 'code'] as const) {
      createLocalConversation('Same title', undefined, { workspace_mode: mode });
    }
    createLocalConversation('Legacy');
    const conversations = getLocalConversations();
    expect(conversations.filter(c => conversationWorkspaceMode(c) === 'chat')).toHaveLength(2);
    expect(conversations.filter(c => conversationWorkspaceMode(c) === 'cowork')).toHaveLength(1);
    expect(conversations.filter(c => conversationWorkspaceMode(c) === 'code')).toHaveLength(1);
    expect(conversationWorkspaceMode({})).toBe('chat');
  });

  it('persists active mode and separate collapsed states across hydration', async () => {
    useWorkspaceModeStore.getState().toggleRecents('cowork');
    useWorkspaceModeStore.getState().setMode('code');
    await useWorkspaceModeStore.persist.rehydrate();
    expect(useWorkspaceModeStore.getState().mode).toBe('code');
    expect(useWorkspaceModeStore.getState().recentsCollapsed).toEqual({ chat: false, cowork: true, code: false });
  });

  it('persists the last Code project across hydration and mode changes', async () => {
    useWorkspaceModeStore.getState().setMode('code');
    useWorkspaceModeStore.getState().setLastCodeProjectId('project-1');
    useWorkspaceModeStore.getState().setMode('chat');
    await useWorkspaceModeStore.persist.rehydrate();
    expect(useWorkspaceModeStore.getState().lastCodeProjectId).toBe('project-1');
    expect(JSON.parse(localStorage.getItem('workspace-mode')!).state.lastCodeProjectId).toBe('project-1');
  });
});
