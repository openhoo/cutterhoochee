import assert from 'node:assert/strict';
import test from 'node:test';
import { ProvidersRuntime } from '../dist/providers.js';

function fixture(onAccountChanged = async () => {}) {
  const prompts = [];
  let currentRun;
  const credentials = {
    setRunId(id) { currentRun = id; },
    async accountId() { return 'generated-account'; },
  };
  const models = {
    async login(_provider, _type, interaction) {
      await interaction.prompt({ type: 'secret', message: 'Generated test prompt' });
    },
  };
  const runtime = new ProvidersRuntime(models, credentials, {
    emit(event, data) { if (event === 'providers_auth_prompt') prompts.push(data); },
  }, '/unused-cutterhoochee-test-selection.json', onAccountChanged);
  return { runtime, prompts, currentRun: () => currentRun };
}

const login = { action: 'login', providerId: 'openai', authType: 'api_key', sessionOnly: true };

test('cancelled authentication rejects its prompt and permits a fresh login', { timeout: 2000 }, async () => {
  const f = fixture();
  const oldLogin = f.runtime.handle(login, undefined, 'auth-old');
  const oldRejection = assert.rejects(oldLogin, error => error.code === 'JOB_CANCELLED');
  const oldPrompt = f.prompts.at(-1);
  assert.ok(oldPrompt);
  await f.runtime.cancelActiveAuth();
  await oldRejection;
  assert.equal(f.currentRun(), undefined);
  await assert.rejects(f.runtime.handle({ action: 'answer', promptId: oldPrompt.promptId, value: 'stale' }, undefined, 'auth-old'), error => error.code === 'STALE_SESSION');

  const freshLogin = f.runtime.handle(login, undefined, 'auth-new');
  const freshPrompt = f.prompts.at(-1);
  assert.notEqual(freshPrompt.promptId, oldPrompt.promptId);
  const answer = await f.runtime.handle({ action: 'answer', promptId: freshPrompt.promptId, value: 'generated-test-value' }, undefined, 'auth-new');
  assert.equal(answer.accepted, true);
  assert.deepEqual(await freshLogin, { action: 'login', providerId: 'openai', type: 'api_key', configured: true });
  assert.equal(f.currentRun(), undefined);
});

test('an already-aborted login emits no prompt and does not block the next login', { timeout: 2000 }, async () => {
  const f = fixture();
  await assert.rejects(f.runtime.handle(login, AbortSignal.abort(), 'auth-aborted'), error => error.code === 'JOB_CANCELLED');
  assert.equal(f.prompts.length, 0);
  assert.equal(f.currentRun(), undefined);
  const next = f.runtime.handle(login, undefined, 'auth-next');
  const nextRejection = assert.rejects(next, error => error.code === 'JOB_CANCELLED');
  assert.equal(f.prompts.length, 1);
  await f.runtime.cancelActiveAuth();
  await nextRejection;
});

test('account-change session cancellation cannot deadlock a successful login', { timeout: 2000 }, async () => {
  let f;
  let accountChanged = false;
  f = fixture(async () => {
    await f.runtime.cancelActiveAuth();
    accountChanged = true;
  });
  const pending = f.runtime.handle(login, undefined, 'auth-complete');
  await f.runtime.handle({ action: 'answer', promptId: f.prompts.at(-1).promptId, value: 'generated-test-value' }, undefined, 'auth-complete');
  assert.equal((await pending).configured, true);
  assert.equal(accountChanged, true);
  assert.equal(f.currentRun(), undefined);
});
