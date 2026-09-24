import { useCallback, useEffect, useRef, useState } from 'react';
import type { SubscriptionProvider } from '../../platform/types';
import { getPlatform } from '../../platform';
import {
  cancelSubscriptionLogin,
  refreshSubscriptionModels,
  refreshSubscriptionStatus,
  signOutSubscription,
  startSubscriptionLogin,
  useSubscriptionStore,
} from '../../stores/subscriptionStore';
import { Button, Text } from '../ui';
import { notifyError, notifySuccess } from '../../utils/notifications';
import { SettingsCard, SettingsCardHeader, SettingsCardSection } from './SettingsPrimitives';

const PROVIDERS: Array<{
  id: SubscriptionProvider;
  title: string;
  description: string;
}> = [
  {
    id: 'codex-subscription',
    title: 'ChatGPT subscription',
    description: 'Use models available to your ChatGPT account.',
  },
  {
    id: 'grok-subscription',
    title: 'Grok subscription',
    description: 'Use models available to your xAI account.',
  },
];

type CardState = { busy: boolean; error: string | null };

function safeMessage(error: unknown, fallback: string): string {
  const candidate = typeof error === 'string' ? error : error instanceof Error ? error.message : '';
  if (
    candidate.length > 0 &&
    candidate.length <= 240 &&
    !/(bearer|authorization|refresh.?token|access.?token|https?:\/\/)/i.test(candidate)
  ) {
    return candidate;
  }
  return fallback;
}

