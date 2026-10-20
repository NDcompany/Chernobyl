import { runAdapterConformance, runSafetyConformance } from './conformance';
import { createMockAdapter } from './mockAdapter';

// One adapter, two suites: the base contract every adapter must satisfy and
// the G4 safety rules driven through the fault-injection knobs.
runAdapterConformance('mock-adapter', (opts) => createMockAdapter(opts));
runSafetyConformance('mock-adapter', (opts) => createMockAdapter(opts));
