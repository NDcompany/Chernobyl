/**
 * List/detail components: agent cards, session rows, permission cards,
 * activity rows, the prompt composer, task timeline, output block, file
 * rows and segmented tabs.
 */
import { useForm } from 'react-hook-form';
import { useRef, useState } from 'react';
import { ActivityIndicator, Image, Modal, Pressable, StyleSheet, View } from 'react-native';
import type { StyleProp, ViewStyle } from 'react-native';
import { useSafeAreaInsets } from 'react-native-safe-area-context';
import { CameraView, useCameraPermissions } from 'expo-camera';
import {
  PROMPT_IMAGES_DATA_MAX,
  PROMPT_IMAGES_MAX,
  type ActivityEvent,
  type Agent,
  type FileChange,
  type Host,
  type PermissionRequest,
  type PromptImage,
  type Session,
  type SessionStep,
} from '@chernobyl/protocol';
import { colors, spacing, radii, typography } from '@/theme/tokens';
import {
  Button,
  LivePcTag,
  RiskPill,
  SimulatedTag,
  StatusDot,
  StatusPill,
  Text,
  TextField,
} from '@/ui/primitives';
import { Card } from '@/ui/surface';
import { AgentIcon } from '@/ui/graphics';
import { AgentMark } from '@/ui/agentMark';
import { Icon, type IconName } from '@/ui/icons';
import { zodHandler } from '@/forms/zodBridge';
import { transportKind } from '@/data/transport';
import {
  FRAME_MAX_BYTES,
  cleanBase64,
  filterSlashCommands,
  imageBudgetLeft,
  pickPictureSize,
  sealedFrameBytes,
  slashQuery,
} from '@/utils/composer';
import { useAppStore } from '@/store/appStore';
import { z } from 'zod';
import {
  agentStatusLabel,
  agentTone,
  countdown,
  relativeTime,
} from '@/utils/format';
import { useNow } from '@/hooks/useReducedMotion';

/* ------------------------------------------------------------------ */
/* Agent card                                                          */
/* ------------------------------------------------------------------ */

export interface AgentCardProps {
  agent: Agent;
  host?: Host;
  activeCount?: number;
  totalCount?: number;
  onPress?: () => void;
  variant?: 'tile' | 'row';
}

export function AgentCard({ agent, host, activeCount = 0, totalCount, onPress, variant = 'row' }: AgentCardProps) {
  const status = `${agentStatusLabel(agent.state)}`;
  // ADR-009: demo entities never read as live
  const simulated = Boolean(agent.simulated || host?.simulated);
  const countLabel =
    activeCount > 0 ? `${activeCount} active` : totalCount ? `${totalCount} total` : 'no sessions';

  const meta = (
    <View style={{ flex: 1, gap: 3 }}>
      <Text variant={variant === 'tile' ? 'bodyStrong' : 'bodyStrong'} numberOfLines={1}>
        {agent.name}
      </Text>
      {variant === 'row' ? (
        <Text variant="xs" color={colors.text2} numberOfLines={1}>
          {host?.name ?? 'Unknown host'} · {countLabel}
        </Text>
      ) : null}
      <View style={styles.markers}>
        <StatusDot tone={agentTone(agent.state)} label={status} />
        {simulated ? <SimulatedTag /> : null}
      </View>
    </View>
  );

  const body =
    variant === 'tile' ? (
      <View style={styles.tile}>
        <AgentIcon kind={agent.kind} size={42} />
        {meta}
        <Text variant="xs" color={colors.text3} numberOfLines={1} style={{ marginTop: spacing.xs }}>
          {countLabel}
        </Text>
      </View>
    ) : (
      <View style={styles.rowCard}>
        <AgentIcon kind={agent.kind} size={44} />
        {meta}
        <Icon name="chevron-right" size={18} color={colors.text3} />
      </View>
    );

  if (!onPress) return <View style={variant === 'tile' ? styles.tileWrap : undefined}>{body}</View>;
  return (
    <Pressable
      onPress={onPress}
      focusable
      accessible
      accessibilityRole="button"
      accessibilityLabel={`${agent.name}, ${status}, ${countLabel}${simulated ? ', simulated' : ''}`}
      style={({ pressed }) => [
        variant === 'tile' ? styles.tileWrap : undefined,
        pressed && { opacity: 0.85, transform: [{ scale: 0.99 }] },
      ]}
    >
      {body}
    </Pressable>
  );
}

