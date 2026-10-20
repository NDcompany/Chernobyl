/**
 * Secrets live in the OS keystore/keychain (expo-secure-store) — never in
 * the SQLite database or in plain state.
 */
import * as SecureStore from 'expo-secure-store';
import { Platform } from 'react-native';

const TOKEN_KEY = 'chernobyl_host_token';
/** web has no secure store; keep it in memory for the dev session only */
let webToken: string | null = null;

export async function saveHostToken(token: string): Promise<void> {
  if (Platform.OS === 'web') {
    webToken = token;
    return;
  }
  try {
    await SecureStore.setItemAsync(TOKEN_KEY, token, {
      keychainAccessible: SecureStore.WHEN_UNLOCKED,
    });
  } catch {
    webToken = token;
  }
}

export async function readHostToken(): Promise<string | null> {
  if (Platform.OS === 'web') return webToken;
  try {
    return await SecureStore.getItemAsync(TOKEN_KEY);
  } catch {
    return null;
  }
}

export async function clearHostToken(): Promise<void> {
  if (Platform.OS === 'web') {
    webToken = null;
    return;
  }
  try {
    await SecureStore.deleteItemAsync(TOKEN_KEY);
  } catch {
    /* nothing to clear */
  }
}
