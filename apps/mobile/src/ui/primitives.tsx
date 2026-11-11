/**
 * Core primitives: typography, buttons, chips, status markers, search,
 * statistic cards and the RHF-backed text field. Every interactive element
 * exposes real accessibility props and a visible focus ring.
 */
import { useState } from 'react';
import type { ReactNode } from 'react';
import {
  ActivityIndicator,
  Pressable,
  StyleSheet,
  Text as RNText,
  TextInput,
  View,
} from 'react-native';
import type {
  KeyboardTypeOptions,
  StyleProp,
  TextStyle,
  ViewStyle,
} from 'react-native';
import type { FieldValues, Path, PathValue, UseFormReturn } from 'react-hook-form';
import type { SessionStatus } from '@chernobyl/protocol';
import { colors, spacing, radii, typography, shadow } from '@/theme/tokens';
import type { TextVariant } from '@/theme/tokens';
import { Icon, type IconName } from '@/ui/icons';

/* ------------------------------------------------------------------ */
/* Text                                                                */
/* ------------------------------------------------------------------ */

export interface TextProps {
  variant?: TextVariant;
  color?: string;
  center?: boolean;
  children: ReactNode;
  numberOfLines?: number;
  onPress?: () => void;
  style?: StyleProp<TextStyle>;
  accessibilityRole?: 'text' | 'header' | 'button' | 'none' | 'alert';
  selectable?: boolean;
}

export function Text({
  variant = 'body',
  color = colors.text,
  center,
  children,
  numberOfLines,
  onPress,
  style,
  accessibilityRole,
  selectable,
}: TextProps) {
  return (
    <RNText
      numberOfLines={numberOfLines}
      onPress={onPress}
      selectable={selectable}
      accessibilityRole={accessibilityRole}
      style={[
        typography[variant],
        { color },
        center ? { textAlign: 'center' } : null,
        style,
      ]}
    >
      {children}
    </RNText>
  );
}

/* ------------------------------------------------------------------ */
/* Button                                                              */
/* ------------------------------------------------------------------ */

export interface ButtonProps {
  label: string;
  onPress?: () => void;
  variant?: 'primary' | 'secondary' | 'ghost';
  size?: 'md' | 'sm';
  loading?: boolean;
  disabled?: boolean;
  icon?: IconName;
  fullWidth?: boolean;
  accessibilityLabel?: string;
  /** colour of the focus ring — use white on dark surfaces */
  ringColor?: string;
  style?: StyleProp<ViewStyle>;
}

export function Button({
  label,
  onPress,
  variant = 'primary',
  size = 'md',
  loading = false,
  disabled = false,
  icon,
  fullWidth = true,
  accessibilityLabel,
  ringColor = colors.black,
  style,
}: ButtonProps) {
  const [focused, setFocused] = useState(false);
  const inactive = disabled || loading;
  const isDark = variant === 'primary';
  const labelColor = inactive ? colors.text3 : isDark ? colors.white : colors.black;

  return (
    <Pressable
      onPress={inactive ? undefined : onPress}
      disabled={inactive}
      focusable
      accessible
      accessibilityRole="button"
      accessibilityLabel={accessibilityLabel ?? label}
      accessibilityState={{ disabled: inactive, busy: loading }}
      onFocus={() => setFocused(true)}
      onBlur={() => setFocused(false)}
      style={({ pressed }) => [
        styles.button,
        size === 'md' ? styles.buttonMd : styles.buttonSm,
        variant === 'primary' && styles.buttonPrimary,
        variant === 'secondary' && styles.buttonSecondary,
        variant === 'ghost' && styles.buttonGhost,
        inactive && styles.buttonDisabled,
        fullWidth && styles.buttonFull,
        pressed && !inactive ? { opacity: 0.85 } : null,
        style,
      ]}
    >
      {loading ? (
        <ActivityIndicator size="small" color={isDark ? colors.white : colors.black} />
      ) : (
        <>
          <RNText
            style={[
              size === 'md' ? typography.bodyStrong : typography.smStrong,
              { color: labelColor, letterSpacing: -0.1 },
            ]}
          >
            {label}
          </RNText>
          {icon ? <Icon name={icon} size={size === 'md' ? 18 : 15} color={labelColor} /> : null}
        </>
      )}
      {focused && !inactive ? (
        <View
          pointerEvents="none"
          style={[
            StyleSheet.absoluteFill,
            styles.focusRing,
            { borderColor: ringColor, margin: -5 },
          ]}
        />
      ) : null}
    </Pressable>
  );
}

