/** @jest-environment jsdom */

import { act, render, screen, waitFor } from '@testing-library/react';
import { jest } from '@jest/globals';

type TestAccount = {
  provider: 'codex-subscription';
  state: 'signed-out' | 'pending' | 'signed-in' | 'error';
  accountId: string | null;
  generation: number;
};
let account: TestAccount = {
  provider: 'codex-subscription',
  state: 'signed-in',
  accountId: 'account-test',
  generation: 3,
};
const native = {
  getStatus: jest.fn(async () => account),
  startLogin: jest.fn(async () => ({
    kind: 'device-code' as const,
    loginId: 'login-1',
    verificationUrl: 'https://example.invalid/device',
    userCode: 'ABCD-EFGH',
    expiresAt: Date.now() + 60_000,
  })),
  cancelLogin: jest.fn(async () => {}),
  signOut: jest.fn(async () => {
    account = { ...account, state: 'signed-out', accountId: null, generation: account.generation + 1 };
  }),
  listModels: jest.fn(async () => [{
    id: 'gpt-5.4-codex',
    name: 'GPT-5.4 Codex',
    apiBackend: 'responses' as const,
    images: 'supported' as const,
    reasoning: 'supported' as const,
    tools: 'supported' as const,
    recommended: true,
    contextWindow: 256000,
  }]),
  startRequest: jest.fn(async () => {}),
  cancelRequest: jest.fn(async () => {}),
};

jest.unstable_mockModule('@/platform', () => ({
  getPlatform: () => ({
    capabilities: { hasSubscriptionAuth: true },
    subscriptions: native,
  }),
}));

const storeModule = await import('../subscriptionStore');
const modelsModule = await import('../../hooks/useModels');
let previousConnections: string[] | null = null;
let stableConnectionsObserved = false;

function Harness() {
  const connections = storeModule.useAvailableAiConnections();
  if (previousConnections === connections) stableConnectionsObserved = true;
  previousConnections = connections;
  const { groupedByProvider } = modelsModule.useModels(['openai', 'codex-subscription']);
  return <div>
    <span data-testid="connections">{connections.join(',')}</span>
    <span data-testid="api-models">{groupedByProvider.openai.map((model) => model.id).join(',')}</span>
    <span data-testid="codex-models">{groupedByProvider.codexSubscription.map((model) => model.id).join(',')}</span>
  </div>;
}

describe('subscriptionStore', () => {
  beforeEach(() => {
    localStorage.clear();
    account = { provider: 'codex-subscription', state: 'signed-in', accountId: 'account-test', generation: 3 };
    jest.clearAllMocks();
    previousConnections = null;
    stableConnectionsObserved = false;
  });

  it('exposes signed-in readiness and merges a native catalog with cached API models; sign-out clears it', async () => {
    localStorage.setItem('openscad_studio_models_cache', JSON.stringify({
      models: [{ id: 'gpt-5.4', display_name: 'GPT-5.4', provider: 'openai', visionSupport: 'yes' }],
      providers: ['openai'],
      fetchedAt: Date.now(),
    }));

    const view = render(<Harness />);
    await waitFor(() => {
      expect(screen.getByTestId('connections').textContent).toContain('codex-subscription');
      expect(screen.getByTestId('api-models').textContent).toBe('gpt-5.4');
      expect(screen.getByTestId('codex-models').textContent).toBe('gpt-5.4-codex');
    });
    expect(native.listModels).toHaveBeenCalledWith('codex-subscription', 3);
    expect(stableConnectionsObserved).toBe(true);

    await act(async () => storeModule.signOutSubscription('codex-subscription'));
    await waitFor(() => {
      expect(screen.getByTestId('connections').textContent).not.toContain('codex-subscription');
      expect(screen.getByTestId('codex-models').textContent).toBe('');
    });
    view.unmount();
  });
});