/* ------------------------------------------------------------------ */
/* Session row                                                         */
/* ------------------------------------------------------------------ */

export interface SessionRowProps {
  session: Session;
  agent?: Agent;
  host?: Host;
  onPress?: () => void;
  style?: StyleProp<ViewStyle>;
}

export function SessionRow({ session, agent, host, onPress, style }: SessionRowProps) {
  const now = useNow(60_000);
  const simulated = Boolean(agent?.simulated || host?.simulated);
  return (
    <Card
      onPress={onPress}
      accessibilityLabel={`${session.title}, ${session.status}, ${agent?.name ?? ''}${session.livePc ? ', live on PC' : ''}${simulated ? ', simulated' : ''}`}
      style={[styles.sessionRow, style]}
    >
      <View style={{ flexDirection: 'row', gap: spacing.sm, alignItems: 'flex-start' }}>
        <View style={{ flex: 1, gap: 4 }}>
          <Text variant="bodyStrong" numberOfLines={2}>
            {session.title}
          </Text>
          <View style={styles.sessionAgent}>
            <AgentMark kind={agent?.kind} size={16} />
            <Text variant="xs" color={colors.text2} numberOfLines={1} style={{ flexShrink: 1 }}>
              {agent?.name ?? 'Agent'} · {host?.name ?? 'host'}
            </Text>
          </View>
          {session.forkedFrom ? (
            <Text variant="xs" color={colors.text3} numberOfLines={1}>
              Forked from “{session.forkedFrom}”
            </Text>
          ) : null}
          {simulated ? <SimulatedTag /> : null}
        </View>
        {/* ADR-018: written by a terminal or VS Code right now */}
        {session.livePc ? <LivePcTag /> : <StatusPill status={session.status} />}
      </View>
      <View style={styles.sessionMeta}>
        <Icon name="folder" size={13} color={colors.text3} />
        <Text variant="mono" color={colors.text3} numberOfLines={1} style={{ flex: 1 }}>
          {session.project}
        </Text>
        <Text variant="xs" color={colors.text3}>
          {relativeTime(session.lastActivityAt, now)}
        </Text>
        <Icon name="chevron-right" size={16} color={colors.text3} />
      </View>
    </Card>
  );
}

/* ------------------------------------------------------------------ */
/* Mono block (commands / paths on black)                              */
/* ------------------------------------------------------------------ */

export function MonoBlock({ text, caption }: { text: string; caption?: string }) {
  return (
    <View style={styles.monoBlock}>
      {caption ? (
        <Text variant="xs" color="rgba(255,255,255,0.6)">
          {caption}
        </Text>
      ) : null}
      <Text variant="mono" color={colors.white} selectable>
        {text}
      </Text>
    </View>
  );
}

/* ------------------------------------------------------------------ */
/* Permission request card                                             */
/* ------------------------------------------------------------------ */

export interface PermissionCardProps {
  permission: PermissionRequest;
  agent?: Agent;
  host?: Host;
  session?: Session;
  onPress?: () => void;
  now?: number;
  variant?: 'compact' | 'full';
  /** dimmed state for already-decided requests */
  resolved?: boolean;
}