/* ------------------------------------------------------------------ */
/* Chip / filters                                                      */
/* ------------------------------------------------------------------ */

export interface ChipProps {
  label: string;
  active?: boolean;
  count?: number;
  /** leading decoration (e.g. an `AgentMark`), hidden from screen readers; the label says it all */
  icon?: ReactNode;
  onPress?: () => void;
  style?: StyleProp<ViewStyle>;
}

export function Chip({ label, active = false, count, icon, onPress, style }: ChipProps) {
  return (
    <Pressable
      onPress={onPress}
      disabled={!onPress}
      focusable={!!onPress}
      accessible
      accessibilityRole="button"
      accessibilityState={{ selected: active }}
      accessibilityLabel={count != null ? `${label}, ${count}` : label}
      style={({ pressed }) => [
        styles.chip,
        active && styles.chipActive,
        icon != null && styles.chipIcon,
        pressed && { opacity: 0.8 },
        style,
      ]}
    >
      {icon}
      <RNText style={[typography.smStrong, { color: active ? colors.white : colors.text2 }]}>
        {label}
        {count != null ? ` ${count}` : ''}
      </RNText>
    </Pressable>
  );
}

/* ------------------------------------------------------------------ */
/* Status markers                                                      */
/* ------------------------------------------------------------------ */

export type Tone = 'success' | 'warning' | 'error' | 'neutral' | 'inverse';

const TONE_COLOR: Record<Tone, string> = {
  success: colors.success,
  warning: colors.warning,
  error: colors.error,
  neutral: colors.text3,
  inverse: colors.white,
};

export function StatusDot({
  tone = 'neutral',
  label,
  inverse = false,
}: {
  tone?: Tone;
  label?: string;
  inverse?: boolean;
}) {
  return (
    <View style={styles.inline} accessible accessibilityLabel={label ? `${label}` : undefined}>
      <View style={[styles.dot, { backgroundColor: TONE_COLOR[tone] }]} />
      {label ? (
        <Text variant="sm" color={inverse ? colors.white : colors.text2}>
          {label}
        </Text>
      ) : null}
    </View>
  );
}

const PILL: Record<SessionStatus, { bg: string; fg: string; label: string }> = {
  running: { bg: colors.black, fg: colors.white, label: 'Running' },
  waiting: { bg: colors.warningBg, fg: colors.warning, label: 'Waiting' },
  completed: { bg: colors.surface2, fg: colors.text2, label: 'Completed' },
  interrupted: { bg: colors.errorBg, fg: colors.error, label: 'Interrupted' },
  idle: { bg: colors.surface2, fg: colors.text3, label: 'Idle' },
};

export function StatusPill({ status, inverse = false }: { status: SessionStatus; inverse?: boolean }) {
  const p = PILL[status];
  return (
    <View
      style={[
        styles.pill,
        { backgroundColor: p.bg },
        inverse && { borderWidth: 1, borderColor: colors.white },
      ]}
      accessible
      accessibilityLabel={`Status: ${p.label}`}
    >
      <RNText style={[typography.xs, { color: p.fg, fontWeight: '700' }]} numberOfLines={1}>
        {p.label}
      </RNText>
    </View>
  );
}

export type Risk = 'low' | 'medium' | 'high';

