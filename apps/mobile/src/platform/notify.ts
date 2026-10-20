/**
 * Local notifications (permission requests, finished sessions).
 * Remote push would additionally require googleServicesFile/APNs config —
 * that wiring is intentionally left for the real host rollout.
 */
import {
  AndroidImportance,
  getPermissionsAsync,
  requestPermissionsAsync,
  scheduleNotificationAsync,
  setNotificationChannelAsync,
  setNotificationHandler,
} from 'expo-notifications';
import { Platform } from 'react-native';

let initialized = false;

export async function initNotifications(): Promise<boolean> {
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
    if (!current.granted) {
      const asked = await requestPermissionsAsync();
      initialized = asked.granted;
    } else {
      initialized = true;
    }
  } catch {
    initialized = false;
  }
  return initialized;
}

/** Fires only when permission was granted and the user enabled the setting. */
export async function notify(title: string, body: string): Promise<void> {
  if (Platform.OS === 'web' || !initialized) return;
  try {
    await scheduleNotificationAsync({
      content: { title, body, sound: false, autoDismiss: true },
      trigger: null,
    });
  } catch {
    /* notifications unavailable — silently skip */
  }
}
