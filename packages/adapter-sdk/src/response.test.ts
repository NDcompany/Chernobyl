import assert from 'node:assert/strict';
import test from 'node:test';
import { AdapterError } from './adapter';
import { validateJsonResponse } from './response';

test('validateJsonResponse: parses valid JSON bodies with application/json header', () => {
  const obj = validateJsonResponse('application/json', '{"ok":true,"count":42}');
  assert.deepEqual(obj, { ok: true, count: 42 });

  const arr = validateJsonResponse('application/json; charset=utf-8', '[1,2,3]');
  assert.deepEqual(arr, [1, 2, 3]);

  const upper = validateJsonResponse('APPLICATION/JSON', '"string-value"');
  assert.equal(upper, 'string-value');
});

test('validateJsonResponse: throws AdapterError when Content-Type is missing or non-JSON', () => {
  assert.throws(
    () => validateJsonResponse(undefined, '{"ok":true}'),
    (err: unknown) => err instanceof AdapterError && err.code === 'invalid-response',
  );

  assert.throws(
    () => validateJsonResponse('text/html', '<html>200 OK</html>'),
    (err: unknown) => err instanceof AdapterError && err.code === 'invalid-response',
  );

  assert.throws(
    () => validateJsonResponse('text/plain', 'ok'),
    (err: unknown) => err instanceof AdapterError && err.code === 'invalid-response',
  );
});

test('validateJsonResponse: throws AdapterError when body is invalid JSON', () => {
  assert.throws(
    () => validateJsonResponse('application/json', '{invalid json'),
    (err: unknown) => err instanceof AdapterError && err.code === 'invalid-response',
  );

  assert.throws(
    () => validateJsonResponse('application/json', ''),
    (err: unknown) => err instanceof AdapterError && err.code === 'invalid-response',
  );
});