export function PermissionCard({
  permission,
  agent,
  host,
  session,
  onPress,
  now,
  variant = 'compact',
  resolved = false,
}: PermissionCardProps) {
  // callers with a live ticker pass `now`; static callers fall back to mount time
  const [mountedAt] = useState(() => Date.now());
  const nowMs = now ?? mountedAt;
  const countdownText =
    permission.status === 'pending' ? countdown(permission.expiresAt, nowMs) : '';
  const simulated = Boolean(agent?.simulated || host?.simulated);
  const simLabel = `${permission.fromPc ? ', from a PC session' : ''}${simulated ? ', simulated' : ''}`;
  // ADR-018: asked by a Claude Code session in a terminal or VS Code
  const origin = permission.fromPc ? ' · from a PC session' : '';

  if (variant === 'compact') {
    return (
      <Card
        onPress={onPress}
        accessibilityLabel={`${permission.title}: ${permission.command}, risk ${permission.risk}${simLabel}`}
        style={[styles.permCompact, resolved && { opacity: 0.6 }]}
      >
        <View style={{ flex: 1, gap: 3 }}>
          <Text variant="smStrong" numberOfLines={1}>
            {permission.title}
          </Text>
          <Text variant="mono" color={colors.text2} numberOfLines={1}>
            {permission.command}
          </Text>
          <Text variant="xs" color={colors.text3} numberOfLines={1}>
            {agent?.name ?? 'Agent'} · {host?.name ?? 'host'}
            {origin}
          </Text>
          {simulated ? <SimulatedTag /> : null}
        </View>
        <View style={{ alignItems: 'flex-end', gap: 6 }}>
          <RiskPill risk={permission.risk} />
          <Icon name="chevron-right" size={16} color={colors.text3} />
        </View>
      </Card>
    );
  }

  return (
    <Card
      onPress={onPress}
      accessibilityLabel={`${permission.title}: ${permission.command}${simLabel}`}
      style={[styles.permFull, resolved && { opacity: 0.65 }]}
    >
      <View style={styles.permHead}>
        <AgentIcon kind={agent?.kind ?? 'custom'} size={40} />
        <View style={{ flex: 1, gap: 2 }}>
          <Text variant="bodyStrong" numberOfLines={1}>
            {permission.title}
          </Text>
          <Text variant="xs" color={colors.text2} numberOfLines={1}>
            {agent?.name ?? 'Agent'} · {host?.name ?? 'host'}
            {session ? ` · ${session.title}` : ''}
            {origin}
          </Text>
          {simulated ? <SimulatedTag /> : null}
        </View>
        <RiskPill risk={permission.risk} />
      </View>
      <MonoBlock text={permission.command} caption="Exact command" />
      <View style={styles.permFoot}>
        <Icon name="terminal" size={13} color={colors.text3} />
        <Text variant="mono" color={colors.text3} numberOfLines={1} style={{ flex: 1 }}>
          {permission.directory ?? '—'}
        </Text>
        <Text variant="xs" color={colors.text3}>
          {countdownText}
        </Text>
      </View>
    </Card>
  );
}

/* ------------------------------------------------------------------ */
/* Activity row                                                        */
/* ------------------------------------------------------------------ */

const ACTIVITY_DOT: Record<ActivityEvent['kind'], string> = {
  session: colors.black,
  permission: colors.warning,
  host: colors.text3,
  agent: colors.black,
};

export function ActivityRow({
  event,
  time,
  onPress,
}: {
  event: ActivityEvent;
  time: string;
  onPress?: () => void;
}) {
  const body = (
    <View style={styles.activityRow}>
      <View style={[styles.activityDot, { backgroundColor: ACTIVITY_DOT[event.kind] }]} />
      <Text variant="sm" color={colors.text} style={{ flex: 1 }}>
        {event.text}
      </Text>
      <Text variant="xs" color={colors.text3}>
        {time}
      </Text>
    </View>
  );
  if (!onPress) return body;
  return (
    <Pressable onPress={onPress} focusable accessible accessibilityRole="button" style={({ pressed }) => pressed && { opacity: 0.7 }}>
      {body}
    </Pressable>
  );
}

/* ------------------------------------------------------------------ */
/* Prompt composer                                                     */
/* ------------------------------------------------------------------ */

const promptSchema = z.object({
  prompt: z
    .string()
    .trim()
    .min(2, 'Type a message first')
    // ADR-015 §3.4: submit_prompt.text limit
    .max(32_000, 'Message is too long (32,000 characters at most)'),
});

const BUSY_CAPTION = {
  merge: 'Joins the current turn',
  queue: 'Runs after this turn',
  none: 'Agent is working…',
} as const;

