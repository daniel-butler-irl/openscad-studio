import type {
  SubscriptionBridge,
  SubscriptionProvider,
  SubscriptionRequest,
  SubscriptionStreamEvent,
} from '../platform/types';

export interface SubscriptionFetchOptions {
  provider: SubscriptionProvider;
  modelId: string;
  accountGeneration: number;
  createRequestId?: () => string;
}

/**
 * Adapt the native subscription stream to the Response contract expected by the AI SDK.
 * The URL and headers supplied by the SDK are intentionally ignored: the native bridge
 * derives a fixed provider endpoint and attaches the account credential itself.
 */
export function createSubscriptionFetch(
  bridge: SubscriptionBridge,
  options: SubscriptionFetchOptions
): typeof fetch {
  return async (input, init) => {
    const signal = init?.signal ?? (input instanceof Request ? input.signal : undefined);
    if (signal?.aborted) throw abortError();

    const rawBody = init?.body ?? (input instanceof Request ? await input.clone().text() : null);
    if (typeof rawBody !== 'string') {
      throw new Error('The subscription request did not contain a JSON body.');
    }

    let body: Record<string, unknown>;
    try {
      const parsed: unknown = JSON.parse(rawBody);
      if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) {
        throw new Error('Expected a JSON object.');
      }
      body = parsed as Record<string, unknown>;
    } catch {
      throw new Error('The subscription request could not be read.');
    }
    if (signal?.aborted) throw abortError();

    const requestId = options.createRequestId?.() ?? crypto.randomUUID();
    const request: SubscriptionRequest = {
      requestId,
      provider: options.provider,
      accountGeneration: options.accountGeneration,
      modelId: options.modelId,
      body,
    };

    let responseReceived = false;
    let terminal = false;
    let expectedSequence = 0;
    let responseResolve!: (response: Response) => void;
    let responseReject!: (error: Error) => void;
    const responsePromise = new Promise<Response>((resolve, reject) => {
      responseResolve = resolve;
      responseReject = reject;
    });
    let streamController: ReadableStreamDefaultController<Uint8Array> | undefined;
    let cancelSent = false;
    let responseStatus = 0;
    const cancelNative = () => {
      if (cancelSent) return;
      cancelSent = true;
      void Promise.resolve(bridge.cancelRequest(requestId)).catch(() => {});
    };
    const finishFailure = (error: Error, cancelRequest = true, errorBody = true) => {
      if (terminal) return;
      terminal = true;
      if (cancelRequest) cancelNative();
      if (!responseReceived) {
        responseReject(error);
      } else if (error.name === 'AbortError') {
        streamController?.error(error);
      } else if (errorBody && responseStatus >= 400 && streamController) {
        streamController.enqueue(
          new TextEncoder().encode(JSON.stringify({ error: { message: error.message } }))
        );
        streamController.close();
      } else if (errorBody) {
        streamController?.error(error);
      }
      signal?.removeEventListener('abort', cancel);
    };
    const stream = new ReadableStream<Uint8Array>({
      start(controller) {
        streamController = controller;
      },
      cancel() {
        finishFailure(abortError(), true, false);
      },
    });

    const cancel = () => {
      finishFailure(abortError());
    };
    signal?.addEventListener('abort', cancel, { once: true });

    if (signal?.aborted) {
      cancel();
      return responsePromise;
    }

    const onEvent = (event: SubscriptionStreamEvent) => {
      if (terminal) return;
      if (event.requestId !== requestId) return;
      if (event.sequence !== expectedSequence++) {
        finishFailure(new Error('The native subscription stream arrived out of order.'));
        return;
      }

      if (event.kind === 'response') {
        if (
          responseReceived ||
          event.sequence !== 0 ||
          !Number.isInteger(event.status) ||
          event.status < 200 ||
          event.status > 599
        ) {
          finishFailure(new Error('The native subscription response was invalid.'));
          return;
        }
        const safeHeaders = new Headers();
        const contentType = Object.entries(event.headers).find(
          ([name]) => name.toLowerCase() === 'content-type'
        )?.[1];
        if (event.status >= 400) {
          safeHeaders.set('content-type', 'application/json');
        } else if (contentType) {
          safeHeaders.set('content-type', contentType);
        }
        try {
          const response = new Response(stream, { status: event.status, headers: safeHeaders });
          responseReceived = true;
          responseStatus = event.status;
          responseResolve(response);
        } catch {
          finishFailure(new Error('The native subscription response was invalid.'));
        }
      } else if (event.kind === 'chunk') {
        if (!responseReceived) {
          finishFailure(new Error('The native subscription response was incomplete.'));
          return;
        }
        streamController?.enqueue(Uint8Array.from(event.bytes));
      } else if (event.kind === 'complete') {
        if (!responseReceived) {
          finishFailure(
            new Error('The native subscription stream ended before the response started.')
          );
          return;
        }
        terminal = true;
        streamController?.close();
        signal?.removeEventListener('abort', cancel);
      } else if (event.kind === 'error') {
        finishFailure(new Error(event.message), false);
      }
    };

    void bridge.startRequest(request, onEvent).catch((error: unknown) => {
      void error;
      finishFailure(new Error('The native subscription request failed.'));
    });

    return responsePromise;
  };
}

function abortError(): DOMException {
  return new DOMException('The operation was aborted.', 'AbortError');
}
