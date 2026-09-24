/** @jest-environment jsdom */

import { act, render } from '@testing-library/react';
import { jest } from '@jest/globals';
import type { SubscriptionAccountStatus, SubscriptionLoginStart } from '../../../platform/types';

type TestSnapshot = {
  status: Record<'codex-subscription' | 'grok-subscription', SubscriptionAccountStatus>;
  pendingLogins: Partial<
    Record<'codex-subscription' | 'grok-subscription', SubscriptionLoginStart>
  >;
  errors: Partial<Record<'codex-subscription' | 'grok-subscription', string>>;
};

let snapshot: TestSnapshot;
const refreshSubscriptionStatus = jest.fn(async () => {});

jest.unstable_mockModule('@/platform', () => ({
  getPlatform: () => ({ subscriptions: {} }),
}));

jest.unstable_mockModule('@/stores/subscriptionStore', () => ({
  useSubscriptionStore: () => snapshot,
  refreshSubscriptionStatus,
  refreshSubscriptionModels: jest.fn(async () => []),
  signOutSubscription: jest.fn(async () => {}),
  startSubscriptionLogin: jest.fn(async () => ({
    kind: 'device-code',
    loginId: 'test-login',
    verificationUrl: 'https://example.invalid/device',
    userCode: 'TEST-CODE',
    expiresAt: Date.now() + 60_000,
  })),
  cancelSubscriptionLogin: jest.fn(async () => {}),
}));

const { SubscriptionSettings } = await import('../SubscriptionSettings');

function account(
  provider: 'codex-subscription' | 'grok-subscription',
  state: SubscriptionAccountStatus['state']
): SubscriptionAccountStatus {
  return { provider, state, accountId: null, generation: 0 };
}

function pendingLogin(): SubscriptionLoginStart {
  return {
    kind: 'device-code',
    loginId: 'test-login',
    verificationUrl: 'https://example.invalid/device',
    userCode: 'TEST-CODE',
    expiresAt: Date.now() + 60_000,
  };
}

describe('SubscriptionSettings status polling', () => {
  beforeEach(() => {
    jest.useFakeTimers();
    jest.clearAllMocks();
    snapshot = {
      status: {
        'codex-subscription': account('codex-subscription', 'pending'),
        'grok-subscription': account('grok-subscription', 'signed-out'),
      },
      pendingLogins: { 'codex-subscription': pendingLogin() },
      errors: {},
    };
  });

  afterEach(() => {
    jest.useRealTimers();
  });

  it.each(['signed-in', 'error'] as const)(
    'stops the fast poll when login changes to %s',
    async (state) => {
      const view = render(<SubscriptionSettings isOpen />);
      expect(refreshSubscriptionStatus).toHaveBeenCalledTimes(1);

      await act(async () => {
        jest.advanceTimersByTime(3000);
      });
      expect(refreshSubscriptionStatus).toHaveBeenCalledTimes(3);

      snapshot = {
        ...snapshot,
        status: { ...snapshot.status, 'codex-subscription': account('codex-subscription', state) },
        pendingLogins: {},
      };
      await act(async () => view.rerender(<SubscriptionSettings isOpen />));
      expect(refreshSubscriptionStatus).toHaveBeenCalledTimes(4);

      await act(async () => {
        jest.advanceTimersByTime(5000);
      });
      expect(refreshSubscriptionStatus).toHaveBeenCalledTimes(4);
      view.unmount();
    }
  );
});
