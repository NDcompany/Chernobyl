/**
 * Secrets live in the OS keystore/keychain (expo-secure-store) — never in
 * the SQLite database or in plain state.
 */
import * as SecureStore from 'expo-secure-store';
import { Platform } from 'react-native';

const TOKEN_KEY = 'chernobyl_host_token';
const TOKEN_HOST_KEY = 'chernobyl_host_token_host';
/** web has no secure store (and a failed keystore write): memory, for the session only */
const webSecrets = new Map<string, string>();

/**
 * `host` is the pairing host id the token was entered for (null: no pairing yet);
 * saving a pairing for another host drops the token (`keepTokenOnPair`).
 */
export async function saveHostToken(token: string, host: string | null): Promise<void> {
  await saveSecret(TOKEN_KEY, token);
  if (host) await saveSecret(TOKEN_HOST_KEY, host);
  else await deleteSecret(TOKEN_HOST_KEY);
}

/** The host id a stored token is bound to; null when unbound. */
export async function readHostTokenHost(): Promise<string | null> {
  return readSecret(TOKEN_HOST_KEY);
}

export async function readHostToken(): Promise<string | null> {
  return readSecret(TOKEN_KEY);
}

export async function clearHostToken(): Promise<void> {
  await deleteSecret(TOKEN_KEY);
  await deleteSecret(TOKEN_HOST_KEY);
}

async function saveSecret(key: string, value: string): Promise<void> {
  if (Platform.OS !== 'web') {
    try {
      await SecureStore.setItemAsync(key, value, { keychainAccessible: SecureStore.WHEN_UNLOCKED });
      return;
    } catch {
      /* fall back to memory */
    }
  }
  webSecrets.set(key, value);
}

async function readSecret(key: string): Promise<string | null> {
  if (Platform.OS === 'web') return webSecrets.get(key) ?? null;
  try {
    return (await SecureStore.getItemAsync(key)) ?? webSecrets.get(key) ?? null;
  } catch {
    return null;
  }
}

async function deleteSecret(key: string): Promise<void> {
  webSecrets.delete(key);
  if (Platform.OS === 'web') return;
  try {
    await SecureStore.deleteItemAsync(key);
  } catch {
    /* nothing to clear */
  }
}

/**
 * The pairing payload (QR JSON) IS the credential — it carries the PSK, so it
 * lives in the keystore exactly like the host token, never in SQLite or plain
 * state (ADR-002/012). Stored as the raw scanned/pasted text (its `psk` later
 * replaced by the rotated device key, see rotatePairingPsk); validated by
 * parsePairingPayload on every read.
 */
const PAIRING_KEY = 'chernobyl_pairing';
let webPairing: string | null = null;

export async function savePairing(payloadJson: string): Promise<void> {
  if (Platform.OS === 'web') {
    webPairing = payloadJson;
    return;
  }
  try {
    await SecureStore.setItemAsync(PAIRING_KEY, payloadJson, {
      keychainAccessible: SecureStore.WHEN_UNLOCKED,
    });
  } catch {
    webPairing = payloadJson;
  }
}

export async function readPairing(): Promise<string | null> {
  if (Platform.OS === 'web') return webPairing;
  try {
    return await SecureStore.getItemAsync(PAIRING_KEY);
  } catch {
    return null;
  }
}

/**
 * Swaps the stored pairing's PSK for the device key the companion rotated it
 * to at first use (ADR-011 addendum); the rest of the payload is kept. Rejects
 * when the stored pairing is no longer this device's (re-paired meanwhile) or
 * the keystore write fails: the transport then withholds its ack and keeps the
 * old key, so a failed write can never lock the phone out. Unlike savePairing
 * there is no in-memory fallback, since the ack must mean "stored".
 */
export async function rotatePairingPsk(deviceId: string, pskB64: string): Promise<void> {
  const payload = JSON.parse((await readPairing()) ?? 'null') as Record<string, unknown> | null;
  if (payload?.dev !== deviceId) throw new Error('stored pairing changed');
  const rotated = JSON.stringify({ ...payload, psk: pskB64 });
  if (Platform.OS === 'web') {
    webPairing = rotated;
    return;
  }
  await SecureStore.setItemAsync(PAIRING_KEY, rotated, {
    keychainAccessible: SecureStore.WHEN_UNLOCKED,
  });
}

export async function clearPairing(): Promise<void> {
  if (Platform.OS === 'web') {
    webPairing = null;
    return;
  }
  try {
    await SecureStore.deleteItemAsync(PAIRING_KEY);
  } catch {
    /* nothing to clear */
  }
}
