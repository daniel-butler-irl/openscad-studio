import { useCallback, useEffect, useMemo, useSyncExternalStore } from 'react';
import type {
  AiConnectionProvider,
  SubscriptionAccountStatus,
  SubscriptionLoginStart,
  SubscriptionModelInfo,
  SubscriptionProvider,
} from '../platform/types';
import { getPlatform } from '../platform';
import { useAvailableProviders } from './apiKeyStore';

type SubscriptionStoreSnapshot = {
  status: Record<SubscriptionProvider, SubscriptionAccountStatus>;
  pendingLogins: Partial<Record<SubscriptionProvider, SubscriptionLoginStart>>;
  models: Partial<Record<SubscriptionProvider, SubscriptionModelInfo[]>>;
  loadingModels: Partial<Record<SubscriptionProvider, boolean>>;
  errors: Partial<Record<SubscriptionProvider, string>>;
};

const providers: SubscriptionProvider[] = ['codex-subscription', 'grok-subscription'];

function emptyStatus(provider: SubscriptionProvider): SubscriptionAccountStatus {
  return { provider, state: 'signed-out', accountId: null, generation: 0 };
}

let snapshot: SubscriptionStoreSnapshot = {
  status: {
    'codex-subscription': emptyStatus('codex-subscription'),
    'grok-subscription': emptyStatus('grok-subscription'),
  },
  pendingLogins: {},
  models: {},
  loadingModels: {},
  errors: {},
};

const listeners = new Set<() => void>();
const statusSequences = new Map<SubscriptionProvider, number>();
const modelSequences = new Map<SubscriptionProvider, number>();
const modelCache = new Map<string, SubscriptionModelInfo[]>();
let statusBootstrapPromise: Promise<void> | null = null;

function emit(): void {
  for (const listener of listeners) listener();
}

function update(patch: Partial<SubscriptionStoreSnapshot>): void {
  snapshot = { ...snapshot, ...patch };
  emit();
}

function bridge() {
  const platform = getPlatform();
  if (!platform.capabilities.hasSubscriptionAuth || !platform.subscriptions) return null;
  return platform.subscriptions;
}

function setStatus(status: SubscriptionAccountStatus): void {
  const previous = snapshot.status[status.provider];
  const statusByProvider = { ...snapshot.status, [status.provider]: status };
  const patch: Partial<SubscriptionStoreSnapshot> = { status: statusByProvider };
  if (status.state !== 'pending' && snapshot.pendingLogins[status.provider]) {
    const pendingLogins = { ...snapshot.pendingLogins };
    delete pendingLogins[status.provider];
    patch.pendingLogins = pendingLogins;
  }
  if (previous.generation !== status.generation) {
    for (const key of modelCache.keys()) {
      if (key.startsWith(`${status.provider}:`)) modelCache.delete(key);
    }
    patch.models = { ...snapshot.models, [status.provider]: undefined };
  }
  if (status.state !== 'error') {
    patch.errors = { ...snapshot.errors, [status.provider]: undefined };
  }
  update(patch);
}

export function getSubscriptionSnapshot(): SubscriptionStoreSnapshot {
  return snapshot;
}

