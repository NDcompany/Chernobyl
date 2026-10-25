/**
 * Installs `crypto.getRandomValues` where the JS runtime lacks it (Hermes on
 * Android/iOS). The E2E handshake draws its 16-byte client nonce through
 * @noble/hashes `randomBytes`, which uses that global and throws without it —
 * every phone handshake would fail (security review 2026-10-25). expo-crypto
 * reads the platform CSPRNG (SecureRandom / SecRandomCopyBytes). Imported first
 * from the root layout so it runs before any transport is built.
 */
import { getRandomValues } from 'expo-crypto';

type CryptoLike = { getRandomValues?: (a: Uint8Array) => Uint8Array };
const g = globalThis as { crypto?: CryptoLike };
if (typeof g.crypto?.getRandomValues !== 'function') {
  g.crypto = { ...(g.crypto ?? {}), getRandomValues: (a: Uint8Array) => getRandomValues(a) };
}