export function SubscriptionSettings({ isOpen }: { isOpen: boolean }) {
  const bridge = getPlatform().subscriptions;
  const { status, pendingLogins, errors } = useSubscriptionStore();
  const [cards, setCards] = useState<Record<SubscriptionProvider, CardState>>({
    'codex-subscription': { busy: false, error: null },
    'grok-subscription': { busy: false, error: null },
  });
  const loadedModelGenerations = useRef<Partial<Record<SubscriptionProvider, number>>>({});

  const refreshStatuses = useCallback(async () => {
    if (bridge) await refreshSubscriptionStatus();
  }, [bridge]);
  const hasPendingLogin = Object.values(pendingLogins).some(Boolean);

  useEffect(() => {
    if (!isOpen || !bridge) return;
    void refreshStatuses().catch(() => {});
    if (!hasPendingLogin) return;
    const timer = window.setInterval(() => {
      void refreshStatuses().catch(() => {});
    }, 1500);
    return () => window.clearInterval(timer);
  }, [bridge, hasPendingLogin, isOpen, refreshStatuses]);

  useEffect(() => {
    for (const { id } of PROVIDERS) {
      const account = status[id];
      if (
        account.state !== 'signed-in' ||
        loadedModelGenerations.current[id] === account.generation
      ) {
        continue;
      }
      loadedModelGenerations.current[id] = account.generation;
      void refreshSubscriptionModels(id).catch(() => {
        if (loadedModelGenerations.current[id] === account.generation) {
          delete loadedModelGenerations.current[id];
        }
      });
    }
  }, [status]);

  const updateCard = (provider: SubscriptionProvider, patch: Partial<CardState>) => {
    setCards((current) => ({
      ...current,
      [provider]: { ...current[provider], ...patch },
    }));
  };

  const copyText = async (provider: SubscriptionProvider, label: string, value: string) => {
    try {
      await navigator.clipboard.writeText(value);
      notifySuccess(`${label} copied`, { toastId: `copy-subscription-${provider}-${label}` });
    } catch (error) {
      notifyError({
        operation: 'copy-subscription-sign-in',
        error,
        fallbackMessage: `Could not copy the ${label.toLowerCase()}.`,
        toastId: `copy-subscription-error-${provider}-${label}`,
      });
    }
  };

  const handleSignIn = async (provider: SubscriptionProvider) => {
    if (!bridge) return;
    updateCard(provider, { busy: true, error: null });
    try {
      const challenge = await startSubscriptionLogin(provider);
      updateCard(provider, { busy: false });
      try {
        const { openUrl } = await import('@tauri-apps/plugin-opener');
        await openUrl(challenge.verificationUrl);
      } catch {
        // Keep the visible link and code as a fallback if external opening fails.
      }
    } catch (error) {
      const message = safeMessage(error, 'Could not start sign-in.');
      updateCard(provider, { busy: false, error: message });
    }
  };

  const handleCancel = async (provider: SubscriptionProvider) => {
    updateCard(provider, { busy: true, error: null });
    try {
      await cancelSubscriptionLogin(provider);
      updateCard(provider, { busy: false });
    } catch (error) {
      const message = safeMessage(error, 'Could not cancel sign-in.');
      updateCard(provider, { busy: false, error: message });
    }
  };

  const handleSignOut = async (provider: SubscriptionProvider) => {
    updateCard(provider, { busy: true, error: null });
    try {
      await signOutSubscription(provider);
      updateCard(provider, { busy: false });
      notifySuccess('Subscription disconnected', { toastId: `subscription-signout-${provider}` });
    } catch (error) {
      const message = safeMessage(error, 'Could not disconnect account.');
      updateCard(provider, { busy: false, error: message });
    }
  };

  if (!bridge) return null;

  return (
    <div className="flex flex-col" style={{ gap: 'var(--space-section-gap)' }}>
      <Text variant="body" color="secondary">
        Sign in with your provider account. Subscription credentials stay in the desktop secure
        store and are never shared with the web app.
      </Text>
      {PROVIDERS.map(({ id, title, description }) => {
        const account = status[id];
        const challenge = pendingLogins[id];
        const card = cards[id];
        const signedIn = account.state === 'signed-in';
        return (
          <SettingsCard key={id} className="ph-no-capture">
            <SettingsCardHeader
              title={title}
              description={description}
              action={
                <span
                  className="text-xs px-2 py-0.5 rounded-full font-medium"
                  style={{
                    backgroundColor: signedIn
                      ? 'rgba(133, 153, 0, 0.15)'
                      : 'rgba(128, 128, 128, 0.1)',
                    color: signedIn ? 'var(--color-success)' : 'var(--text-tertiary)',
                  }}
                >
                  {signedIn
                    ? 'Connected'
                    : account.state === 'pending'
                      ? 'Signing in'
                      : 'Not connected'}
                </span>
              }
            />
            <SettingsCardSection
              className="flex flex-col"
              style={{ gap: 'var(--space-field-gap)' }}
            >
              {signedIn ? (
                <>
                  <Text variant="caption" color="secondary">
                    {account.accountId ? `Signed in as ${account.accountId}` : 'Account connected'}
                  </Text>
                  {account.message ? (
                    <Text variant="caption" color="error" role="alert">
                      {account.message}
                    </Text>
                  ) : null}
                  <Button
                    type="button"
                    size="sm"
                    variant="secondary"
                    disabled={card.busy}
                    onClick={() => void handleSignOut(id)}
                  >
                    {card.busy ? 'Disconnecting…' : 'Disconnect'}
                  </Button>
                </>
              ) : challenge ? (
                <>
                  <Text variant="caption" color="secondary">
                    Continue sign-in in your browser. This code expires at{' '}
                    {new Date(challenge.expiresAt).toLocaleTimeString()}.
                  </Text>
                  <a
                    href={challenge.verificationUrl}
                    target="_blank"
                    rel="noreferrer"
                    className="text-sm underline"
                    style={{ color: 'var(--text-accent)' }}
                  >
                    Open provider sign-in
                  </a>
                  <Button
                    type="button"
                    size="sm"
                    variant="ghost"
                    onClick={() => void copyText(id, 'Sign-in link', challenge.verificationUrl)}
                  >
                    Copy sign-in link
                  </Button>
                  {challenge.userCode ? (
                    <div className="flex items-center" style={{ gap: 'var(--space-control-gap)' }}>
                      <Text variant="caption" color="secondary">
                        Code
                      </Text>
                      <code className="font-mono text-sm">{challenge.userCode}</code>
                      <Button
                        type="button"
                        size="sm"
                        variant="ghost"
                        onClick={() => void copyText(id, 'Sign-in code', challenge.userCode)}
                      >
                        Copy code
                      </Button>
                    </div>
                  ) : null}
                  <Button
                    type="button"
                    size="sm"
                    variant="ghost"
                    disabled={card.busy}
                    onClick={() => void handleCancel(id)}
                  >
                    Cancel sign-in
                  </Button>
                </>
              ) : (
                <Button
                  type="button"
                  size="sm"
                  variant="secondary"
                  disabled={card.busy}
                  onClick={() => void handleSignIn(id)}
                >
                  {card.busy ? 'Starting…' : 'Sign in'}
                </Button>
              )}
              {card.error || errors[id] ? (
                <Text variant="caption" color="error" role="alert">
                  {card.error ?? errors[id]}
                </Text>
              ) : null}
            </SettingsCardSection>
          </SettingsCard>
        );
      })}
    </div>
  );
}