export interface PromptComposerProps {
  /** `images` is empty unless `canAttach` and the user added photos (ADR-017). */
  onSubmit: (text: string, images: PromptImage[]) => void;
  /** shows Stop while busy */
  onInterrupt?: () => void;
  /** a turn is running (or the session waits) */
  busy?: boolean;
  /**
   * ADR-015 `Agent.live.promptWhileBusy`: 'merge' and 'queue' keep sending
   * enabled while busy and say where the prompt goes; 'none' (default)
   * blocks sending meanwhile.
   */
  promptWhileBusy?: 'merge' | 'queue' | 'none';
  disabled?: boolean;
  hint?: string;
  placeholder?: string;
  /** ADR-017: slash command names; a leading "/" lists the matching ones. */
  slashCommands?: string[];
  /** ADR-017: the camera button (agents whose `live.composer` has 'images'). */
  canAttach?: boolean;
}

/** A photo waiting in the composer; `uri` is the local preview, never sent. */
interface ComposerImage extends PromptImage {
  uri: string;
}

export function PromptComposer({
  onSubmit,
  onInterrupt,
  busy = false,
  promptWhileBusy = 'none',
  disabled = false,
  hint,
  placeholder = 'Ask the agent…',
  slashCommands,
  canAttach = false,
}: PromptComposerProps) {
  const form = useForm<{ prompt: string }>({ defaultValues: { prompt: '' } });
  const [images, setImages] = useState<ComposerImage[]>([]);
  const [camera, setCamera] = useState(false);
  const [tooLarge, setTooLarge] = useState(false);
  const text = form.watch('prompt') ?? '';
  const query = slashCommands?.length ? slashQuery(text) : null;
  const matches = query === null || !slashCommands ? [] : filterSlashCommands(slashCommands, query);
  const submit = zodHandler(promptSchema, form, (data) => {
    const prompt = data.prompt.trim();
    const payload = images.map(({ mediaType, data: b64 }) => ({ mediaType, data: b64 }));
    // one WebSocket frame carries the whole prompt (ADR-017): check before sending
    const probe = { type: 'submit_prompt', commandId: 'x'.repeat(48), sessionId: 'x'.repeat(64), text: prompt, images: payload };
    if (payload.length && sealedFrameBytes(probe) > FRAME_MAX_BYTES) {
      setTooLarge(true);
      return;
    }
    setTooLarge(false);
    onSubmit(prompt, payload);
    setImages([]);
    form.reset({ prompt: '' });
  });
  // ADR-009 honesty: never tell a live-companion user "nothing is executed
  // for real." The caption reflects the ACTIVE transport, not a hardcoded claim.
  // Subscribing to the connection keeps it current when pairing swaps transports.
  useAppStore((s) => s.connection);
  const idleCaption =
    transportKind() === 'mock'
      ? 'Mock host — nothing is executed for real'
      : 'Connected to the real companion — commands are enforced host-side';

  return (
    <View style={styles.composer}>
      {matches.length ? (
        <View style={styles.slashMenu}>
          {matches.map((name) => (
            <Pressable
              key={name}
              onPress={() => form.setValue('prompt', `/${name} `, { shouldDirty: true })}
              accessibilityRole="button"
              accessibilityLabel={`Insert /${name}`}
              style={({ pressed }) => [styles.slashRow, pressed && styles.pressed]}
            >
              <Text variant="mono" numberOfLines={1}>
                /{name}
              </Text>
            </Pressable>
          ))}
        </View>
      ) : null}
      {images.length ? (
        <View style={styles.thumbRow}>
          {images.map((im, i) => (
            <View key={im.uri} style={styles.thumb}>
              <Image source={{ uri: im.uri }} style={styles.thumbImage} accessibilityIgnoresInvertColors />
              <Pressable
                onPress={() => {
                  setTooLarge(false);
                  setImages(images.filter((_, j) => j !== i));
                }}
                hitSlop={8}
                accessibilityRole="button"
                accessibilityLabel={`Remove photo ${i + 1}`}
                style={styles.thumbRemove}
              >
                <Icon name="x" size={12} color={colors.white} />
              </Pressable>
            </View>
          ))}
        </View>
      ) : null}
      <TextField
        form={form}
        name="prompt"
        placeholder={placeholder}
        multiline
        onSubmitEditing={submit}
      />
      {tooLarge ? (
        <Text variant="xs" color={colors.error} accessibilityRole="alert">
          Too large to send in one message: remove a photo or shorten the text.
        </Text>
      ) : null}
      <View style={styles.composerRow}>
        <Text variant="xs" color={colors.text3} style={{ flex: 1 }} numberOfLines={1}>
          {hint ?? (busy ? BUSY_CAPTION[promptWhileBusy] : images.length ? `${images.length} of ${PROMPT_IMAGES_MAX} photos` : idleCaption)}
        </Text>
        {canAttach ? (
          <Pressable
            onPress={() => setCamera(true)}
            disabled={disabled || images.length >= PROMPT_IMAGES_MAX}
            hitSlop={6}
            accessibilityRole="button"
            accessibilityLabel="Attach a photo"
            accessibilityState={{ disabled: disabled || images.length >= PROMPT_IMAGES_MAX }}
            style={({ pressed }) => [
              styles.iconButton,
              (disabled || images.length >= PROMPT_IMAGES_MAX) && styles.dimmed,
              pressed && styles.pressed,
            ]}
          >
            <Icon name="camera" size={18} />
          </Pressable>
        ) : null}
        {busy && onInterrupt ? (
          <Button
            label="Stop"
            variant="secondary"
            size="sm"
            icon="stop"
            fullWidth={false}
            onPress={onInterrupt}
            accessibilityLabel="Stop"
          />
        ) : null}
        <Button
          label="Send"
          size="sm"
          icon="send"
          fullWidth={false}
          disabled={disabled || (busy && promptWhileBusy === 'none')}
          onPress={submit}
          accessibilityLabel="Send prompt to agent"
        />
      </View>
      {canAttach ? (
        <PhotoCapture
          visible={camera}
          budget={imageBudgetLeft(images, PROMPT_IMAGES_DATA_MAX)}
          onClose={() => setCamera(false)}
          onCapture={(im) => setImages((list) => [...list, im].slice(0, PROMPT_IMAGES_MAX))}
        />
      ) : null}
    </View>
  );
}