export function subscribeToSubscriptions(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

export function useSubscriptionStore(): SubscriptionStoreSnapshot {
  useEffect(() => {
    let disposed = false;
    const refresh = () => {
      if (disposed || document.visibilityState === 'hidden') return;
      if (statusBootstrapPromise) return;
      statusBootstrapPromise = refreshSubscriptionStatus().finally(() => {
        statusBootstrapPromise = null;
      });
    };
    refresh();
    const interval = window.setInterval(refresh, bridge() ? 30_000 : 1_000);
    window.addEventListener('focus', refresh);
    document.addEventListener('visibilitychange', refresh);
    return () => {
      disposed = true;
      window.clearInterval(interval);
      window.removeEventListener('focus', refresh);
      document.removeEventListener('visibilitychange', refresh);
    };
  }, []);
  return useSyncExternalStore(
    subscribeToSubscriptions,
    getSubscriptionSnapshot,
    getSubscriptionSnapshot
  );
}

export function useAvailableAiConnections(): AiConnectionProvider[] {
  const apiProviders = useAvailableProviders();
  const { status } = useSubscriptionStore();
  return useMemo(() => {
    const connectedSubscriptions = providers.filter(
      (provider) => status[provider].state === 'signed-in'
    );
    return [...apiProviders, ...connectedSubscriptions];
  }, [apiProviders, status]);
}

export async function refreshSubscriptionStatus(provider?: SubscriptionProvider): Promise<void> {
  const native = bridge();
  if (!native) return;
  const requestedProviders = provider ? [provider] : providers;
  await Promise.all(
    requestedProviders.map(async (entry) => {
      const sequence = (statusSequences.get(entry) ?? 0) + 1;
      statusSequences.set(entry, sequence);
      try {
        const status = await native.getStatus(entry);
        if (statusSequences.get(entry) === sequence) setStatus(status);
      } catch (error) {
        if (statusSequences.get(entry) !== sequence) return;
        const message =
          error instanceof Error ? error.message : 'Could not read subscription status.';
        setStatus({ ...snapshot.status[entry], state: 'error', message });
      }
    })
  );
}

export async function startSubscriptionLogin(
  provider: SubscriptionProvider
): Promise<SubscriptionLoginStart> {
  const native = bridge();
  if (!native) throw new Error('Subscription sign-in is available in the desktop app.');
  const challenge = await native.startLogin(provider);
  update({
    pendingLogins: { ...snapshot.pendingLogins, [provider]: challenge },
    status: { ...snapshot.status, [provider]: { ...snapshot.status[provider], state: 'pending' } },
    errors: { ...snapshot.errors, [provider]: undefined },
  });
  return challenge;
}

export async function cancelSubscriptionLogin(provider: SubscriptionProvider): Promise<void> {
  const native = bridge();
  const challenge = snapshot.pendingLogins[provider];
  if (native && challenge) await native.cancelLogin(provider, challenge.loginId);
  const pendingLogins = { ...snapshot.pendingLogins };
  delete pendingLogins[provider];
  update({ pendingLogins });
  await refreshSubscriptionStatus(provider);
}

export async function signOutSubscription(provider: SubscriptionProvider): Promise<void> {
  const native = bridge();
  if (!native) throw new Error('Subscription sign-out is available in the desktop app.');
  statusSequences.set(provider, (statusSequences.get(provider) ?? 0) + 1);
  const current = snapshot.status[provider];
  const pendingLogins = { ...snapshot.pendingLogins };
  delete pendingLogins[provider];
  const models = { ...snapshot.models };
  delete models[provider];
  for (const key of modelCache.keys()) {
    if (key.startsWith(`${provider}:`)) modelCache.delete(key);
  }
  update({
    pendingLogins,
    models,
    status: {
      ...snapshot.status,
      [provider]: { ...current, state: 'signed-out', accountId: null },
    },
    errors: { ...snapshot.errors, [provider]: undefined },
  });
  try {
    await native.signOut(provider);
    await refreshSubscriptionStatus(provider);
  } catch (error) {
    const message = error instanceof Error ? error.message : 'Could not sign out of this account.';
    update({
      status: {
        ...snapshot.status,
        [provider]: { ...snapshot.status[provider], state: 'error', message },
      },
      errors: { ...snapshot.errors, [provider]: message },
    });
    throw error;
  }
}

export async function refreshSubscriptionModels(
  provider: SubscriptionProvider,
  force = false
): Promise<SubscriptionModelInfo[]> {
  const native = bridge();
  const account = snapshot.status[provider];
  if (!native || account.state !== 'signed-in') return [];
  const key = `${provider}:${account.generation}`;
  const cached = modelCache.get(key);
  if (cached && !force) return cached;
  const requestId = (modelSequences.get(provider) ?? 0) + 1;
  modelSequences.set(provider, requestId);

  update({
    loadingModels: { ...snapshot.loadingModels, [provider]: true },
    errors: { ...snapshot.errors, [provider]: undefined },
  });
  try {
    const models = await native.listModels(provider, account.generation);
    const current = snapshot.status[provider];
    if (
      modelSequences.get(provider) !== requestId ||
      current.generation !== account.generation ||
      current.state !== 'signed-in'
    )
      return [];
    modelCache.set(key, models);
    update({ models: { ...snapshot.models, [provider]: models } });
    return models;
  } catch (error) {
    const current = snapshot.status[provider];
    if (modelSequences.get(provider) !== requestId || current.generation !== account.generation)
      return [];
    const message = error instanceof Error ? error.message : 'Could not load subscription models.';
    update({ errors: { ...snapshot.errors, [provider]: message } });
    throw error;
  } finally {
    if (modelSequences.get(provider) === requestId) {
      update({ loadingModels: { ...snapshot.loadingModels, [provider]: false } });
    }
  }
}

export function useRefreshSubscriptionStatus(): () => Promise<void> {
  return useCallback(() => refreshSubscriptionStatus(), []);
}
