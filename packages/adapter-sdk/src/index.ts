/**
 * @chernobyl/adapter-sdk — the adapter contract, native response
 * validation, the MockAdapter reference implementation, and the
 * conformance suites (base contract + G4 safety rules) that every real
 * adapter must pass at its phase gate.
 */
export {
  AdapterError,
  type AdapterErrorCode,
  type AdapterFaultOptions,
  type AdapterHealth,
  type AgentAdapter,
} from './adapter';
export { validateJsonResponse } from './response';
export { createMockAdapter } from './mockAdapter';
export { runAdapterConformance, runSafetyConformance, type AdapterFactory } from './conformance';