const RISK: Record<Risk, { bg: string; fg: string; label: string }> = {
  low: { bg: colors.surface2, fg: colors.text2, label: 'Low risk' },
  medium: { bg: colors.warningBg, fg: colors.warning, label: 'Medium risk' },
  high: { bg: colors.errorBg, fg: colors.error, label: 'High risk' },
};

export function RiskPill({ risk }: { risk: Risk }) {
  const r = RISK[risk];
  return (
    <View style={[styles.pill, { backgroundColor: r.bg }]} accessible accessibilityLabel={`Risk: ${r.label}`}>
      <RNText style={[typography.xs, { color: r.fg, fontWeight: '700' }]}>{r.label}</RNText>
    </View>
  );
}

/** ADR-009 provenance marker: the entity is demo data (`simulated` on the wire), not a live runtime. */
export function SimulatedTag() {
  return (
    <View style={[styles.pill, styles.simTag]} accessible accessibilityLabel="Simulated: demo data, not a real agent">
      <RNText style={[typography.xs, { color: colors.text2, fontWeight: '700' }]}>Simulated</RNText>
    </View>
  );
}

/**
 * ADR-018: a Claude Code session another process on the PC (a terminal,
 * VS Code) is writing right now. A state, so its dot is `success`.
 */
export function LivePcTag({ inverse = false }: { inverse?: boolean }) {
  return (
    <View
      style={[styles.pill, styles.livePcTag, inverse && styles.livePcInverse]}
      accessible
      accessibilityLabel="Live on PC: this session is running on your PC right now"
    >
      <View style={[styles.dot, { backgroundColor: colors.success }]} />
      <RNText style={[typography.xs, { color: inverse ? colors.white : colors.text, fontWeight: '700' }]}>
        Live on PC
      </RNText>
    </View>
  );
}

/* ------------------------------------------------------------------ */
/* Progress                                                            */
/* ------------------------------------------------------------------ */

export function ProgressBar({
  value,
  tone = 'dark',
  style,
}: {
  value: number;
  tone?: 'dark' | 'success';
  style?: StyleProp<ViewStyle>;
}) {
  const pct = Math.max(0, Math.min(100, Math.round(value)));
  return (
    <View
      style={[styles.track, style]}
      accessible
      accessibilityRole="progressbar"
      accessibilityValue={{ min: 0, max: 100, now: pct }}
    >
      <View
        style={[
          styles.fill,
          {
            width: `${pct}%`,
            backgroundColor: tone === 'success' ? colors.success : colors.black,
          },
        ]}
      />
    </View>
  );
}

/* ------------------------------------------------------------------ */
/* Statistic card                                                      */
/* ------------------------------------------------------------------ */

export interface StatCardProps {
  label: string;
  value: string;
  caption?: string;
  onPress?: () => void;
  style?: StyleProp<ViewStyle>;
}

export function StatCard({ label, value, caption, onPress, style }: StatCardProps) {
  const body = (
    <View style={styles.statBody}>
      <Text variant="eyebrow" color={colors.text3}>
        {label}
      </Text>
      <Text variant="h2" style={{ marginTop: spacing.xs }}>
        {value}
      </Text>
      {caption ? (
        <>
          <View style={styles.divider} />
          <Text variant="xs" color={colors.text2}>
            {caption}
          </Text>
        </>
      ) : null}
      {onPress ? (
        <Icon name="chevron-right" size={16} color={colors.text3} style={{ position: 'absolute', right: 0, top: 2 } as StyleProp<ViewStyle>} />
      ) : null}
    </View>
  );

  if (!onPress) {
    return <View style={[styles.card, styles.statCard, style]}>{body}</View>;
  }
  return (
    <Pressable
      onPress={onPress}
      focusable
      accessible
      accessibilityRole="button"
      accessibilityLabel={`${label}: ${value}`}
      style={({ pressed }) => [
        styles.card,
        styles.statCard,
        pressed && { opacity: 0.85, transform: [{ scale: 0.99 }] },
        style,
      ]}
    >
      {body}
    </Pressable>
  );
}

