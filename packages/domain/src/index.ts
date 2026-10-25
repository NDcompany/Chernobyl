/**
 * @chernobyl/domain — canonical state machines and models shared by the
 * protocol engine, the mobile client and the adapter SDK.
 *
 * Layering: this package is dependency-free (no imports at runtime), so
 * @chernobyl/protocol, apps/mobile and packages/adapter-sdk can all
 * consume it without creating a cycle.
 */

export {
  TERMINAL_STATES,
  canTransition,
  isTerminal,
  transitionCommand,
  type CommandState,
} from './command';

export {
  PERMISSION_DECISION_ERRORS,
  evaluatePermissionDecision,
  legacyDecisions,
  type PermissionDecisionCode,
  type PermissionDecisionErrorCode,
  type PermissionDecisionInput,
  type PermissionDecisionOption,
  type PermissionDecisionQuestion,
  type PermissionDecisionRequest,
  type PermissionVerdict,
} from './permission';

export {
  applyCommandLifecycle,
  type CommandEntry,
  type CommandTracker,
  type LifecycleEvent,
} from './tracker';

export { isUsable, isValidCapability, type Capability, type CapabilityStatus } from './capability';
