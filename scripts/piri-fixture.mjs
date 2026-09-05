// Run inside an installed Piri checkout; no model request or credentials.
// PIRI_SESSION_MANAGER points to its built session-manager.js.
import { pathToFileURL } from 'node:url';
import { readFileSync, writeFileSync } from 'node:fs';
import assert from 'node:assert/strict';
const { SessionManager } = await import(pathToFileURL(process.env.PIRI_SESSION_MANAGER));
const [mode, path] = process.argv.slice(2);
if (mode === 'generate') {
  const manager = SessionManager.create('/fixture/repo', '/tmp/piri-fixtures');
  const usage = { input: 10, output: 5, cacheRead: 0, cacheWrite: 0, totalTokens: 15,
    cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } };
  manager.appendMessage({ role: 'user', content: 'Read hello.txt', timestamp: Date.now() });
  manager.appendMessage({ role: 'assistant', api: 'anthropic-messages', provider: 'anthropic', model: 'fixture-model', usage,
    stopReason: 'toolUse', timestamp: Date.now(), content: [{ type: 'toolCall', id: 'fixture_call', name: 'read', arguments: { path: 'hello.txt' } }] });
  manager.appendMessage({ role: 'toolResult', toolCallId: 'fixture_call', toolName: 'read', content: [{ type: 'text', text: 'hello' }], isError: false, timestamp: Date.now() });
  manager.appendMessage({ role: 'assistant', api: 'anthropic-messages', provider: 'anthropic', model: 'fixture-model', usage,
    stopReason: 'stop', timestamp: Date.now(), content: [{ type: 'text', text: 'The file says hello.' }] });
  writeFileSync(path, readFileSync(manager.getSessionFile()));
} else if (mode === 'verify') {
  const manager = SessionManager.open(path);
  assert.equal(manager.getHeader().version, 3);
  const messages = manager.buildSessionContext().messages;
  assert.ok(messages.some(m => m.role === 'user'));
  assert.ok(messages.some(m => m.role === 'assistant' && m.content.some(b => b.type === 'toolCall')));
  assert.ok(messages.some(m => m.role === 'toolResult'));
  assert.ok(messages.some(m => m.role === 'assistant' && m.content.some(b => b.type === 'text')));
  console.log(`Piri opened v3 session: ${messages.length} messages`);
} else throw Error('expected generate|verify FILE');