/* ------------------------------------------------------------------ */
/* Search                                                              */
/* ------------------------------------------------------------------ */

export interface SearchFieldProps {
  value: string;
  onChangeText: (v: string) => void;
  placeholder?: string;
  onDark?: boolean;
  onSubmit?: () => void;
  style?: StyleProp<ViewStyle>;
}

export function SearchField({
  value,
  onChangeText,
  placeholder = 'Search…',
  onDark = false,
  onSubmit,
  style,
}: SearchFieldProps) {
  return (
    <View
      style={[
        styles.search,
        !onDark && { borderColor: colors.border, borderWidth: 1 },
        style,
      ]}
    >
      <Icon name="search" size={18} color={colors.text3} />
      <TextInput
        value={value}
        onChangeText={onChangeText}
        onSubmitEditing={onSubmit}
        placeholder={placeholder}
        placeholderTextColor={colors.text3}
        returnKeyType="search"
        accessible
        accessibilityLabel={placeholder}
        style={[styles.searchInput, { color: colors.text }]}
        autoCorrect={false}
      />
      {value.length > 0 ? (
        <Pressable
          onPress={() => onChangeText('')}
          hitSlop={10}
          accessible
          accessibilityRole="button"
          accessibilityLabel="Clear search"
        >
          <Icon name="x" size={16} color={colors.text3} />
        </Pressable>
      ) : null}
    </View>
  );
}

/* ------------------------------------------------------------------ */
/* Section title                                                       */
/* ------------------------------------------------------------------ */

export function SectionTitle({
  children,
  actionLabel,
  onAction,
  style,
}: {
  children: ReactNode;
  actionLabel?: string;
  onAction?: () => void;
  style?: StyleProp<ViewStyle>;
}) {
  return (
    <View style={[styles.sectionRow, style]}>
      <Text variant="h3" accessibilityRole="header">
        {children}
      </Text>
      {actionLabel && onAction ? (
        <Pressable onPress={onAction} hitSlop={8} accessible accessibilityRole="button">
          <Text variant="smStrong" color={colors.text2}>
            {actionLabel}
          </Text>
        </Pressable>
      ) : null}
    </View>
  );
}

/* ------------------------------------------------------------------ */
/* RHF text field                                                      */
/* ------------------------------------------------------------------ */

export interface TextFieldProps<T extends FieldValues> {
  form: UseFormReturn<T>;
  name: Path<T>;
  label?: string;
  placeholder?: string;
  hint?: string;
  secureTextEntry?: boolean;
  keyboardType?: KeyboardTypeOptions;
  autoCapitalize?: 'none' | 'sentences' | 'words' | 'characters';
  multiline?: boolean;
  onSubmitEditing?: () => void;
}

export function TextField<T extends FieldValues>({
  form,
  name,
  label,
  placeholder,
  hint,
  secureTextEntry,
  keyboardType,
  autoCapitalize = 'sentences',
  multiline = false,
  onSubmitEditing,
}: TextFieldProps<T>) {
  const error = form.formState.errors[name];
  const value = form.watch(name);
  return (
    <View style={{ gap: 6 }}>
      {label ? (
        <Text variant="smStrong" color={colors.text}>
          {label}
        </Text>
      ) : null}
      <TextInput
        value={value == null ? '' : String(value)}
        onChangeText={(t) =>
          form.setValue(name, t as PathValue<T, Path<T>>, { shouldDirty: true })
        }
        placeholder={placeholder}
        placeholderTextColor={colors.text3}
        secureTextEntry={secureTextEntry}
        keyboardType={keyboardType}
        autoCapitalize={autoCapitalize}
        autoCorrect={false}
        multiline={multiline}
        onSubmitEditing={onSubmitEditing}
        accessible
        accessibilityLabel={label ?? placeholder}
        accessibilityHint={error ? String(error.message) : undefined}
        style={[
          styles.input,
          multiline && styles.inputMultiline,
          !!error && { borderColor: colors.error },
        ]}
      />
      {error ? (
        <Text variant="xs" color={colors.error} accessibilityRole="alert">
          {String(error.message)}
        </Text>
      ) : hint ? (
        <Text variant="xs" color={colors.text3}>
          {hint}
        </Text>
      ) : null}
    </View>
  );
}

