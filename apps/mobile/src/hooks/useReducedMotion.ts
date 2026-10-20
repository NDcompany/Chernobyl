import { useEffect, useState } from 'react';
import { AccessibilityInfo, Platform } from 'react-native';
import { useAppStore } from '@/store/appStore';

/**
 * Reduced-motion awareness: follows the OS setting, or forces off when the
 * user enables "Reduce motion" in appearance settings.
 */
export function useReducedMotion(): boolean {
  const forced = useAppStore((s) => s.settings.reduceMotion === 'on');
  const [system, setSystem] = useState(() => {
    if (Platform.OS === 'web' && typeof window !== 'undefined' && window.matchMedia) {
      return window.matchMedia('(prefers-reduced-motion: reduce)').matches;
    }
    return false;
  });

  useEffect(() => {
    if (Platform.OS === 'web' && typeof window !== 'undefined' && window.matchMedia) {
      const mq = window.matchMedia('(prefers-reduced-motion: reduce)');
      const onChange = () => setSystem(mq.matches);
      mq.addEventListener('change', onChange);
      return () => mq.removeEventListener('change', onChange);
    }
    let mounted = true;
    AccessibilityInfo.isReduceMotionEnabled()
      .then((enabled) => {
        if (mounted) setSystem(enabled);
      })
      .catch(() => undefined);
    const sub = AccessibilityInfo.addEventListener('reduceMotionChanged', setSystem);
    return () => {
      mounted = false;
      sub.remove();
    };
  }, []);

  return forced || system;
}

/** Re-renders on an interval — used for expiry countdowns and stale checks. */
export function useNow(intervalMs = 1000): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), intervalMs);
    return () => clearInterval(id);
  }, [intervalMs]);
  return now;
}
