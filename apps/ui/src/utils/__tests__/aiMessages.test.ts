import { getVisionSupportForModelId, messagesToModelMessages } from '../aiMessages';
import type { AttachmentStore, Message } from '../../types/aiChat';

describe('aiMessages', () => {
  it('replays scoped Codex encrypted reasoning and tool results on the next SDK turn', () => {
    const messages: Message[] = [
      {
        id: 'user-1',
        timestamp: 1,
        type: 'user',
        parts: [{ type: 'text', text: 'Inspect and revise.' }],
      },
      {
        id: 'assistant-1',
        timestamp: 2,
        type: 'assistant',
        turnId: 'turn-1',
        content: 'I inspected the source.',
        state: 'complete',
      },
      {
        id: 'tool-1',
        timestamp: 3,
        type: 'tool-call',
        toolCallId: 'call-1',
        toolName: 'read_file',
        args: { path: 'main.scad' },
        state: 'completed',
        result: 'cube(10);',
      },
      {
        id: 'assistant-2',
        timestamp: 4,
        type: 'assistant',
        turnId: 'turn-1',
        content: 'The file contains a cube.',
        state: 'complete',
        continuation: {
          provider: 'codex-subscription',
          accountGeneration: 7,
          itemId: 'reasoning-item-8',
          encryptedContent: 'opaque-encrypted-reasoning',
        },
      },
      {
        id: 'user-2',
        timestamp: 5,
        type: 'user',
        parts: [{ type: 'text', text: 'Make it larger.' }],
      },
    ];

    const nextTurn = messagesToModelMessages(
      messages,
      {},
      {
        provider: 'codex-subscription',
        accountGeneration: 7,
      }
    );
    expect(nextTurn).toContainEqual({
      role: 'assistant',
      content: [
        {
          type: 'reasoning',
          text: '',
          providerOptions: {
            openai: {
              itemId: 'reasoning-item-8',
              reasoningEncryptedContent: 'opaque-encrypted-reasoning',
            },
          },
        },
        { type: 'text', text: 'The file contains a cube.' },
      ],
    });
    expect(nextTurn).toContainEqual({
      role: 'tool',
      content: [
        {
          type: 'tool-result',
          toolCallId: 'call-1',
          toolName: 'read_file',
          output: { type: 'text', value: 'cube(10);' },
        },
      ],
    });
    expect(
      messagesToModelMessages(
        messages,
        {},
        {
          provider: 'codex-subscription',
          accountGeneration: 8,
        }
      ).some(
        (message) =>
          message.role === 'assistant' &&
          Array.isArray(message.content) &&
          message.content.some((part) => part.type === 'reasoning')
      )
    ).toBe(false);
  });

  it('maps text and image user parts into multimodal model messages', () => {
    const messages: Message[] = [
      {
        id: 'user-1',
        timestamp: 1,
        type: 'user',
        parts: [
          { type: 'text', text: 'Describe this model.' },
          {
            type: 'image',
            attachmentId: 'image-1',
            filename: 'reference.png',
            mimeType: 'image/png',
            width: 640,
            height: 480,
          },
        ],
      },
    ];

    const attachments: AttachmentStore = {
      'image-1': {
        id: 'image-1',
        filename: 'reference.png',
        sourceMimeType: 'image/png',
        normalizedMimeType: 'image/png',
        sizeBytes: 2048,
        normalizedData: 'ZmFrZS1pbWFnZS1iYXNlNjQ=',
        status: 'ready',
        dedupeKey: 'reference.png:2048:1',
      },
    };

    expect(messagesToModelMessages(messages, attachments)).toEqual([
      {
        role: 'user',
        content: [
          { type: 'text', text: 'Describe this model.' },
          {
            type: 'image',
            image: 'ZmFrZS1pbWFnZS1iYXNlNjQ=',
            mediaType: 'image/png',
          },
        ],
      },
    ]);
  });

  it('serializes completed tool calls before later assistant text using toolCallId', () => {
    const messages: Message[] = [
      {
        id: 'user-1',
        timestamp: 1,
        type: 'user',
        parts: [{ type: 'text', text: 'Inspect the file.' }],
      },
      {
        id: 'assistant-1',
        timestamp: 2,
        type: 'assistant',
        turnId: 'turn-1',
        content: 'Checking the file.',
        state: 'complete',
      },
      {
        id: 'tool-msg-1',
        timestamp: 3,
        type: 'tool-call',
        toolCallId: 'tool-1',
        toolName: 'read_file',
        args: { path: 'main.scad' },
        state: 'completed',
        result: 'cube(10);',
      },
      {
        id: 'assistant-2',
        timestamp: 4,
        type: 'assistant',
        turnId: 'turn-1',
        content: 'Done.',
        state: 'complete',
      },
    ];

    expect(messagesToModelMessages(messages, {})).toEqual([
      {
        role: 'user',
        content: [{ type: 'text', text: 'Inspect the file.' }],
      },
      {
        role: 'assistant',
        content: [{ type: 'text', text: 'Checking the file.' }],
      },
      {
        role: 'assistant',
        content: [
          {
            type: 'tool-call',
            toolCallId: 'tool-1',
            toolName: 'read_file',
            input: { path: 'main.scad' },
          },
        ],
      },
      {
        role: 'tool',
        content: [
          {
            type: 'tool-result',
            toolCallId: 'tool-1',
            toolName: 'read_file',
            output: { type: 'text', value: 'cube(10);' },
          },
        ],
      },
      {
        role: 'assistant',
        content: [{ type: 'text', text: 'Done.' }],
      },
    ]);
  });

  it('does not replay incomplete or failed tool calls as completed tool history', () => {
    const messages: Message[] = [
      {
        id: 'user-1',
        timestamp: 1,
        type: 'user',
        parts: [{ type: 'text', text: 'Inspect the file.' }],
      },
      {
        id: 'assistant-1',
        timestamp: 2,
        type: 'assistant',
        turnId: 'turn-1',
        content: 'Partial answer.',
        state: 'error',
      },
      {
        id: 'tool-msg-1',
        timestamp: 3,
        type: 'tool-call',
        toolCallId: 'tool-1',
        toolName: 'read_file',
        args: { path: 'main.scad' },
        state: 'pending',
      },
      {
        id: 'tool-msg-2',
        timestamp: 4,
        type: 'tool-call',
        toolCallId: 'tool-2',
        toolName: 'apply_edit',
        args: { old_string: 'a', new_string: 'b' },
        state: 'error',
        errorText: 'Edit failed',
      },
    ];

    expect(messagesToModelMessages(messages, {})).toEqual([
      {
        role: 'user',
        content: [{ type: 'text', text: 'Inspect the file.' }],
      },
      {
        role: 'assistant',
        content: [{ type: 'text', text: 'Partial answer.' }],
      },
    ]);
  });

  it('omits hidden tool result metadata when replaying tool history to the model', () => {
    const messages: Message[] = [
      {
        id: 'user-1',
        timestamp: 1,
        type: 'user',
        parts: [{ type: 'text', text: 'Patch the file.' }],
      },
      {
        id: 'tool-msg-1',
        timestamp: 2,
        type: 'tool-call',
        toolCallId: 'tool-1',
        toolName: 'apply_edit',
        args: { old_string: 'a', new_string: 'b' },
        state: 'completed',
        result: {
          status: 'success',
          message: 'Edit applied successfully.',
          __checkpointId: 'cp-123',
        },
      },
    ];

    expect(messagesToModelMessages(messages, {})).toEqual([
      {
        role: 'user',
        content: [{ type: 'text', text: 'Patch the file.' }],
      },
      {
        role: 'assistant',
        content: [
          {
            type: 'tool-call',
            toolCallId: 'tool-1',
            toolName: 'apply_edit',
            input: { old_string: 'a', new_string: 'b' },
          },
        ],
      },
      {
        role: 'tool',
        content: [
          {
            type: 'tool-result',
            toolCallId: 'tool-1',
            toolName: 'apply_edit',
            output: {
              type: 'text',
              value: JSON.stringify({
                status: 'success',
                message: 'Edit applied successfully.',
              }),
            },
          },
        ],
      },
    ]);
  });

  it('reports known vision support for configured model families', () => {
    expect(getVisionSupportForModelId('claude-sonnet-4-5')).toBe('yes');
    expect(getVisionSupportForModelId('gpt-4o')).toBe('yes');
    expect(getVisionSupportForModelId('text-only-model')).toBe('no');
    expect(getVisionSupportForModelId('mystery-model')).toBe('unknown');
  });
});