/* ------------------------------------------------------------------ */
/* Styles                                                              */
/* ------------------------------------------------------------------ */

const styles = StyleSheet.create({
  inline: { flexDirection: 'row', alignItems: 'center', gap: 6 },
  dot: { width: 8, height: 8, borderRadius: 4 },

  button: {
    flexDirection: 'row',
    alignItems: 'center',
    justifyContent: 'center',
    gap: 8,
    borderRadius: radii.md,
    borderWidth: 1,
    position: 'relative',
  },
  buttonMd: { minHeight: 52, paddingHorizontal: 22 },
  buttonSm: { minHeight: 38, paddingHorizontal: 14, borderRadius: radii.sm },
  buttonPrimary: { backgroundColor: colors.black, borderColor: colors.black },
  buttonSecondary: { backgroundColor: colors.white, borderColor: colors.black },
  buttonGhost: { backgroundColor: 'transparent', borderColor: 'transparent' },
  buttonDisabled: { backgroundColor: colors.surface2, borderColor: colors.border },
  buttonFull: { alignSelf: 'stretch' },
  focusRing: {
    position: 'absolute',
    borderWidth: 2,
    borderRadius: radii.lg,
  },

  chip: {
    minHeight: 36,
    paddingHorizontal: 14,
    paddingVertical: 7,
    borderRadius: radii.pill,
    borderWidth: 1,
    borderColor: colors.border,
    backgroundColor: colors.white,
    alignItems: 'center',
    justifyContent: 'center',
  },
  chipActive: { backgroundColor: colors.black, borderColor: colors.black },
  chipIcon: { flexDirection: 'row', gap: spacing.sm, paddingLeft: spacing.md },

  pill: {
    paddingHorizontal: 9,
    paddingVertical: 4,
    borderRadius: radii.pill,
    alignSelf: 'flex-start',
  },
  simTag: {
    backgroundColor: colors.surface2,
    borderWidth: StyleSheet.hairlineWidth,
    borderColor: colors.border,
  },
  livePcTag: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: spacing.xs,
    borderWidth: StyleSheet.hairlineWidth,
    borderColor: colors.black,
  },
  livePcInverse: { borderColor: colors.text2 },

  track: {
    height: 6,
    borderRadius: 3,
    backgroundColor: '#E8E8E8',
    overflow: 'hidden',
  },
  fill: { height: '100%', borderRadius: 3 },

  card: {
    backgroundColor: colors.white,
    borderRadius: radii.md,
    borderWidth: 1,
    borderColor: colors.border,
  },
  statCard: { padding: spacing.lg },
  statBody: { gap: 2 },
  divider: {
    height: StyleSheet.hairlineWidth,
    backgroundColor: colors.border,
    marginTop: spacing.sm,
    marginBottom: spacing.xs,
  },

  search: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: 8,
    height: 46,
    borderRadius: radii.sm,
    backgroundColor: colors.white,
    paddingHorizontal: 14,
  },
  searchInput: { flex: 1, fontSize: 15, paddingVertical: 0 },

  sectionRow: {
    flexDirection: 'row',
    alignItems: 'center',
    justifyContent: 'space-between',
    gap: spacing.md,
  },

  input: {
    minHeight: 50,
    borderWidth: 1,
    borderColor: colors.border,
    borderRadius: radii.sm,
    backgroundColor: colors.white,
    paddingHorizontal: 14,
    paddingVertical: 12,
    fontSize: 15,
    color: colors.text,
  },
  inputMultiline: { minHeight: 110, textAlignVertical: 'top' },
});

export { shadow };
