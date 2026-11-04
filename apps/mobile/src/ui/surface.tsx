/**
 * Screen-level surfaces: layout shell, page header, card, empty state,
 * toast host and confirmation modal.
 */
import type { ReactNode, RefObject } from 'react';
import {
  KeyboardAvoidingView,
  Modal,
  Pressable,
  ScrollView,
  StyleSheet,
  View,
} from 'react-native';
import type { ScrollViewInstance, StyleProp, ViewStyle } from 'react-native';
import { SafeAreaView, useSafeAreaInsets } from 'react-native-safe-area-context';
import { useRouter, useSegments } from 'expo-router';
import { colors, spacing, radii, shadow, TAB_BAR_HEIGHT } from '@/theme/tokens';
import { Button, Text } from '@/ui/primitives';
import { Contours, type ContourVariant } from '@/ui/graphics';
import { Icon, type IconName } from '@/ui/icons';
import { useAppStore } from '@/store/appStore';
import { transportKind } from '@/data/transport';
import { useSnapshot } from '@/data/state';
import { hasSimulatedData } from '@/data/selectors';

/* ------------------------------------------------------------------ */
/* Screen                                                              */
/* ------------------------------------------------------------------ */

export interface ScreenProps {
  children: ReactNode;
  /** fixed element rendered above the scrollable body (e.g. a dark header) */
  header?: ReactNode;
  /** pinned element rendered below the body (e.g. a composer) */
  footer?: ReactNode;
  scroll?: boolean;
  padded?: boolean;
  background?: string;
  /** include the top safe-area inset (false when the screen draws its own dark header) */
  safeTop?: boolean;
  contentStyle?: StyleProp<ViewStyle>;
  /** extra bottom space, e.g. tab-bar clearance */
  bottomInset?: number;
  /** caller-managed ref for auto-scrolling the body (e.g. live output) */
  scrollRef?: RefObject<ScrollViewInstance | null>;
}

export function Screen({
  children,
  header,
  footer,
  scroll = true,
  padded = true,
  background = colors.white,
  safeTop = true,
  contentStyle,
  bottomInset = 0,
  scrollRef,
}: ScreenProps) {
  const insets = useSafeAreaInsets();
  const horizontal = padded ? spacing.xl : 0;
  const body = scroll ? (
    <ScrollView
      ref={scrollRef}
      style={{ flex: 1 }}
      contentContainerStyle={[
        {
          paddingHorizontal: horizontal,
          paddingTop: header ? spacing.lg : spacing.xl,
          paddingBottom: spacing.xxxl + bottomInset + insets.bottom,
          gap: spacing.xl,
        },
        contentStyle,
      ]}
      keyboardShouldPersistTaps="handled"
      showsVerticalScrollIndicator={false}
    >
      {children}
    </ScrollView>
  ) : (
    <View style={[{ flex: 1, paddingHorizontal: horizontal }, contentStyle]}>{children}</View>
  );

  return (
    <SafeAreaView
      edges={safeTop ? ['top'] : []}
      style={{ flex: 1, backgroundColor: background }}
    >
      {header}
      {/* Android too: edge-to-edge (edgeToEdgeEnabled) means adjustResize no longer shrinks the window */}
      <KeyboardAvoidingView style={{ flex: 1 }} behavior="padding">
        {body}
        {footer ? (
          <View
            style={{
              paddingHorizontal: horizontal,
              paddingTop: spacing.md,
              paddingBottom: spacing.md + insets.bottom,
              backgroundColor: background,
            }}
          >
            {footer}
          </View>
        ) : null}
      </KeyboardAvoidingView>
    </SafeAreaView>
  );
}

/* ------------------------------------------------------------------ */
/* Page header                                                         */
/* ------------------------------------------------------------------ */

export interface PageHeaderProps {
  title?: string;
  subtitle?: string;
  /** undefined → router.back(); null → no back button */
  onBack?: (() => void) | null;
  right?: ReactNode;
  dark?: boolean;
  bordered?: boolean;
}

export function PageHeader({ title, subtitle, onBack, right, dark = false, bordered = true }: PageHeaderProps) {
  const router = useRouter();
  const showBack = onBack !== null;
  const handleBack = onBack ?? (() => router.back());
  const fg = dark ? colors.white : colors.black;
  const fg2 = dark ? 'rgba(255,255,255,0.72)' : colors.text2;

  return (
    <View
      style={[
        styles.header,
        bordered && !dark && { borderBottomWidth: StyleSheet.hairlineWidth, borderBottomColor: colors.border },
      ]}
    >
      {showBack ? (
        <Pressable
          onPress={handleBack}
          hitSlop={10}
          focusable
          accessible
          accessibilityRole="button"
          accessibilityLabel="Go back"
          style={({ pressed }) => [
            styles.headerBtn,
            !dark && { borderWidth: 1, borderColor: colors.border },
            pressed && { opacity: 0.6 },
          ]}
        >
          <Icon name="chevron-left" size={20} color={fg} />
        </Pressable>
      ) : (
        <View style={styles.headerBtn} />
      )}
      <View style={{ flex: 1, alignItems: 'center' }}>
        {title ? (
          <Text variant="h3" color={fg} numberOfLines={1} accessibilityRole="header">
            {title}
          </Text>
        ) : null}
        {subtitle ? (
          <Text variant="xs" color={fg2} numberOfLines={1}>
            {subtitle}
          </Text>
        ) : null}
      </View>
      <View style={styles.headerBtn}>{right}</View>
    </View>
  );
}

