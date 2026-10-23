/**
 * One error type for the whole client-side E2E surface, with a stable `code`
 * so transports and UIs branch on the class of failure, not message text.
 */
export type E2eErrorCode =
  | 'handshake-rejected' // server said hs_err (unknown/expired/spent/revoked device)
  | 'handshake-timeout' // no hs_challenge/hs_ok within the deadline
  | 'server-proof-mismatch' // server failed mutual auth — possible MITM
  | 'malformed' // not JSON / missing or badly typed fields
  | 'replay' // counter already seen
  | 'desync' // counter skipped
  | 'aead' // authentication tag failed (tamper or wrong key)
  | 'bad-utf8'; // sealed body is not valid UTF-8

export class E2eError extends Error {
  readonly code: E2eErrorCode;

  constructor(code: E2eErrorCode, message: string) {
    super(message);
    this.name = 'E2eError';
    this.code = code;
  }
}