/** The camera shoots at most this long edge (Claude downsizes past 1568 px anyway). */
const SHOT_EDGE = 1280;
/** JPEG qualities tried in turn until the photo fits what is left of the budget. */
const SHOT_QUALITIES = [0.7, 0.5, 0.35, 0.2, 0.1];

/**
 * ADR-017: one photo from the camera (expo-camera, already in the app for the
 * pairing QR). Gallery picking would need expo-image-picker, which the app
 * does not ship. The photo is re-encoded at falling JPEG quality until its
 * base64 fits `budget`; it stays on the phone until the prompt is sent.
 */
function PhotoCapture({
  visible,
  budget,
  onClose,
  onCapture,
}: {
  visible: boolean;
  budget: number;
  onClose: () => void;
  onCapture: (image: ComposerImage) => void;
}) {
  const [permission, requestPermission] = useCameraPermissions();
  const ref = useRef<CameraView>(null);
  const insets = useSafeAreaInsets();
  const [size, setSize] = useState<string>();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();

  const close = () => {
    setError(undefined);
    onClose();
  };
  const shoot = async () => {
    const cam = ref.current;
    if (!cam || busy) return;
    setBusy(true);
    setError(undefined);
    try {
      const pic = await cam.takePictureAsync({ pictureRef: true, shutterSound: false });
      try {
        for (const quality of SHOT_QUALITIES) {
          const out = await pic.savePictureAsync({ quality, base64: true });
          const data = cleanBase64(out.base64 ?? '');
          if (data.startsWith('/9j/') && data.length <= budget) {
            onCapture({ uri: out.uri, mediaType: 'image/jpeg', data });
            close();
            return;
          }
        }
        setError('This photo is too large to add. Remove a photo or send what you have.');
      } finally {
        pic.release();
      }
    } catch (e) {
      setError(e instanceof Error && e.message ? e.message : 'The camera could not take a photo');
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal visible={visible} animationType="slide" onRequestClose={close} statusBarTranslucent>
      <View style={[styles.cameraScreen, { paddingTop: insets.top + spacing.md, paddingBottom: insets.bottom + spacing.xl }]}>
        <View style={styles.cameraHead}>
          <Text variant="h3" color={colors.white} style={{ flex: 1 }} accessibilityRole="header">
            Attach a photo
          </Text>
          <Pressable onPress={close} hitSlop={10} accessibilityRole="button" accessibilityLabel="Close camera">
            <Icon name="x" size={20} color={colors.white} />
          </Pressable>
        </View>
        {!permission ? (
          <ActivityIndicator color={colors.white} style={{ flex: 1 }} />
        ) : permission.granted ? (
          <View style={styles.cameraBox}>
            {visible ? (
              <CameraView
                ref={ref}
                style={StyleSheet.absoluteFill}
                facing="back"
                pictureSize={size}
                onCameraReady={() => {
                  void ref.current
                    ?.getAvailablePictureSizesAsync()
                    .then((sizes) => setSize(pickPictureSize(sizes, SHOT_EDGE)))
                    .catch(() => undefined);
                }}
                onMountError={(e) => setError(e.message || 'The camera failed to start')}
              />
            ) : null}
          </View>
        ) : (
          <View style={styles.cameraAsk}>
            <Text variant="body" color={colors.white} center>
              {permission.canAskAgain
                ? 'CHERNOBYL needs the camera to take the photo. It is sent to Claude on your PC with your next message.'
                : 'Camera access is off. Turn it on for CHERNOBYL in the system settings to attach photos.'}
            </Text>
            {permission.canAskAgain ? (
              <Button label="Allow camera" variant="primary" ringColor={colors.white} onPress={() => void requestPermission()} />
            ) : null}
          </View>
        )}
        {error ? (
          <Text variant="sm" color={colors.white} center accessibilityRole="alert">
            {error}
          </Text>
        ) : null}
        {permission?.granted ? (
          <Pressable
            onPress={() => void shoot()}
            disabled={busy}
            accessibilityRole="button"
            accessibilityLabel="Take photo"
            accessibilityState={{ disabled: busy, busy }}
            style={({ pressed }) => [styles.shutter, pressed && styles.pressed]}
          >
            {busy ? <ActivityIndicator color={colors.black} /> : null}
          </Pressable>
        ) : null}
      </View>
    </Modal>
  );
}

/* ------------------------------------------------------------------ */
/* Task timeline                                                       */
/* ------------------------------------------------------------------ */

export function Timeline({ steps }: { steps: SessionStep[] }) {
  return (
    <View>
      {steps.map((step, i) => {
        const last = i === steps.length - 1;
        return (
          <View key={step.id} style={styles.timelineRow}>
            <View style={styles.timelineGutter}>
              <View
                style={[
                  styles.timelineDot,
                  step.status === 'done' && { backgroundColor: colors.black, borderColor: colors.black },
                  step.status === 'active' && { backgroundColor: colors.white, borderColor: colors.black, borderWidth: 2.5 },
                  step.status === 'pending' && { backgroundColor: colors.white, borderColor: colors.border },
                ]}
              />
              {last ? null : <View style={styles.timelineLine} />}
            </View>
            <Text
              variant={step.status === 'active' ? 'smStrong' : 'sm'}
              color={step.status === 'pending' ? colors.text3 : step.status === 'active' ? colors.text : colors.text2}
              style={{ flex: 1, paddingBottom: last ? 0 : spacing.md }}
            >
              {step.label}
            </Text>
          </View>
        );
      })}
    </View>
  );
}

/* ------------------------------------------------------------------ */
/* Output block                                                        */
/* ------------------------------------------------------------------ */

export function OutputBlock({ lines }: { lines: { id: string; text: string; stream: string }[] }) {
  if (lines.length === 0) {
    return (
      <Text variant="sm" color={colors.text3}>
        Output will appear here as soon as the agent starts.
      </Text>
    );
  }
  return (
    <View style={styles.output}>
      {lines.map((line) => (
        <Text
          key={line.id}
          variant="mono"
          color={line.stream === 'stderr' ? colors.error : line.stream === 'system' ? colors.text2 : colors.text}
          style={line.stream === 'system' ? { fontStyle: 'italic' } : undefined}
          selectable
        >
          {line.text}
        </Text>
      ))}
    </View>
  );
}

/* ------------------------------------------------------------------ */
/* File row                                                            */
/* ------------------------------------------------------------------ */

export function FileRow({ file }: { file: FileChange }) {
  return (
    <View style={styles.fileRow}>
      <Icon name="file" size={15} color={colors.text3} />
      <Text variant="mono" color={colors.text} numberOfLines={1} style={{ flex: 1 }}>
        {file.path}
      </Text>
      <Text variant="mono" color={colors.success}>
        +{file.additions}
      </Text>
      <Text variant="mono" color={colors.error}>
        -{file.deletions}
      </Text>
    </View>
  );
}

/* ------------------------------------------------------------------ */
/* Detail row (label/value)                                            */
/* ------------------------------------------------------------------ */

export function DetailRow({
  label,
  value,
  mono = false,
  onPress,
  icon,
}: {
  label: string;
  value: string;
  mono?: boolean;
  onPress?: () => void;
  icon?: IconName;
}) {
  const content = (
    <View style={styles.detailRow}>
      <Text variant="sm" color={colors.text2} style={{ width: 118 }}>
        {label}
      </Text>
      <Text
        variant={mono ? 'mono' : 'smStrong'}
        color={colors.text}
        style={{ flex: 1, textAlign: 'right' }}
        numberOfLines={2}
      >
        {value}
      </Text>
      {onPress || icon ? <Icon name={onPress ? 'chevron-right' : (icon as IconName)} size={15} color={colors.text3} /> : null}
    </View>
  );
  if (!onPress) return content;
  return (
    <Pressable onPress={onPress} focusable accessible accessibilityRole="button" style={({ pressed }) => pressed && { opacity: 0.6 }}>
      {content}
    </Pressable>
  );
}

/* ------------------------------------------------------------------ */
/* Segmented tabs                                                      */
/* ------------------------------------------------------------------ */

export interface SegmentedTabsProps {
  tabs: { key: string; label: string; badge?: number }[];
  active: string;
  onChange: (key: string) => void;
}

export function SegmentedTabs({ tabs, active, onChange }: SegmentedTabsProps) {
  return (
    <View style={styles.segment} accessibilityRole="tablist">
      {tabs.map((t) => {
        const selected = t.key === active;
        return (
          <Pressable
            key={t.key}
            onPress={() => onChange(t.key)}
            focusable
            accessible
            accessibilityRole="tab"
            accessibilityState={{ selected }}
            accessibilityLabel={t.badge ? `${t.label}, ${t.badge}` : t.label}
            style={[styles.segmentItem, selected && styles.segmentItemActive]}
          >
            <Text
              variant="smStrong"
              color={selected ? colors.white : colors.text2}
            >
              {t.label}
              {t.badge ? ` ${t.badge}` : ''}
            </Text>
          </Pressable>
        );
      })}
    </View>
  );
}

/* ------------------------------------------------------------------ */
/* Styles                                                              */
/* ------------------------------------------------------------------ */

const styles = StyleSheet.create({
  tileWrap: { width: 158 },
  markers: { flexDirection: 'row', alignItems: 'center', flexWrap: 'wrap', gap: spacing.sm },
  tile: {
    width: 158,
    gap: 8,
    borderWidth: 1,
    borderColor: colors.border,
    borderRadius: radii.md,
    backgroundColor: colors.white,
    padding: spacing.lg,
  },
  rowCard: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: spacing.md,
    borderWidth: 1,
    borderColor: colors.border,
    borderRadius: radii.md,
    backgroundColor: colors.white,
    padding: spacing.lg,
    minHeight: 78,
  },

  sessionRow: { gap: spacing.sm, minHeight: 88, justifyContent: 'center' },
  sessionAgent: { flexDirection: 'row', alignItems: 'center', gap: spacing.xs },
  sessionMeta: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: spacing.sm,
    marginTop: 2,
  },

  monoBlock: {
    backgroundColor: colors.black,
    borderRadius: radii.sm,
    padding: spacing.md,
    gap: 6,
  },

  permCompact: { flexDirection: 'row', alignItems: 'center', gap: spacing.md, padding: spacing.md },
  permFull: { gap: spacing.md, padding: spacing.lg },
  permHead: { flexDirection: 'row', alignItems: 'center', gap: spacing.md },
  permFoot: { flexDirection: 'row', alignItems: 'center', gap: spacing.sm },

  activityRow: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: spacing.md,
    paddingVertical: spacing.md,
  },
  activityDot: { width: 8, height: 8, borderRadius: 4 },

  composer: {
    gap: spacing.sm,
    borderWidth: 1,
    borderColor: colors.border,
    borderRadius: radii.md,
    backgroundColor: colors.white,
    padding: spacing.md,
  },
  composerRow: { flexDirection: 'row', alignItems: 'center', gap: spacing.sm },
  pressed: { opacity: 0.7 },
  dimmed: { opacity: 0.4 },
  iconButton: {
    minWidth: 36,
    minHeight: 36,
    alignItems: 'center',
    justifyContent: 'center',
    borderRadius: radii.pill,
    borderWidth: StyleSheet.hairlineWidth,
    borderColor: colors.border,
  },
  slashMenu: {
    borderWidth: StyleSheet.hairlineWidth,
    borderColor: colors.border,
    borderRadius: radii.sm,
    paddingVertical: spacing.xs,
  },
  slashRow: { minHeight: 36, justifyContent: 'center', paddingHorizontal: spacing.md },
  thumbRow: { flexDirection: 'row', gap: spacing.sm },
  thumb: { width: 56, height: 56, borderRadius: radii.sm, overflow: 'hidden', backgroundColor: colors.surface2 },
  thumbImage: { width: '100%', height: '100%' },
  thumbRemove: {
    position: 'absolute',
    top: 2,
    right: 2,
    width: 20,
    height: 20,
    borderRadius: radii.pill,
    alignItems: 'center',
    justifyContent: 'center',
    backgroundColor: colors.black,
  },
  cameraScreen: { flex: 1, backgroundColor: colors.black, paddingHorizontal: spacing.xl, gap: spacing.lg },
  cameraHead: { flexDirection: 'row', alignItems: 'center' },
  cameraBox: { flex: 1, borderRadius: radii.md, overflow: 'hidden' },
  cameraAsk: { flex: 1, justifyContent: 'center', gap: spacing.lg },
  shutter: {
    alignSelf: 'center',
    width: 72,
    height: 72,
    borderRadius: radii.pill,
    borderWidth: 4,
    borderColor: colors.text3,
    backgroundColor: colors.white,
    alignItems: 'center',
    justifyContent: 'center',
  },

  timelineRow: { flexDirection: 'row', gap: spacing.md },
  timelineGutter: { width: 16, alignItems: 'center' },
  timelineDot: { width: 14, height: 14, borderRadius: 7, borderWidth: 1.5 },
  timelineLine: { width: 1.5, flex: 1, backgroundColor: colors.border, marginVertical: 3 },

  output: {
    backgroundColor: colors.surface2,
    borderRadius: radii.sm,
    borderWidth: StyleSheet.hairlineWidth,
    borderColor: colors.border,
    padding: spacing.md,
    gap: 5,
  },

  fileRow: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: spacing.sm,
    paddingVertical: spacing.md,
    borderBottomWidth: StyleSheet.hairlineWidth,
    borderBottomColor: colors.border,
  },

  detailRow: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: spacing.sm,
    paddingVertical: spacing.md,
  },

  segment: {
    flexDirection: 'row',
    gap: 4,
    backgroundColor: colors.surface2,
    borderRadius: radii.pill,
    padding: 4,
  },
  segmentItem: {
    flex: 1,
    minHeight: 38,
    borderRadius: radii.pill,
    alignItems: 'center',
    justifyContent: 'center',
    paddingHorizontal: spacing.sm,
  },
  segmentItemActive: { backgroundColor: colors.black },

  metaText: typography.xs,
});