/* ------------------------------------------------------------------ */
/* Card                                                                */
/* ------------------------------------------------------------------ */

export interface CardProps {
  children: ReactNode;
  onPress?: () => void;
  /** thin black outline instead of the default grey hairline */
  strong?: boolean;
  style?: StyleProp<ViewStyle>;
  accessibilityLabel?: string;
}

export function Card({ children, onPress, strong = false, style, accessibilityLabel }: CardProps) {
  const base = [styles.card, strong && styles.cardStrong, style];
  if (!onPress) {
    return <View style={base}>{children}</View>;
  }
  return (
    <Pressable
      onPress={onPress}
      focusable
      accessible
      accessibilityRole="button"
      accessibilityLabel={accessibilityLabel}
      style={({ pressed }) => [base, pressed && { opacity: 0.85 }]}
    >
      {children}
    </Pressable>
  );
}

/* ------------------------------------------------------------------ */
/* Empty state                                                         */
/* ------------------------------------------------------------------ */

export interface EmptyStateProps {
  icon?: IconName;
  title: string;
  caption?: string;
  contour?: ContourVariant | false;
  actionLabel?: string;
  onAction?: () => void;
  style?: StyleProp<ViewStyle>;
}

export function EmptyState({
  icon = 'search',
  title,
  caption,
  contour = 'bottom',
  actionLabel,
  onAction,
  style,
}: EmptyStateProps) {
  return (
    <View style={[styles.empty, style]}>
      {contour ? <Contours variant={contour} style={{ opacity: 0.5 }} /> : null}
      <View
        style={{
          width: 60,
          height: 60,
          borderRadius: 30,
          borderWidth: 1,
          borderColor: colors.black,
          alignItems: 'center',
          justifyContent: 'center',
          backgroundColor: colors.white,
        }}
      >
        <Icon name={icon} size={24} color={colors.black} />
      </View>
      <Text variant="h3" center style={{ marginTop: spacing.lg }}>
        {title}
      </Text>
      {caption ? (
        <Text variant="sm" color={colors.text2} center style={{ marginTop: spacing.xs, maxWidth: 300 }}>
          {caption}
        </Text>
      ) : null}
      {actionLabel && onAction ? (
        <Button
          label={actionLabel}
          onPress={onAction}
          variant="secondary"
          size="md"
          style={{ marginTop: spacing.xl, alignSelf: 'stretch' }}
        />
      ) : null}
    </View>
  );
}

/* ------------------------------------------------------------------ */
/* Toasts                                                              */
/* ------------------------------------------------------------------ */

export function ToastHost() {
  const toasts = useAppStore((s) => s.toasts);
  const dismiss = useAppStore((s) => s.dismissToast);
  const insets = useSafeAreaInsets();
  const segments = useSegments();
  const onTabs = segments[0] === '(tabs)';
  if (toasts.length === 0) return null;

  const bottom = insets.bottom + (onTabs ? TAB_BAR_HEIGHT + spacing.md : spacing.lg);

  return (
    <View
      pointerEvents="box-none"
      style={[StyleSheet.absoluteFill, { justifyContent: 'flex-end', paddingHorizontal: spacing.lg, paddingBottom: bottom, zIndex: 50 }]}
    >
      {toasts.map((t) => (
        <Pressable
          key={t.id}
          onPress={() => dismiss(t.id)}
          accessibilityRole="alert"
          style={[
            styles.toast,
            shadow.lift,
            t.tone === 'error' && { borderLeftWidth: 4, borderLeftColor: colors.error },
            t.tone === 'success' && { borderLeftWidth: 4, borderLeftColor: colors.success },
            t.tone === 'warning' && { borderLeftWidth: 4, borderLeftColor: colors.warning },
          ]}
        >
          <Text variant="smStrong" color={colors.white} style={{ flex: 1 }} numberOfLines={3}>
            {t.message}
          </Text>
          <Icon name="x" size={14} color={colors.text3} />
        </Pressable>
      ))}
    </View>
  );
}

/* ------------------------------------------------------------------ */
/* Confirmation modal                                                  */
/* ------------------------------------------------------------------ */

export interface ConfirmModalProps {
  visible: boolean;
  title: string;
  message: string;
  /** scope / safety explanation shown under the message */
  note?: string;
  confirmLabel?: string;
  cancelLabel?: string;
  busy?: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}

