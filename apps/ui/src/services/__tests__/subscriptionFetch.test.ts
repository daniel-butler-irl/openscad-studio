import { jest } from '@jest/globals';
import { stepCountIs, streamText, tool } from 'ai';
import { createOpenAI } from '@ai-sdk/openai';
import { z } from 'zod';
import { createSubscriptionFetch } from '../subscriptionFetch';
import type {
  SubscriptionBridge,
  SubscriptionRequest,
  SubscriptionStreamEvent,
} from '../../platform/types';

function createBridge(
  onStart: (request: SubscriptionRequest, onEvent: (event: SubscriptionStreamEvent) => void) => void
) {
  const bridge: SubscriptionBridge = {
    getStatus: jest.fn(),
    startLogin: jest.fn(),
    cancelLogin: jest.fn(),
    signOut: jest.fn(),
    listModels: jest.fn(),
    startRequest: jest.fn(async (request, onEvent) => onStart(request, onEvent)),
    cancelRequest: jest.fn(),
  };
  return bridge;
}

describe('createSubscriptionFetch', () => {
  it('returns an ordered streaming Response without forwarding URL or authorization headers', async () => {
    const requestId = 'request-1';
    const bridge = createBridge((request, onEvent) => {
      expect(request).toMatchObject({
        requestId,
        provider: 'codex-subscription',
        accountGeneration: 7,
        modelId: 'gpt-5-codex',
        body: { input: [{ role: 'user', content: 'make a cube' }] },
      });
      expect(request).not.toHaveProperty('url');
      expect(request).not.toHaveProperty('headers');
      onEvent({
        requestId,
        sequence: 0,
        kind: 'response',
        status: 200,
        headers: { 'content-type': 'text/event-stream', 'set-cookie': 'private=1' },
      });

      // Split an SSE tool call at boundaries that do not align with UTF-8 or JSON tokens.
      const bytes = new TextEncoder().encode(
        'event: response.output_item.added\ndata: {"type":"response.output_item.added","item":{"type":"function_call","call_id":"call-1","name":"apply_edit"}}\n\n' +
          'event: response.function_call_arguments.delta\ndata: {"type":"response.function_call_arguments.delta","delta":"{\\"path\\":\\"main.scad\\"}"}\n\n' +
          'event: response.output_item.done\ndata: {"type":"response.output_item.done","item":{"type":"function_call","call_id":"call-1","name":"apply_edit","arguments":"{\\"path\\":\\"main.scad\\"}"}}\n\n'
      );
      onEvent({ requestId, sequence: 1, kind: 'chunk', bytes: [...bytes.slice(0, 41)] });
      onEvent({ requestId, sequence: 2, kind: 'chunk', bytes: [...bytes.slice(41, 123)] });
      onEvent({ requestId, sequence: 3, kind: 'chunk', bytes: [...bytes.slice(123)] });
      onEvent({ requestId, sequence: 4, kind: 'complete' });
    });
    const fetch = createSubscriptionFetch(bridge, {
      provider: 'codex-subscription',
      modelId: 'gpt-5-codex',
      accountGeneration: 7,
      createRequestId: () => requestId,
    });

    const response = await fetch('https://api.openai.com/v1/responses', {
      method: 'POST',
      headers: {
        authorization: 'Bearer frontend-must-not-send',
        'content-type': 'application/json',
      },
      body: JSON.stringify({ input: [{ role: 'user', content: 'make a cube' }] }),
    });

    expect(response.status).toBe(200);
    expect(response.headers.get('content-type')).toBe('text/event-stream');
    expect(response.headers.has('set-cookie')).toBe(false);
    const streamedText = await response.text();
    expect(streamedText).toContain('response.function_call_arguments.delta');
    expect(streamedText).toContain('"call_id":"call-1"');
    expect(streamedText).toContain('"name":"apply_edit"');
    expect(bridge.startRequest).toHaveBeenCalledTimes(1);
  });

  it('preserves tool results in subsequent requests and maps sanitized entitlement errors to SDK JSON', async () => {
    const bridge = createBridge((request, onEvent) => {
      expect(request.body).toEqual({
        input: [
          { type: 'function_call_output', call_id: 'call-1', output: '{"status":"success"}' },
        ],
      });
      onEvent({
        requestId: request.requestId,
        sequence: 0,
        kind: 'response',
        status: 403,
        headers: { 'content-type': 'text/html' },
      });
      onEvent({
        requestId: request.requestId,
        sequence: 1,
        kind: 'error',
        message: 'This subscription does not include access to the selected model.',
      });
    });
    const fetch = createSubscriptionFetch(bridge, {
      provider: 'grok-subscription',
      modelId: 'grok-4.6',
      accountGeneration: 2,
      createRequestId: () => 'request-2',
    });

    const response = await fetch('https://api.openai.com/v1/responses', {
      method: 'POST',
      body: JSON.stringify({
        input: [
          { type: 'function_call_output', call_id: 'call-1', output: '{"status":"success"}' },
        ],
      }),
    });

    expect(response.status).toBe(403);
    expect(response.headers.get('content-type')).toBe('application/json');
    expect(await response.json()).toEqual({
      error: { message: 'This subscription does not include access to the selected model.' },
    });
  });

  it('cancels the native request when the SDK aborts', async () => {
    const bridge = createBridge((_request, onEvent) => {
      onEvent({ requestId: 'request-3', sequence: 0, kind: 'response', status: 200, headers: {} });
    });
    const fetch = createSubscriptionFetch(bridge, {
      provider: 'codex-subscription',
      modelId: 'gpt-5-codex',
      accountGeneration: 1,
      createRequestId: () => 'request-3',
    });
    const controller = new AbortController();
    const response = await fetch('https://api.openai.com/v1/responses', {
      method: 'POST',
      signal: controller.signal,
      body: JSON.stringify({ input: [] }),
    });

    controller.abort();
    await expect(response.text()).rejects.toHaveProperty('name', 'AbortError');
    expect(bridge.cancelRequest).toHaveBeenCalledWith('request-3');
  });

  it('lets the AI SDK parse a fragmented function call and send its tool result on the next request', async () => {
    const capturedBodies: Array<Record<string, unknown>> = [];
    let turn = 0;
    const bridge = createBridge((request, onEvent) => {
      capturedBodies.push(request.body);
      const events =
        turn++ === 0
          ? [
              {
                type: 'response.created',
                response: { id: 'resp-1', status: 'in_progress', output: [] },
              },
              {
                type: 'response.output_item.added',
                output_index: 0,
                item: {
                  id: 'fc-item-1',
                  type: 'function_call',
                  call_id: 'call-1',
                  name: 'apply_edit',
                  arguments: '',
                  status: 'in_progress',
                },
              },
              {
                type: 'response.function_call_arguments.delta',
                output_index: 0,
                item_id: 'fc-item-1',
                delta: '{"path":"main.scad"}',
              },
              {
                type: 'response.output_item.done',
                output_index: 0,
                item: {
                  id: 'fc-item-1',
                  type: 'function_call',
                  call_id: 'call-1',
                  name: 'apply_edit',
                  arguments: '{"path":"main.scad"}',
                  status: 'completed',
                },
              },
              {
                type: 'response.completed',
                response: {
                  id: 'resp-1',
                  status: 'completed',
                  output: [],
                  usage: { input_tokens: 5, output_tokens: 4 },
                },
              },
            ]
          : [
              {
                type: 'response.created',
                response: { id: 'resp-2', status: 'in_progress', output: [] },
              },
              {
                type: 'response.output_item.added',
                output_index: 0,
                item: { id: 'msg-1', type: 'message', role: 'assistant', content: [] },
              },
              {
                type: 'response.content_part.added',
                output_index: 0,
                item_id: 'msg-1',
                content_index: 0,
                part: { type: 'output_text', text: '' },
              },
              {
                type: 'response.output_text.delta',
                output_index: 0,
                item_id: 'msg-1',
                content_index: 0,
                delta: 'Done.',
              },
              {
                type: 'response.output_text.done',
                output_index: 0,
                item_id: 'msg-1',
                content_index: 0,
                text: 'Done.',
              },
              {
                type: 'response.output_item.done',
                output_index: 0,
                item: {
                  id: 'msg-1',
                  type: 'message',
                  role: 'assistant',
                  status: 'completed',
                  content: [{ type: 'output_text', text: 'Done.', annotations: [] }],
                },
              },
              {
                type: 'response.completed',
                response: {
                  id: 'resp-2',
                  status: 'completed',
                  output: [],
                  usage: { input_tokens: 9, output_tokens: 2 },
                },
              },
            ];
      const sse = `${events.map((event) => `data: ${JSON.stringify(event)}\n\n`).join('')}data: [DONE]\n\n`;
      const bytes = new TextEncoder().encode(sse);
      onEvent({
        requestId: request.requestId,
        sequence: 0,
        kind: 'response',
        status: 200,
        headers: { 'content-type': 'text/event-stream' },
      });
      // Deliberately split the SSE byte stream inside JSON strings and event boundaries.
      let sequence = 1;
      for (let offset = 0; offset < bytes.length; sequence++) {
        const next = Math.min(bytes.length, offset + (sequence % 11) + 1);
        onEvent({
          requestId: request.requestId,
          sequence,
          kind: 'chunk',
          bytes: [...bytes.slice(offset, next)],
        });
        offset = next;
      }
      onEvent({ requestId: request.requestId, sequence, kind: 'complete' });
    });
    const modelProvider = createOpenAI({
      apiKey: 'native-managed',
      fetch: createSubscriptionFetch(bridge, {
        provider: 'codex-subscription',
        modelId: 'gpt-5-codex',
        accountGeneration: 8,
        createRequestId: (() => {
          let id = 0;
          return () => `sdk-request-${++id}`;
        })(),
      }),
    });
    const result = streamText({
      model: modelProvider.responses('gpt-5-codex'),
      prompt: 'Inspect and edit the project.',
      tools: {
        apply_edit: tool({
          description: 'Edit a project file.',
          inputSchema: z.object({ path: z.string() }),
          execute: async ({ path }) => ({ status: 'success', path }),
        }),
      },
      stopWhen: stepCountIs(2),
    });
    const observed: string[] = [];
    for await (const part of result.fullStream) {
      if (part.type === 'tool-call')
        observed.push(`call:${part.toolName}:${JSON.stringify(part.input)}`);
      if (part.type === 'tool-result') observed.push(`result:${JSON.stringify(part.output)}`);
      if (part.type === 'text-delta') observed.push(`text:${part.text}`);
    }

    expect(observed).toContain('call:apply_edit:{"path":"main.scad"}');
    expect(observed).toContain('result:{"status":"success","path":"main.scad"}');
    expect(observed).toContain('text:Done.');
    expect(capturedBodies).toHaveLength(2);
    expect(JSON.stringify(capturedBodies[1])).toContain('function_call_output');
    expect(JSON.stringify(capturedBodies[1])).toContain('"call_id":"call-1"');
  });
});
