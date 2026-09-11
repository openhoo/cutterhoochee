import assert from 'node:assert/strict';
import test from 'node:test';
import { PassThrough } from 'node:stream';
import { NdjsonBridge } from '../dist/bridge.js';

const status = { kind: 'project_status', data: { generation: 7, open: false } };

test('tool and run identities survive a production bridge round trip', { timeout: 2000 }, async () => {
  const outgoing = new PassThrough();
  const incoming = new PassThrough();
  let observed;
  const caller = new NdjsonBridge(incoming, outgoing, { generation: 7, projectId: 'project-1' });
  const peer = new NdjsonBridge(outgoing, incoming, {
    generation: 7,
    projectId: 'project-1',
    handleRequest: async request => { observed = request; return status; },
  });
  caller.start();
  peer.start();
  try {
    const reply = await caller.request('project_status', {}, { runId: 'run-1', toolCallId: 'tool-1' });
    assert.equal(observed.runId, 'run-1');
    assert.equal(observed.toolCallId, 'tool-1');
    assert.equal(reply.toolCallId, 'tool-1');
    assert.equal(reply.ok, true);
    assert.deepEqual(reply.data, status);
  } finally {
    caller.close(); peer.close(); outgoing.destroy(); incoming.destroy();
  }
});

test('a reply for another tool cannot settle the requested tool successfully', { timeout: 2000 }, async () => {
  const incoming = new PassThrough();
  const outgoing = new PassThrough();
  const bridge = new NdjsonBridge(incoming, outgoing, { generation: 7, projectId: 'project-1' });
  outgoing.on('data', chunk => {
    const request = JSON.parse(chunk.toString());
    incoming.write(JSON.stringify({ v: request.v, id: request.id, projectId: request.projectId, generation: request.generation, runId: request.runId, toolCallId: 'wrong-tool', kind: 'response', ok: true, data: status }) + '\n');
  });
  bridge.start();
  try {
    await assert.rejects(bridge.request('project_status', {}, { runId: 'run-1', toolCallId: 'tool-1' }), error => error.code === 'STALE_SESSION');
  } finally {
    bridge.close(); incoming.destroy(); outgoing.destroy();
  }
});
