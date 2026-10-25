/**
 * Local notifications (permission requests, finished sessions).
 * Remote push would additionally require googleServicesFile/APNs config —
 * that wiring is intentionally left for the real host rollout.
 */
import {
  AndroidImportance,
  addNotificationResponseReceivedListener,
  clearLastNotificationResponse,
  getLastNotificationResponse,
  getPermissionsAsync,
  requestPermissionsAsync,
  scheduleNotificationAsync,
  setNotificationChannelAsync,
  setNotificationHandler,
  type NotificationResponse,
} from 'expo-notifications';
import { Platform } from 'react-native';

/** The only routes a notification may open (ours are local notifications; anything else is ignored). */
const ROUTE_RE = /^\/(session|permission)\/[^/?#]+(\?focus=[^&#]+)?$/;

let initialized = false;

/**
 * Resolves whether notify() can post. `ask: false` never shows the permission
 * dialog: it re-reads a grant the user may have given in system settings.
 */
export async function initNotifications(ask = true): Promise<boolean> {
  if (Platform.OS === 'web' || initialized) return initialized;
  try {
    setNotificationHandler({
      handleNotification: async () => ({
        shouldShowBanner: true,
        shouldShowList: true,
        shouldPlaySound: false,
        shouldSetBadge: false,
      }),
    });
    if (Platform.OS === 'android') {
      await setNotificationChannelAsync('default', {
        name: 'CHERNOBYL',
        importance: AndroidImportance.DEFAULT,
        lightColor: '#050505',
        vibrationPattern: [0, 120, 80, 120],
      });
    }
    const current = await getPermissionsAsync();
    if (!current.granted && ask) {
      const asked = await requestPermissionsAsync();
      initialized = asked.granted;
    } else {
      initialized = current.granted;
    }
  } catch {
    initialized = false;
  }
  return initialized;
}

/**
 * Fires only when permission was granted and the user enabled the setting.
 * `url` is the in-app route a tap opens (see onNotificationOpen).
 */
export async function notify(title: string, body: string, url?: string): Promise<void> {
  if (Platform.OS === 'web' || !initialized) return;
  try {
    await scheduleNotificationAsync({
      content: { title, body, sound: false, autoDismiss: true, data: url ? { url } : {} },
      trigger: null,
    });
  } catch {
    /* notifications unavailable — silently skip */
  }
}

/** Calls `open` with the route of each tapped notification, including the one that launched the app. */
export function onNotificationOpen(open: (url: string) => void): () => void {
  if (Platform.OS === 'web') return () => undefined;
  const handle = (response: NotificationResponse | null) => {
    const url = response?.notification.request.content.data?.url;
    if (typeof url === 'string' && ROUTE_RE.test(url)) open(url);
  };
  let sub: { remove(): void } | undefined;
  try {
    sub = addNotificationResponseReceivedListener(handle);
    const launch = getLastNotificationResponse();
    clearLastNotificationResponse();
    handle(launch);
  } catch {
    /* notifications unavailable, or the navigator was not ready */
  }
  return () => sub?.remove();
}
