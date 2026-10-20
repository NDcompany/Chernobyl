/**
 * Native response validation (G4.2). Proven necessary by the OpenCode
 * probe: unknown REST paths answered `text/html` with HTTP 200, so status
 * codes alone prove nothing. Every adapter runs its native responses
 * through this before trusting the body.
 */
import { AdapterError } from './adapter';

export function validateJsonResponse(contentType: string | undefined, body: string): unknown {
  if (!contentType || !contentType.toLowerCase().includes('application/json')) {
    throw new AdapterError(
      'invalid-response',
      `expected application/json, got ${contentType ?? '(no content-type)'}`,
    );
  }
  try {
    return JSON.parse(body);
  } catch {
    throw new AdapterError('invalid-response', 'response body is not valid JSON');
  }
}
