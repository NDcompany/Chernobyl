/**
 * Android foreground service for the live connection (ADR-011 §4): while it
 * runs, the process (and with it the E2E socket and JS timers) stays alive with
 * the app in the background. Native side: modules/connection-service.
 * Web, iOS and Expo Go have no native module, so every call is a no-op.
 */
import { requireOptionalNativeModule } from 'expo';
import { AppRegistry, Platform } from 'react-native';

interface ConnectionServiceModule {
  start(title: string, text: string): void;
  update(text: string): void;
  stop(): void;
}

const native = requireOptionalNativeModule<ConnectionServiceModule>('ConnectionService');

// Module scope runs once; the root layout imports this file so the task exists
// before any start. The service runs it because RN pauses JS timers in the
// background unless a headless task is active. It never settles: the service
// finishes it natively when it stops.
if (Platform.OS === 'android') {
  AppRegistry.registerHeadlessTask('ChernobylConnection', () => async () => {
    await new Promise<never>(() => {});
  });
}

/** Never throws: these run inside the transport's status callbacks. */
function call(fn: (m: ConnectionServiceModule) => void): void {
  if (!native) return;
  try {
    fn(native);
  } catch (e) {
    console.warn('[connectionService]', e);
  }
}

/** Only while the app is in the foreground: Android 12+ refuses a background start. */
export const start = (title: string, text: string): void => call((m) => m.start(title, text));
export const update = (text: string): void => call((m) => m.update(text));
export const stop = (): void => call((m) => m.stop());