export function ConfirmModal({
  visible,
  title,
  message,
  note,
  confirmLabel = 'Confirm',
  cancelLabel = 'Cancel',
  busy = false,
  onConfirm,
  onCancel,
}: ConfirmModalProps) {
  return (
    <Modal visible={visible} transparent animationType="fade" onRequestClose={onCancel} statusBarTranslucent>
      <Pressable style={styles.overlay} onPress={busy ? undefined : onCancel} accessibilityLabel="Dismiss dialog">
        <Pressable style={[styles.dialog, shadow.lift]} onPress={() => undefined} accessibilityViewIsModal>
          <Text variant="h2">{title}</Text>
          <Text variant="body" color={colors.text2}>
            {message}
          </Text>
          {note ? (
            <View style={styles.note}>
              <Icon name="eye" size={16} color={colors.text2} />
              <Text variant="xs" color={colors.text2} style={{ flex: 1 }}>
                {note}
              </Text>
            </View>
          ) : null}
          <View style={{ gap: spacing.sm, marginTop: spacing.sm }}>
            <Button label={confirmLabel} onPress={onConfirm} loading={busy} />
            <Button label={cancelLabel} onPress={onCancel} variant="secondary" disabled={busy} />
          </View>
        </Pressable>
      </Pressable>
    </Modal>
  );
}

/* ------------------------------------------------------------------ */
/* Styles                                                              */
/* ------------------------------------------------------------------ */

/* ------------------------------------------------------------------ */
/* Simulation badge (ADR-009)                                          */
/*                                                                     */
/* Persistent, visible "Simulation" indication wherever state or       */
/* approvals are shown. Hidden on onboarding (the demo is the point).  */
/* Shown while the app's own mock transport is active OR while the     */
/* snapshot carries any `simulated` host/agent — a real companion      */
/* serving its demo adapter is still a simulation (provenance travels  */
/* with the data). The transport is a render-time read (set at boot);  */
/* the snapshot subscription keeps the provenance check current.       */
/* ------------------------------------------------------------------ */

const SIMULATION_PATHS = new Set([
  'sign-in',
  'connect-host',
  'settings/[section]',
  'new-task',
]);

export function SimulationBadge() {
  const segments = useSegments();
  // Re-render when the transport lifecycle changes: pairing swaps mock →
  // live mid-session. Over a real connection the badge shows only while the
  // data itself is marked simulated (ADR-009).
  useAppStore((s) => s.connection);
  const simulatedData = hasSimulatedData(useSnapshot());
  // below the status bar, not over the clock (seen on the emulator)
  const insetTop = useSafeAreaInsets().top;
  const top = segments[segments.length - 1];
  if (typeof top === 'string' && SIMULATION_PATHS.has(top)) return null;
  if (transportKind() !== 'mock' && !simulatedData) return null;
  return (
    <View
      pointerEvents="none"
      style={[styles.simBadge, { top: insetTop + spacing.xs }]}
      accessibilityRole="text"
      accessibilityLabel="Simulation mode: showing demo data, not a real agent"
    >
      <Icon name="eye" size={12} color={colors.white} />
      <Text variant="xs" color={colors.white}>
        Simulation
      </Text>
    </View>
  );
}

const styles = StyleSheet.create({
  header: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: spacing.sm,
    paddingHorizontal: spacing.xl,
    paddingVertical: spacing.sm,
    minHeight: 56,
  },
  headerBtn: {
    width: 44,
    height: 44,
    borderRadius: 22,
    alignItems: 'center',
    justifyContent: 'center',
  },

  card: {
    backgroundColor: colors.white,
    borderRadius: radii.md,
    borderWidth: 1,
    borderColor: colors.border,
    padding: spacing.lg,
  },
  cardStrong: { borderColor: colors.black, borderWidth: 1.5 },

  empty: {
    alignItems: 'center',
    paddingVertical: spacing.huge,
    paddingHorizontal: spacing.xl,
    gap: spacing.xs,
    overflow: 'hidden',
    borderRadius: radii.lg,
  },

  toast: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: spacing.md,
    backgroundColor: colors.black,
    borderRadius: radii.sm + 2,
    paddingHorizontal: spacing.lg,
    paddingVertical: spacing.md,
    marginTop: spacing.sm,
  },

  overlay: {
    flex: 1,
    backgroundColor: 'rgba(5,5,5,0.55)',
    alignItems: 'center',
    justifyContent: 'center',
    padding: spacing.xl,
  },
  dialog: {
    width: '100%',
    maxWidth: 440,
    backgroundColor: colors.white,
    borderRadius: radii.lg,
    borderWidth: 1,
    borderColor: colors.black,
    padding: spacing.xl,
    gap: spacing.md,
  },
  note: {
    flexDirection: 'row',
    gap: spacing.sm,
    backgroundColor: colors.surface2,
    borderRadius: radii.sm,
    padding: spacing.md,
    alignItems: 'flex-start',
  },
  simBadge: {
    position: 'absolute',
    top: spacing.sm,
    left: spacing.xl,
    zIndex: 50,
    flexDirection: 'row',
    alignItems: 'center',
    gap: spacing.xs,
    backgroundColor: colors.black,
    borderRadius: radii.pill,
    paddingHorizontal: spacing.sm,
    paddingVertical: 3,
  },
});
