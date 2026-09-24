/** @jest-environment jsdom */

import { act, waitFor } from '@testing-library/react';
import { jest } from '@jest/globals';
import { createAnalyticsSpy, createHookHarness, createStreamResult } from './test-utils';
import { setStoredModelSelection } from '../../stores/apiKeyStore';
import type { SubscriptionBridge } from '../../platform/types';
import type { AiConnectionProvider } from '../../platform/types';

const testProviders: AiConnectionProvider[] = ['codex-subscription', 'anthropic'];

let status = {
  provider: 'codex-subscription' as const,
  state: 'signed-in' as const,
  accountId: 'codex-test',
  generation: 9,
};
const bridge: SubscriptionBridge = {
  getStatus: jest.fn(async () => status),
  startLogin: jest.fn(),
  cancelLogin: jest.fn(),
  signOut: jest.fn(async () => {
    status = { ...status, state: 'signed-out', accountId: null, generation: status.generation + 1 } as typeof status;
  }),
  listModels: jest.fn(async () => []),
  startRequest: jest.fn(async () => {}),
  cancelRequest: jest.fn(),
};

jest.unstable_mockModule('@/platform', () => ({
  getPlatform: () => ({ capabilities: { hasSubscriptionAuth: true }, subscriptions: bridge }),
  eventBus: { emit: jest.fn(), on: jest.fn() },
  historyService: { createCheckpoint: jest.fn(), restoreTo: jest.fn() },
}));

const [{ useAiAgent }, store] = await Promise.all([
  import('../useAiAgent'),
  import('../../stores/subscriptionStore'),
]);

describe('useAiAgent subscription continuation', () => {
  beforeEach(() => {
    localStorage.clear();
    status = { provider: 'codex-subscription', state: 'signed-in', accountId: 'codex-test', generation: 9 };
    setStoredModelSelection({ provider: 'codex-subscription', modelId: 'gpt-5.4-codex' });
    jest.clearAllMocks();
  });

  it('captures encrypted reasoning for the next turn and clears it on provider switch/sign-out', async () => {
    const analytics = createAnalyticsSpy();
    const modelCalls: unknown[][] = [];
    const streamCalls: Array<Record<string, unknown>> = [];
    const streams = [
      createStreamResult([
        { type: 'reasoning-end', id: 'reason-1', providerMetadata: { openai: { itemId: 'reasoning-item-9', reasoningEncryptedContent: 'sealed-reasoning' } } } as never,
        { type: 'text-start', id: 'text-1' } as never,
        { type: 'text-delta', id: 'text-1', text: 'First answer.' } as never,
        { type: 'text-end', id: 'text-1' } as never,
        { type: 'finish', finishReason: 'stop' } as never,
      ]),
      createStreamResult([
        { type: 'text-start', id: 'text-2' } as never,
        { type: 'text-delta', id: 'text-2', text: 'Second answer.' } as never,
        { type: 'text-end', id: 'text-2' } as never,
        { type: 'finish', finishReason: 'stop' } as never,
      ]),
    ];
    const hook = createHookHarness(() => useAiAgent({
      testOverrides: {
        analytics: analytics as never,
        availableProviders: testProviders as never,
        createModel: ((...args: unknown[]) => { modelCalls.push(args); return {}; }) as never,
        buildTools: (() => ({})) as never,
        startAiStream: ((options: Record<string, unknown>) => {
          streamCalls.push(options);
          return Promise.resolve(streams.shift()!);
        }) as never,
      },
    }));

    await waitFor(() => expect(hook.current().availableProviders).toContain('codex-subscription'));
    await act(async () => hook.current().submitPrompt('First prompt'));
    await waitFor(() => expect(hook.current().isStreaming).toBe(false));
    const firstAssistant = hook.current().messages.find((message) => message.type === 'assistant');
    expect(firstAssistant).toMatchObject({
      continuation: {
        provider: 'codex-subscription', accountGeneration: 9,
        itemId: 'reasoning-item-9', encryptedContent: 'sealed-reasoning',
      },
    });

    await act(async () => hook.current().submitPrompt('Second prompt'));
    await waitFor(() => expect(hook.current().isStreaming).toBe(false));
    const requestMessages = streamCalls[1].messages as Array<{ role: string; content: unknown[] }>;
    expect(JSON.stringify(requestMessages)).toContain('sealed-reasoning');
    expect(JSON.stringify(requestMessages)).toContain('reasoning-item-9');
    expect(modelCalls[0]).toContain('native-managed');

    act(() => hook.current().setCurrentModel('claude-sonnet-4-5', 'unknown', 'anthropic'));
    expect(hook.current().messages.some((message) => message.type === 'assistant' && message.continuation)).toBe(false);
    await act(async () => store.signOutSubscription('codex-subscription'));
    hook.unmount();
  });
});
