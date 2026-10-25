/**
 * Client side of the frozen CHERNOBYL E2E wire contract (ADR-011/012).
 * See services/companion-rs/README.md §"E2E wire format" for the contract and
 * services/companion-rs/src/e2e.rs for the byte-exact server reference.
 */
export { b64decode, b64encode } from './base64.js';
export { E2eError, type E2eErrorCode } from './errors.js';
export { handshakeProof, performHandshake, proofOk, type HandshakeIo } from './handshake.js';
export { PairingError, parsePairingPayload, type PairingCredential } from './pairing.js';
export { SealedSession, deriveKeys, type SessionKeys } from './session.js';
export {
  APP_ORIGIN,
  createE2eTransport,
  retryDelayMs,
  type Ack,
  type E2eTransport,
  type E2eTransportOptions,
  type SocketLike,
  type TransportStatus,
} from './transport.js';
export { utf8Decode, utf8Encode } from './utf8.js';
