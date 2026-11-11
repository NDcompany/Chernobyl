/**
 * ADR-015 conversation view: streaming markdown, collapsible tool cards with
 * diffs, approvals inside the card, plan checklist, notices, turn footers,
 * the transcript list, MCP status and the permission-mode picker.
 * Presentational only: data and callbacks come in as props, decisions go
 * out through `onDecide`. Approval buttons are drawn only from the
 * companion's PermissionRequest record, never from transcript text (§6 T3).
 * Pure logic lives in `@/utils/transcript` (unit-tested).
 */
import { memo, useCallback, useEffect, useMemo, useRef, useState } from 'react';
import type { Dispatch, ReactNode, SetStateAction } from 'react';
import {
  AccessibilityInfo,
  ActivityIndicator,
  FlatList,
  KeyboardAvoidingView,
  Modal,
  Platform,
  Pressable,
  ScrollView,
  StyleSheet,
  Text as RNText,
  TextInput,
  View,
} from 'react-native';
import type {
  ListRenderItemInfo,
  NativeScrollEvent,
  NativeSyntheticEvent,
  StyleProp,
  TextStyle,
  ViewStyle,
} from 'react-native';
import { useSafeAreaInsets } from 'react-native-safe-area-context';
import type {
  AgentKind,
  FileDiff,
  McpServerState,
  McpStatus,
  PermissionDetail,
  PermissionRequest,
  PlanStep,
  ToolStatus,
  TranscriptItem,
  TurnState,
} from '@chernobyl/protocol';
import { colors, radii, shadow, spacing, typography } from '@/theme/tokens';
import { Button, StatusDot, Text, type Tone } from '@/ui/primitives';
import { ConfirmModal } from '@/ui/surface';
import { MonoBlock } from '@/ui/lists';
import { Icon, type IconName } from '@/ui/icons';
import { AgentMark } from '@/ui/agentMark';
import { useNow } from '@/hooks/useReducedMotion';
import {
  formatElapsed,
  workingA11yLabel,
  workingLabel,
  workingStatus,
  type WorkingStatus,
} from '@/utils/agentIdentity';
import {
  DIFF_PREVIEW_LINES,
  agentShortName,
  approvalButtons,
  buildTranscript,
  collectAnswers,
  diffCounts,
  formatCost,
  formatTokens,
  isCallbackUrl,
  isModeChoice,
  mcpSummary,
  modeLabel,
  parseDiff,
  parseMarkdown,
  replyStarts,
  rowIndexFor,
  turnFooter,
  type ApprovalChoice,
  type ApprovalDecision,
  type DiffLineKind,
  type MdBlock,
  type MdSpan,
  type PermissionModeChoice,
  type TranscriptRow,
} from '@/utils/transcript';

export type { ApprovalDecision, PermissionModeChoice } from '@/utils/transcript';

type ToolItem = Extract<TranscriptItem, { kind: 'tool' }>;
type PromptWhileBusy = 'merge' | 'queue' | 'none';
type Decide = (decision: ApprovalDecision) => void | Promise<unknown>;

/** claude.ai connectors are managed on the web; the phone opens this page, nothing else (§4.7). */
const CLAUDE_AI_CONNECTORS = 'https://claude.ai/customize/connectors';

/* ------------------------------------------------------------------ */
/* MessageText: markdown subset                                        */
/* ------------------------------------------------------------------ */

export interface MessageTextProps {
  text: string;
  /** the item is still streaming (`done === false`): unclosed syntax renders as if closed */
  streaming?: boolean;
  /** opens a URL after the user confirmed it; without it links are plain text */
  onOpenLink?: (url: string) => void;
}

/** No HTML, no images, no remote loads. A link opens only on tap, after a confirm showing its URL. */
/**
 * Appended to each line of mono text in a sideways ScrollView. Android sizes such a Text to its
 * measured width, which pixel rounding can leave a pixel short: the longest line then wraps its
 * last word and pushes the final line out of the clipped box (a one-line Write lost its `+hi`).
 * A trailing space counts in the measured width but may overhang when the line is laid out.
 */
const NO_WRAP_SLACK = ' ';

export const MessageText = memo(function MessageText({ text, streaming = false, onOpenLink }: MessageTextProps) {
  const blocks = useMemo(() => parseMarkdown(text, streaming), [text, streaming]);
  const [link, setLink] = useState<string | null>(null);
  return (
    <View style={styles.message}>
      {blocks.map((block, i) => (
        <MdBlockView key={i} block={block} streaming={streaming} onLink={onOpenLink ? setLink : undefined} />
      ))}
      {link ? (
        <ConfirmModal
          visible
          title="Open link?"
          message={link}
          note="This link comes from the agent's output. Check the full address before you open it."
          confirmLabel="Open link"
          onConfirm={() => {
            setLink(null);
            onOpenLink?.(link);
          }}
          onCancel={() => setLink(null)}
        />
      ) : null}
    </View>
  );
});

interface MdBlockViewProps {
  block: MdBlock;
  /** only for the memo check: the same source parses differently once streaming ends */
  streaming: boolean;
  onLink?: (url: string) => void;
}

const MdBlockView = memo(
  function MdBlockView({ block, onLink }: MdBlockViewProps) {
    switch (block.kind) {
      case 'heading':
        return (
          <RNText
            accessibilityRole="header"
            style={[block.level === 1 ? typography.h2 : block.level === 2 ? typography.h3 : typography.bodyStrong, styles.ink]}
          >
            {renderSpans(block.spans, onLink)}
          </RNText>
        );
      case 'paragraph':
        return (
          <RNText selectable={!block.spans.some((s) => s.url)} style={[typography.body, styles.ink]}>
            {renderSpans(block.spans, onLink)}
          </RNText>
        );
      case 'list':
        return (
          <View style={styles.list}>
            {block.items.map((it, i) => (
              <View key={i} style={[styles.listItem, { paddingLeft: it.depth * spacing.lg }]}>
                <RNText style={[typography.body, styles.ink, styles.marker]}>{it.marker}</RNText>
                <RNText selectable={!it.spans.some((s) => s.url)} style={[typography.body, styles.ink, styles.flex]}>
                  {renderSpans(it.spans, onLink)}
                </RNText>
              </View>
            ))}
          </View>
        );
      case 'code':
        return (
          <View style={styles.codeBlock}>
            {block.lang ? (
              <Text variant="xs" color={colors.text3}>
                {block.lang}
              </Text>
            ) : null}
            <ScrollView horizontal showsHorizontalScrollIndicator={false} nestedScrollEnabled>
              <Text variant="mono" color={colors.white} selectable>
                {block.text.replace(/$/gm, NO_WRAP_SLACK)}
              </Text>
            </ScrollView>
          </View>
        );
      case 'rule':
        return <View style={styles.rule} />;
      default:
        return null;
    }
  },
  (a, b) => a.block.src === b.block.src && a.streaming === b.streaming && a.onLink === b.onLink,
);

function renderSpans(spans: MdSpan[], onLink?: (url: string) => void): ReactNode[] {
  return spans.map((s, i) => {
    const style: TextStyle[] = [];
    if (s.bold) style.push(styles.bold);
    if (s.italic) style.push(styles.italic);
    if (s.code) style.push(styles.inlineCode);
    if (s.url) style.push(styles.link);
    const url = s.url;
    if (url && onLink) {
      return (
        <RNText key={i} style={style} onPress={() => onLink(url)} accessibilityRole="link" accessibilityLabel={`Link: ${url}`}>
          {s.text}
        </RNText>
      );
    }
    return style.length ? (
      <RNText key={i} style={style}>
        {s.text}
      </RNText>
    ) : (
      s.text
    );
  });
}

/* ------------------------------------------------------------------ */
/* User bubble, thinking, notice, footer, plan                         */
/* ------------------------------------------------------------------ */

export interface UserBubbleProps {
  text: string;
  /** `TranscriptItem.queued`: sent while a turn was running */
  queued?: boolean;
  /** `Agent.live.promptWhileBusy`: 'merge' says the prompt joins the current turn */
  promptWhileBusy?: PromptWhileBusy;
}

export const UserBubble = memo(function UserBubble({ text, queued, promptWhileBusy }: UserBubbleProps) {
  const caption = queued ? (promptWhileBusy === 'merge' ? 'Joins the current turn' : 'Queued') : undefined;
  return (
    <View style={styles.user}>
      <Text variant="body" selectable>
        {text}
      </Text>
      {caption ? (
        <Text variant="xs" color={colors.text3}>
          {caption}
        </Text>
      ) : null}
    </View>
  );
});

export interface ThinkingRowProps {
  text: string;
  done: boolean;
}

/** Collapsed "Thinking…" with a one-line preview; expands to the summarized thinking. */
export const ThinkingRow = memo(function ThinkingRow({ text, done }: ThinkingRowProps) {
  const [open, setOpen] = useState(false);
  const body = text.trimEnd();
  const label = done ? 'Thought' : 'Thinking…';
  return (
    <View style={styles.gapXs}>
      <Pressable
        onPress={() => setOpen((v) => !v)}
        accessibilityRole="button"
        accessibilityLabel={label}
        accessibilityState={{ expanded: open }}
        style={styles.thinkingRow}
      >
        <Icon name="thinking" size={16} color={colors.text3} />
        <Text variant="smStrong" color={colors.text2}>
          {label}
        </Text>
        <Text variant="sm" color={colors.text3} numberOfLines={1} style={styles.flex}>
          {open ? '' : body.slice(body.lastIndexOf('\n') + 1)}
        </Text>
        <Icon name={open ? 'chevron-down' : 'chevron-right'} size={14} color={colors.text3} />
      </Pressable>
      {open ? (
        <Text variant="sm" color={colors.text2} style={styles.italic} selectable>
          {body}
        </Text>
      ) : null}
    </View>
  );
});

export interface NoticeRowProps {
  level: 'info' | 'warning' | 'error';
  text: string;
}

export const NoticeRow = memo(function NoticeRow({ level, text }: NoticeRowProps) {
  return (
    <Text
      variant="xs"
      center
      color={level === 'error' ? colors.error : level === 'warning' ? colors.warning : colors.text3}
      style={styles.notice}
    >
      {text}
    </Text>
  );
});

/** "Done · 12.4 s · 4.2k tokens · $0.01", "Stopped" or the error; nothing while the turn runs. */
export const TurnFooter = memo(function TurnFooter({ turn }: { turn: TurnState }) {
  const footer = turnFooter(turn);
  if (!footer) return null;
  return (
    <View style={styles.footer} accessible accessibilityLabel={footer.text}>
      <View style={styles.footerLine} />
      <Text variant="xs" color={footer.tone === 'error' ? colors.error : colors.text3} numberOfLines={3} style={styles.footerText}>
        {footer.text}
      </Text>
      <View style={styles.footerLine} />
    </View>
  );
});

const STEP_LABEL: Record<PlanStep['status'], string> = {
  pending: 'pending',
  in_progress: 'in progress',
  completed: 'done',
  cancelled: 'cancelled',
};

/** Checklist; the companion upserts the same item, so it updates in place. */
export const PlanCard = memo(function PlanCard({ steps }: { steps: PlanStep[] }) {
  const done = steps.filter((s) => s.status === 'completed').length;
  return (
    <View style={styles.plan}>
      <View style={styles.planHead}>
        <Text variant="eyebrow" color={colors.text3} accessibilityRole="header">
          Plan
        </Text>
        <Text variant="xs" color={colors.text3}>
          {done}/{steps.length}
        </Text>
      </View>
      {steps.map((s, i) => {
        const settled = s.status === 'completed' || s.status === 'cancelled';
        return (
          <View key={i} style={styles.step} accessible accessibilityLabel={`${s.text}: ${STEP_LABEL[s.status] ?? s.status}`}>
            <View
              style={[
                styles.stepMark,
                s.status === 'completed' && styles.stepDone,
                s.status === 'in_progress' && styles.stepActive,
              ]}
            >
              {s.status === 'completed' ? <Icon name="check" size={10} color={colors.white} strokeWidth={2.4} /> : null}
              {s.status === 'cancelled' ? <Icon name="x" size={10} color={colors.text3} strokeWidth={2} /> : null}
            </View>
            <Text
              variant={s.status === 'in_progress' ? 'smStrong' : 'sm'}
              color={settled ? colors.text3 : colors.text}
              style={[styles.flex, settled && styles.strike]}
            >
              {s.text}
            </Text>
          </View>
        );
      })}
    </View>
  );
});

/* ------------------------------------------------------------------ */
/* DiffView                                                            */
/* ------------------------------------------------------------------ */

const DIFF_COLOR: Record<DiffLineKind, string> = {
  add: colors.success,
  del: colors.error,
  hunk: colors.text3,
  meta: colors.text3,
  context: colors.text,
};

const FILE_STATUS: Record<FileDiff['status'], string> = {
  added: 'Added',
  modified: 'Modified',
  deleted: 'Deleted',
  renamed: 'Renamed',
};

export interface DiffViewProps {
  /** a FileDiff; status and counts are optional (an edit approval only has path + patch) */
  diff: Pick<FileDiff, 'path' | 'patch'> & Partial<Omit<FileDiff, 'path' | 'patch'>>;
}

/** Unified diff: +/- in the state colors, hunk headers in text3, long lines scroll sideways. */
export const DiffView = memo(function DiffView({ diff }: DiffViewProps) {
  const lines = useMemo(() => parseDiff(diff.patch), [diff.patch]);
  const [all, setAll] = useState(false);
  const counted = diffCounts(lines);
  const additions = diff.additions ?? counted.additions;
  const deletions = diff.deletions ?? counted.deletions;
  const status = diff.status ? (FILE_STATUS[diff.status] ?? diff.status) : undefined;
  const shown = all ? lines : lines.slice(0, DIFF_PREVIEW_LINES);
  const hidden = lines.length - shown.length;
  return (
    <View style={styles.diff}>
      <View
        style={styles.diffHead}
        accessible
        accessibilityLabel={`Diff ${diff.path}${status ? `, ${status}` : ''}: ${additions} added, ${deletions} removed`}
      >
        <Icon name="diff" size={14} color={colors.text2} />
        <Text variant="mono" numberOfLines={1} style={styles.flex}>
          {diff.path}
        </Text>
        {status ? (
          <Text variant="xs" color={colors.text3}>
            {status}
          </Text>
        ) : null}
        <Text variant="mono" color={colors.success}>
          +{additions}
        </Text>
        <Text variant="mono" color={colors.error}>
          −{deletions}
        </Text>
      </View>
      {shown.length ? (
        <ScrollView horizontal showsHorizontalScrollIndicator={false} nestedScrollEnabled style={styles.diffBody}>
          <Text variant="mono" selectable>
            {shown.map((l, i) => (
              <RNText key={i} style={{ color: DIFF_COLOR[l.kind] }}>
                {`${l.text}${NO_WRAP_SLACK}${i < shown.length - 1 ? '\n' : ''}`}
              </RNText>
            ))}
          </Text>
        </ScrollView>
      ) : null}
      {hidden > 0 ? (
        <Pressable
          onPress={() => setAll(true)}
          accessibilityRole="button"
          accessibilityLabel={`Show more: ${hidden} more lines`}
          style={styles.showMore}
        >
          <Text variant="smStrong">Show more ({hidden} lines)</Text>
        </Pressable>
      ) : null}
      {diff.truncated ? (
        <Text variant="xs" color={colors.text3} style={styles.diffNote}>
          Diff truncated; the full change is on the PC
        </Text>
      ) : null}
    </View>
  );
});

/* ------------------------------------------------------------------ */
/* Approvals                                                           */
/* ------------------------------------------------------------------ */

export interface ApprovalProps {
  /** the pending snapshot record; its `options` (or legacy `supportedDecisions`) are the only buttons */
  permission: PermissionRequest;
  onDecide: Decide;
  /** names the agent in "Deny and tell Claude why" */
  agentKind?: AgentKind;
  /** for links inside a plan */
  onOpenLink?: (url: string) => void;
}

/**
 * The approval inside a tool card: the request's detail, a primary
 * "Allow", a secondary "Deny", and "More…" for session and persistent
 * grants (persistent ones confirm the exact rule and file first) and
 * "Deny and tell <agent> why". Sends the record's revision.
 */
export function InlineApproval({ permission, onDecide, agentKind, onOpenLink }: ApprovalProps) {
  const agent = agentShortName(agentKind);
  const buttons = useMemo(() => approvalButtons(permission, agent), [permission, agent]);
  const questions = permission.detail?.kind === 'question' ? permission.detail.questions : undefined;
  const [chosen, setChosen] = useState<Record<string, string[]>>({});
  const [free, setFree] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState<string | null>(null);
  const [showMore, setShowMore] = useState(false);
  const [confirm, setConfirm] = useState<ApprovalChoice | null>(null);
  const [why, setWhy] = useState<ApprovalChoice | null>(null);
  const [reason, setReason] = useState('');

  const answers = questions ? collectAnswers(questions, chosen, free) : undefined;
  const answered = !answers || Object.values(answers).every((a) => a.length > 0);

  const send = (c: ApprovalChoice, message?: string) => {
    const d: ApprovalDecision = { permissionId: permission.id, decision: c.decision, revision: permission.revision };
    if (c.optionId) d.optionId = c.optionId;
    if (message) d.message = message;
    if (answers && c.decision !== 'reject') d.answers = answers;
    setBusy(c.key);
    Promise.resolve()
      .then(() => onDecide(d))
      .catch(() => undefined)
      .finally(() => setBusy(null));
  };
  const choose = (c: ApprovalChoice) => {
    if (c.scope === 'persistent') setConfirm(c);
    else if (c.withMessage) {
      setShowMore(false);
      setWhy(c);
    } else send(c);
  };
  const blocked = (c: ApprovalChoice) => busy !== null || (c.decision !== 'reject' && !answered);
  const { primary, secondary, more } = buttons;

  return (
    <View style={styles.approval}>
      <Text variant="eyebrow" color={colors.text3}>
        Approval needed
      </Text>
      <ApprovalDetail
        permission={permission}
        onOpenLink={onOpenLink}
        chosen={chosen}
        setChosen={setChosen}
        free={free}
        setFree={setFree}
      />
      {why ? (
        <View style={styles.gapSm}>
          <Text variant="smStrong">Tell {agent} why</Text>
          <TextInput
            value={reason}
            onChangeText={setReason}
            placeholder={`What should ${agent} do instead?`}
            placeholderTextColor={colors.text3}
            multiline
            maxLength={4096}
            accessibilityLabel={`Reason for ${agent}`}
            style={styles.input}
          />
          <View style={styles.buttonRow}>
            <Button
              label="Send"
              size="sm"
              fullWidth={false}
              loading={busy === why.key}
              disabled={busy !== null || !reason.trim()}
              onPress={() => send(why, reason.trim())}
              accessibilityLabel="Send reason"
              style={styles.flex}
            />
            <Button
              label="Cancel"
              size="sm"
              variant="secondary"
              fullWidth={false}
              disabled={busy !== null}
              onPress={() => setWhy(null)}
              style={styles.flex}
            />
          </View>
        </View>
      ) : (
        <>
          <View style={styles.buttonRow}>
            {primary ? (
              <Button
                label={primary.label}
                loading={busy === primary.key}
                disabled={blocked(primary)}
                onPress={() => choose(primary)}
                fullWidth={false}
                style={styles.flex}
              />
            ) : null}
            {secondary ? (
              <Button
                label={secondary.label}
                variant="secondary"
                loading={busy === secondary.key}
                disabled={blocked(secondary)}
                onPress={() => choose(secondary)}
                fullWidth={false}
                style={styles.flex}
              />
            ) : null}
          </View>
          {more.length ? (
            <Pressable
              onPress={() => setShowMore((v) => !v)}
              accessibilityRole="button"
              accessibilityLabel="More options"
              accessibilityState={{ expanded: showMore }}
              style={styles.moreToggle}
            >
              <Text variant="smStrong" color={colors.text2}>
                More…
              </Text>
              <Icon name={showMore ? 'chevron-down' : 'chevron-right'} size={14} color={colors.text3} />
            </Pressable>
          ) : null}
          {showMore
            ? more.map((c) => (
                <View key={c.key} style={styles.gapXs}>
                  <Button
                    label={c.label}
                    size="sm"
                    variant="secondary"
                    loading={busy === c.key}
                    disabled={blocked(c)}
                    onPress={() => choose(c)}
                  />
                  {c.scope === 'persistent' ? (
                    <Text variant="xs" color={colors.text3}>
                      Saves a rule{c.destination ? ` to ${c.destination}` : ''} on the PC
                    </Text>
                  ) : null}
                  {c.stopsTurn ? (
                    <Text variant="xs" color={colors.text3}>
                      Also ends the current turn
                    </Text>
                  ) : null}
                </View>
              ))
            : null}
        </>
      )}
      {confirm ? (
        <ConfirmModal
          visible
          title={confirm.label}
          message={`Rule: ${confirm.rule ?? '(none given)'}\nFile: ${confirm.destination ?? '(not given)'}`}
          note="The rule is saved on your PC and applies to future sessions too, not only to this request."
          confirmLabel={confirm.decision === 'reject' ? 'Save rule and deny' : 'Save rule and allow'}
          onConfirm={() => {
            setConfirm(null);
            send(confirm);
          }}
          onCancel={() => setConfirm(null)}
        />
      ) : null}
    </View>
  );
}

type Questions = Extract<PermissionDetail, { kind: 'question' }>['questions'];

interface ApprovalDetailProps {
  permission: PermissionRequest;
  onOpenLink?: (url: string) => void;
  chosen: Record<string, string[]>;
  setChosen: Dispatch<SetStateAction<Record<string, string[]>>>;
  free: Record<string, string>;
  setFree: Dispatch<SetStateAction<Record<string, string>>>;
}

/** What the agent wants to do, by detail kind; legacy records fall back to command + context. */
function ApprovalDetail({ permission, onOpenLink, ...answers }: ApprovalDetailProps) {
  const d = permission.detail;
  switch (d?.kind) {
    case 'shell':
      return (
        <View style={styles.gapSm}>
          <MonoBlock caption="Command" text={d.command} />
          {d.cwd ? <PathLine icon="folder" text={d.cwd} /> : null}
          {d.reason ? (
            <Text variant="sm" color={colors.text2}>
              {d.reason}
            </Text>
          ) : null}
        </View>
      );
    case 'edit':
      return d.patch ? <DiffView diff={{ path: d.path, patch: d.patch }} /> : <PathLine icon="file" text={d.path} />;
    case 'read':
      return <PathLine icon="file" text={d.path} />;
    case 'fetch':
      return <MonoBlock caption="URL" text={d.url} />;
    case 'mcp':
      return (
        <View style={styles.gapSm}>
          <Text variant="sm" color={colors.text2}>
            MCP server{' '}
            <Text variant="smStrong" color={colors.text}>
              {d.server}
            </Text>
            {' · tool '}
            <Text variant="smStrong" color={colors.text}>
              {d.tool}
            </Text>
          </Text>
          {d.input ? <MonoBlock caption="Input" text={d.input} /> : null}
        </View>
      );
    case 'question':
      return <QuestionsView questions={d.questions} {...answers} />;
    case 'plan':
      return (
        <View style={styles.planBox}>
          <MessageText text={d.plan} onOpenLink={onOpenLink} />
        </View>
      );
    case 'generic':
      return (
        <View style={styles.gapSm}>
          <Text variant="smStrong">{d.tool}</Text>
          {d.input ? <MonoBlock caption="Input" text={d.input} /> : null}
        </View>
      );
    default:
      return (
        <View style={styles.gapSm}>
          <MonoBlock caption={permission.title} text={permission.command} />
          {permission.context ? (
            <Text variant="sm" color={colors.text2}>
              {permission.context}
            </Text>
          ) : null}
        </View>
      );
  }
}

function QuestionsView({
  questions,
  chosen,
  setChosen,
  free,
  setFree,
}: Omit<ApprovalDetailProps, 'permission' | 'onOpenLink'> & { questions: Questions }) {
  return (
    <View style={styles.gapMd}>
      {questions.map((q) => {
        const picked = chosen[q.id] ?? [];
        const pick = (label: string) => {
          setChosen((c) => {
            const cur = c[q.id] ?? [];
            const on = cur.includes(label);
            const next = q.multiSelect ? (on ? cur.filter((l) => l !== label) : [...cur, label]) : on ? [] : [label];
            return { ...c, [q.id]: next };
          });
          if (!q.multiSelect) setFree((f) => ({ ...f, [q.id]: '' })); // one answer: a chip or free text
        };
        return (
          <View key={q.id} style={styles.gapSm}>
            {q.header ? (
              <Text variant="eyebrow" color={colors.text3}>
                {q.header}
              </Text>
            ) : null}
            <Text variant="bodyStrong">{q.text}</Text>
            <View style={styles.chips}>
              {q.options.map((o) => {
                const on = picked.includes(o.label);
                return (
                  <Pressable
                    key={o.label}
                    onPress={() => pick(o.label)}
                    accessibilityRole={q.multiSelect ? 'checkbox' : 'radio'}
                    accessibilityState={{ checked: on }}
                    accessibilityLabel={o.description ? `${o.label}: ${o.description}` : o.label}
                    style={[styles.option, on && styles.optionOn]}
                  >
                    <Text variant="smStrong" color={on ? colors.white : colors.text}>
                      {o.label}
                    </Text>
                    {o.description ? (
                      <Text variant="xs" color={on ? colors.border : colors.text2}>
                        {o.description}
                      </Text>
                    ) : null}
                  </Pressable>
                );
              })}
            </View>
            {q.allowFreeText ? (
              <TextInput
                value={free[q.id] ?? ''}
                onChangeText={(t) => {
                  setFree((f) => ({ ...f, [q.id]: t }));
                  if (!q.multiSelect && t) setChosen((c) => ({ ...c, [q.id]: [] }));
                }}
                placeholder="Other answer…"
                placeholderTextColor={colors.text3}
                maxLength={1024}
                accessibilityLabel={`Other answer: ${q.text}`}
                style={styles.input}
              />
            ) : null}
          </View>
        );
      })}
    </View>
  );
}

function PathLine({ icon, text }: { icon: IconName; text: string }) {
  return (
    <View style={styles.pathLine}>
      <Icon name={icon} size={14} color={colors.text3} />
      <Text variant="mono" color={colors.text2} selectable style={styles.flex}>
        {text}
      </Text>
    </View>
  );
}

/** A pending request whose tool item is not in view (trimmed or not streamed yet): never invisible. */
export function StandaloneApproval(props: ApprovalProps) {
  return (
    <View style={[styles.toolCard, styles.toolCardStrong]}>
      <View style={styles.toolHead}>
        <Icon name="shield-check" size={16} color={colors.warning} />
        <Text variant="smStrong" numberOfLines={2} style={styles.flex}>
          {props.permission.title}
        </Text>
      </View>
      <View style={styles.toolBody}>
        <InlineApproval {...props} />
      </View>
    </View>
  );
}

/* ------------------------------------------------------------------ */
/* ToolCard                                                            */
/* ------------------------------------------------------------------ */

const TOOL_STATUS: Record<ToolStatus, { color: string; icon?: IconName; word?: string }> = {
  pending: { color: colors.text3, icon: 'clock' },
  running: { color: colors.black },
  'awaiting-approval': { color: colors.warning, icon: 'shield-check', word: 'Approval' },
  completed: { color: colors.success, icon: 'check' },
  failed: { color: colors.error, icon: 'x', word: 'Failed' },
  denied: { color: colors.text2, icon: 'lock', word: 'Denied' },
  cancelled: { color: colors.text2, icon: 'stop', word: 'Stopped' },
};

function toolIcon(name: string): IconName {
  const n = name.toLowerCase();
  if (n.startsWith('mcp__') || n.includes('/')) return 'plug';
  if (/bash|shell|exec|command|terminal/.test(n)) return 'terminal';
  if (/edit|write|patch|notebook/.test(n)) return 'diff';
  if (/web|fetch|url|browser/.test(n)) return 'globe';
  if (/grep|glob|search|find|^ls$/.test(n)) return 'search';
  if (/^(read|view|cat)/.test(n)) return 'file';
  if (/task|agent/.test(n)) return 'bot';
  if (/todo/.test(n)) return 'check';
  return 'code';
}

export interface ToolCardProps {
  item: ToolItem;
  /** the pending request with `toolItemId === item.id`: outlines the card, opens it, shows the approval */
  permission?: PermissionRequest;
  onDecide?: Decide;
  onOpenLink?: (url: string) => void;
  agentKind?: AgentKind;
  /** a nested item awaits approval: keep the card open */
  attention?: boolean;
  /** subagent work under a Task tool */
  childItems?: TranscriptItem[];
  renderChild?: (item: TranscriptItem) => ReactNode;
}

/** Collapsible tool call: name verbatim, one-line title, a status mark for every ToolStatus. */
export const ToolCard = memo(function ToolCard({
  item,
  permission,
  onDecide,
  onOpenLink,
  agentKind,
  attention = false,
  childItems,
  renderChild,
}: ToolCardProps) {
  const tool = item.tool;
  const [open, setOpen] = useState(false);
  const approval = permission?.status === 'pending' && onDecide ? permission : undefined;
  // a pending approval (here or nested) keeps the card open
  const expanded = open || !!approval || attention;
  const mark = TOOL_STATUS[tool.status] ?? TOOL_STATUS.pending;
  return (
    <View style={[styles.toolCard, approval && styles.toolCardStrong]}>
      <Pressable
        onPress={() => setOpen((v) => !v)}
        accessibilityRole="button"
        accessibilityLabel={`Tool ${tool.name}: ${tool.status.replace('-', ' ')}`}
        accessibilityState={{ expanded }}
        style={styles.toolHead}
      >
        <Icon name={toolIcon(tool.name)} size={16} color={colors.text2} />
        <Text variant="smStrong" numberOfLines={1} style={styles.toolName}>
          {tool.name}
        </Text>
        <Text variant="mono" color={colors.text2} numberOfLines={1} style={styles.flex}>
          {tool.title ?? ''}
        </Text>
        {tool.status === 'running' ? <ActivityIndicator size="small" color={colors.black} /> : null}
        {mark.icon ? <Icon name={mark.icon} size={15} color={mark.color} strokeWidth={2} /> : null}
        {mark.word ? (
          <Text variant="xs" color={mark.color}>
            {mark.word}
          </Text>
        ) : null}
        <Icon name={expanded ? 'chevron-down' : 'chevron-right'} size={14} color={colors.text3} />
      </Pressable>
      {expanded ? (
        <View style={styles.toolBody}>
          {approval && onDecide ? (
            <InlineApproval permission={approval} onDecide={onDecide} agentKind={agentKind} onOpenLink={onOpenLink} />
          ) : (
            <>
              {tool.input ? <MonoBlock caption="Input" text={tool.input} /> : null}
              {tool.diffs?.map((d, i) => <DiffView key={`${d.path}:${i}`} diff={d} />)}
              {tool.output ? (
                <View style={styles.output}>
                  <Text variant="mono" selectable>
                    {tool.output}
                  </Text>
                  {tool.outputTruncated ? (
                    <Text variant="xs" color={colors.text3}>
                      Output truncated: the start and the end are shown
                    </Text>
                  ) : null}
                </View>
              ) : null}
              {tool.error ? (
                <Text variant="sm" color={colors.error} selectable>
                  {tool.error}
                </Text>
              ) : null}
              {tool.autoApproved ? (
                <Text variant="xs" color={colors.text3}>
                  Auto-approved ({tool.autoApproved})
                </Text>
              ) : null}
              {childItems?.length && renderChild ? (
                <View style={styles.children}>
                  {childItems.map((c) => (
                    <View key={c.id}>{renderChild(c)}</View>
                  ))}
                </View>
              ) : null}
            </>
          )}
        </View>
      ) : null}
      {!approval && tool.status === 'awaiting-approval' ? (
        <Text variant="sm" color={colors.text2} style={styles.waiting}>
          Waiting for approval…
        </Text>
      ) : null}
    </View>
  );
});

/* ------------------------------------------------------------------ */
/* TranscriptList                                                      */
/* ------------------------------------------------------------------ */

export interface TranscriptListProps {
  items: TranscriptItem[];
  turns: TurnState[];
  /** pending PermissionRequests of this session (snapshot records) */
  permissions: PermissionRequest[];
  /** pass stable callbacks (useCallback) so streaming re-renders only the changed rows */
  onDecide: Decide;
  /** called after the user confirmed a link's full URL */
  onOpenLink: (url: string) => void;
  /** `Agent.live.promptWhileBusy`: picks the queued prompt's caption */
  promptWhileBusy?: PromptWhileBusy;
  /** names the agent in "Deny and tell Claude why" and picks its `AgentMark` */
  agentKind?: AgentKind;
  /** `Agent.name`: heads each reply and the working row (default: the kind's short name) */
  agentName?: string;
  /** snapshot `truncatedBefore`: older items exist only on the PC */
  truncatedBefore?: boolean;
  /** deep link (Approvals tab, notification): scroll this item's card into view once it is there, and highlight it briefly */
  focusItemId?: string;
  style?: StyleProp<ViewStyle>;
}

export interface WorkingRowProps {
  state: WorkingStatus['state'];
  /** the running turn: id seeds the verbs, startedAt drives the counter */
  turnId: string;
  startedAt: number;
  tool?: string;
  /** mid-turn usage when the runtime reports it */
  tokens?: number;
  costUsd?: number;
  agentKind?: AgentKind;
  agentName: string;
}

/**
 * The live "working" line under a running turn: the animated mark, a status
 * verb that rotates every few seconds (or the running tool, or "Waiting for
 * you"), elapsed time and live usage. Ticks on its own once a second, so the
 * transcript rows never re-render for it. Screen readers get one polite live
 * region whose label changes only between working and waiting.
 */
export const WorkingRow = memo(function WorkingRow({
  state,
  turnId,
  startedAt,
  tool,
  tokens,
  costUsd,
  agentKind,
  agentName,
}: WorkingRowProps) {
  const now = useNow(1000);
  const elapsed = now - startedAt;
  const a11y = workingA11yLabel(state, agentName);
  useEffect(() => {
    // iOS has no live regions; Android's polite region below speaks the same label
    if (Platform.OS === 'ios') AccessibilityInfo.announceForAccessibility(a11y);
  }, [a11y]);
  const meta = [formatElapsed(elapsed)];
  if (tokens) meta.push(formatTokens(tokens));
  if (costUsd != null) meta.push(formatCost(costUsd));
  return (
    <View style={styles.working} accessible accessibilityLabel={a11y} accessibilityLiveRegion="polite">
      <AgentMark kind={agentKind} size={24} state={state} />
      <Text variant="smStrong" color={state === 'waiting' ? colors.text : colors.text2} numberOfLines={1} style={styles.flexShrink}>
        {workingLabel(state, elapsed, turnId, tool)}
      </Text>
      <Text variant="xs" color={colors.text3} numberOfLines={1}>
        {meta.join(' · ')}
      </Text>
    </View>
  );
});

const FLASH_MS = 1600;
const FOCUS_POSITION = 0.3; // the focused card sits in the upper third

interface ItemViewProps {
  item: TranscriptItem;
  approval?: PermissionRequest;
  attention: boolean;
  childItems?: TranscriptItem[];
  renderChild?: (item: TranscriptItem) => ReactNode;
  onDecide: Decide;
  onOpenLink: (url: string) => void;
  agentKind?: AgentKind;
  promptWhileBusy?: PromptWhileBusy;
  /** the first agent item of a reply: carries the agent's mark and name */
  replyStart?: boolean;
  agentName?: string;
}

const ItemView = memo(function ItemView({ replyStart, agentName, ...p }: ItemViewProps) {
  if (!replyStart) return itemBody(p);
  return (
    <View style={styles.gapSm}>
      <View style={styles.replyHead}>
        <AgentMark kind={p.agentKind} size={16} />
        <Text variant="smStrong" color={colors.text2} numberOfLines={1}>
          {agentName}
        </Text>
      </View>
      {itemBody(p)}
    </View>
  );
});

function itemBody({ item, ...p }: ItemViewProps): ReactNode {
  switch (item.kind) {
    case 'user':
      return <UserBubble text={item.text} queued={item.queued} promptWhileBusy={p.promptWhileBusy} />;
    case 'text':
      return (
        <View style={styles.gapXs}>
          <MessageText text={item.text} streaming={!item.done} onOpenLink={p.onOpenLink} />
          {item.truncated ? (
            <Text variant="xs" color={colors.text3}>
              Message truncated; the full text is on the PC
            </Text>
          ) : null}
        </View>
      );
    case 'thinking':
      return <ThinkingRow text={item.text} done={item.done} />;
    case 'tool':
      return (
        <ToolCard
          item={item}
          permission={p.approval}
          onDecide={p.onDecide}
          onOpenLink={p.onOpenLink}
          agentKind={p.agentKind}
          attention={p.attention}
          childItems={p.childItems}
          renderChild={p.renderChild}
        />
      );
    case 'plan':
      return <PlanCard steps={item.steps} />;
    case 'notice':
      return <NoticeRow level={item.level} text={item.text} />;
    default:
      return null;
  }
}

const rowKey = (row: TranscriptRow) => row.key;

/**
 * The conversation: items in order with a footer after each ended turn and
 * standalone cards for approvals whose tool item is not in view. Sticks to
 * the bottom while the user is there, otherwise offers "Jump to latest".
 * Screen readers hear each turn's end once, never the streaming deltas.
 */
export function TranscriptList({
  items,
  turns,
  permissions,
  onDecide,
  onOpenLink,
  promptWhileBusy,
  agentKind,
  agentName = agentShortName(agentKind),
  truncatedBefore,
  focusItemId,
  style,
}: TranscriptListProps) {
  const model = useMemo(() => buildTranscript(items, turns, permissions), [items, turns, permissions]);
  const starts = useMemo(() => replyStarts(model.rows), [model.rows]);
  const working = useMemo(() => workingStatus(items, turns, permissions), [items, turns, permissions]);
  const listRef = useRef<FlatList<TranscriptRow>>(null);
  const atBottom = useRef(true);
  const [showJump, setShowJump] = useState(false);
  const [flash, setFlash] = useState<string | null>(null);
  const focused = useRef<string | undefined>(undefined);
  const focusRetries = useRef(0);

  const focusIndex = useMemo(() => (focusItemId ? rowIndexFor(model, focusItemId) : -1), [model, focusItemId]);
  useEffect(() => {
    if (focusIndex < 0 || focused.current === focusItemId) return;
    focused.current = focusItemId;
    focusRetries.current = 0;
    atBottom.current = false; // the stick-to-bottom must not pull it away again
    setShowJump(true);
    setFlash(model.rows[focusIndex].key);
    listRef.current?.scrollToIndex({ index: focusIndex, animated: true, viewPosition: FOCUS_POSITION });
  }, [focusIndex, focusItemId, model.rows]);
  useEffect(() => {
    if (!flash) return;
    const t = setTimeout(() => setFlash(null), FLASH_MS);
    return () => clearTimeout(t);
  }, [flash]);
  const onScrollToIndexFailed = useCallback(({ index, averageItemLength }: { index: number; averageItemLength: number }) => {
    // not laid out yet: jump near it, then try again (a few times at most)
    listRef.current?.scrollToOffset({ offset: averageItemLength * index, animated: false });
    if (focusRetries.current++ < 3) {
      setTimeout(() => listRef.current?.scrollToIndex({ index, animated: true, viewPosition: FOCUS_POSITION }), 120);
    }
  }, []);

  const seen = useRef<Map<string, string> | null>(null);
  useEffect(() => {
    const prev = seen.current;
    seen.current = new Map(turns.map((t) => [t.id, t.status]));
    if (!prev) return; // turns that ended before the screen opened are not news
    for (const t of turns) {
      const footer = turnFooter(t);
      if (footer && prev.get(t.id) !== t.status) AccessibilityInfo.announceForAccessibility(footer.text);
    }
  }, [turns]);

  // Only the user's own drag or fling unpins; growth and our own scrolls never do.
  const userScrolling = useRef(false);
  const onScroll = useCallback((e: NativeSyntheticEvent<NativeScrollEvent>) => {
    const { contentOffset, contentSize, layoutMeasurement } = e.nativeEvent;
    const bottom = contentOffset.y + layoutMeasurement.height >= contentSize.height - spacing.huge;
    if (bottom === atBottom.current || (!bottom && !userScrolling.current)) return;
    atBottom.current = bottom;
    setShowJump(!bottom);
  }, []);
  const scrollStart = useCallback(() => {
    userScrolling.current = true;
  }, []);
  const scrollEnd = useCallback(() => {
    userScrolling.current = false;
  }, []);
  // The native scrollToEnd: FlatList's own uses the last cell's cached frame, which lags
  // behind a card that just arrived or text that grew in place.
  const stick = useCallback(() => {
    if (atBottom.current) listRef.current?.getNativeScrollRef()?.scrollToEnd({ animated: false });
  }, []);
  const jump = useCallback(() => {
    atBottom.current = true;
    setShowJump(false);
    listRef.current?.getNativeScrollRef()?.scrollToEnd({ animated: true });
  }, []);

  const renderItem = (item: TranscriptItem): ReactNode => (
    <ItemView
      item={item}
      approval={model.approvals.get(item.id)}
      attention={model.attention.has(item.id)}
      childItems={model.children.get(item.id)}
      renderChild={model.children.has(item.id) ? renderItem : undefined}
      onDecide={onDecide}
      onOpenLink={onOpenLink}
      agentKind={agentKind}
      promptWhileBusy={promptWhileBusy}
      replyStart={starts.has(item.id)}
      agentName={agentName}
    />
  );
  const renderRow = ({ item: row }: ListRenderItemInfo<TranscriptRow>) => (
    <View style={[styles.row, row.key === flash && styles.flash]}>
      {row.type === 'item' ? (
        renderItem(row.item)
      ) : row.type === 'footer' ? (
        <TurnFooter turn={row.turn} />
      ) : (
        <StandaloneApproval permission={row.permission} onDecide={onDecide} agentKind={agentKind} onOpenLink={onOpenLink} />
      )}
    </View>
  );

  return (
    <View style={[styles.flex, style]}>
      <FlatList
        ref={listRef}
        data={model.rows}
        extraData={flash}
        keyExtractor={rowKey}
        renderItem={renderRow}
        onScrollToIndexFailed={onScrollToIndexFailed}
        onScroll={onScroll}
        onScrollBeginDrag={scrollStart}
        onScrollEndDrag={scrollEnd}
        onMomentumScrollBegin={scrollStart}
        onMomentumScrollEnd={scrollEnd}
        scrollEventThrottle={32}
        onContentSizeChange={stick}
        onLayout={stick}
        keyboardShouldPersistTaps="handled"
        contentContainerStyle={styles.listContent}
        ListHeaderComponent={
          truncatedBefore ? <NoticeRow level="info" text="Earlier messages are only on the PC" /> : undefined
        }
        ListFooterComponent={
          working ? (
            <WorkingRow
              state={working.state}
              turnId={working.turn.id}
              startedAt={working.turn.startedAt}
              tool={working.tool}
              tokens={working.turn.usage ? working.turn.usage.inputTokens + working.turn.usage.outputTokens : undefined}
              costUsd={working.turn.costUsd}
              agentKind={agentKind}
              agentName={agentName}
            />
          ) : undefined
        }
      />
      {showJump ? (
        <Pressable onPress={jump} accessibilityRole="button" accessibilityLabel="Jump to latest" style={styles.jump}>
          <Text variant="smStrong" color={colors.white}>
            Jump to latest
          </Text>
          <Icon name="chevron-down" size={14} color={colors.white} />
        </Pressable>
      ) : null}
    </View>
  );
}

/* ------------------------------------------------------------------ */
/* Header chips: MCP status + permission mode                          */
/* ------------------------------------------------------------------ */

function Sheet({
  visible,
  title,
  onClose,
  children,
}: {
  visible: boolean;
  title: string;
  onClose: () => void;
  children: ReactNode;
}) {
  const insets = useSafeAreaInsets();
  return (
    <Modal visible={visible} transparent animationType="fade" onRequestClose={onClose} statusBarTranslucent>
      <KeyboardAvoidingView behavior={Platform.OS === 'ios' ? 'padding' : undefined} style={styles.sheetWrap}>
        <Pressable style={StyleSheet.absoluteFill} onPress={onClose} accessibilityLabel="Close" />
        <View style={[styles.sheet, { paddingBottom: spacing.xl + insets.bottom }]} accessibilityViewIsModal>
          <View style={styles.sheetHead}>
            <Text variant="h3" accessibilityRole="header" style={styles.flex}>
              {title}
            </Text>
            <Pressable onPress={onClose} hitSlop={10} accessibilityRole="button" accessibilityLabel="Close">
              <Icon name="x" size={18} />
            </Pressable>
          </View>
          <ScrollView keyboardShouldPersistTaps="handled" style={styles.sheetBody} contentContainerStyle={styles.gapMd}>
            {children}
          </ScrollView>
        </View>
      </KeyboardAvoidingView>
    </Modal>
  );
}

export interface McpChipProps {
  servers: McpServerState[];
  onPress: () => void;
  /** on the black session header */
  dark?: boolean;
}

/** "MCP 4/5" (connected of enabled), with a warning dot when a server needs sign-in or failed; hidden without servers. */
export function McpChip({ servers, onPress, dark = false }: McpChipProps) {
  const { connected, total, attention } = mcpSummary(servers);
  if (servers.length === 0) return null;
  return (
    <Pressable
      onPress={onPress}
      accessibilityRole="button"
      accessibilityLabel={`MCP servers: ${connected} of ${total} connected${attention ? ', needs attention' : ''}`}
      style={({ pressed }) => [styles.chip, dark && styles.chipDark, pressed && styles.pressed]}
    >
      <Icon name="plug" size={13} color={dark ? colors.white : colors.text} />
      <RNText style={[typography.xs, styles.chipText, dark && styles.chipTextDark]}>
        MCP {connected}/{total}
      </RNText>
      {attention ? <View style={styles.warnDot} /> : null}
    </Pressable>
  );
}

const MCP_STATUS: Record<McpStatus, { label: string; tone: Tone }> = {
  connected: { label: 'Connected', tone: 'success' },
  pending: { label: 'Connecting…', tone: 'neutral' },
  failed: { label: 'Failed', tone: 'error' },
  'needs-auth': { label: 'Needs sign-in', tone: 'warning' },
  disabled: { label: 'Disabled', tone: 'neutral' },
};

const AUTH_STATE: Record<NonNullable<McpServerState['auth']>['state'], string> = {
  starting: 'Starting sign-in…',
  'awaiting-browser': 'Finish signing in in the browser on your PC',
  'awaiting-callback': 'Sign in on this phone, then paste the address the browser lands on',
  completing: 'Completing sign-in…',
  failed: 'Sign-in failed',
};

export interface McpSheetProps {
  visible: boolean;
  servers: McpServerState[];
  onClose: () => void;
  /** `mcp_auth`: 'desktop' opens the runtime's sign-in page on the PC; 'phone-callback' returns `auth.url` */
  onAuth: (server: string, flow: 'desktop' | 'phone-callback') => void;
  /** `mcp_auth_callback` with the pasted localhost URL */
  onAuthCallback: (server: string, callbackUrl: string) => void;
  /** `mcp_reconnect` for a failed server */
  onReconnect: (server: string) => void;
  /** opens a URL on the phone: `auth.url` of the phone flow, or the claude.ai connectors page */
  onOpenUrl: (url: string) => void;
}

/** Server list with status and the §4.7 actions. Server names are untrusted plain text. */
export function McpSheet({ visible, servers, onClose, ...actions }: McpSheetProps) {
  return (
    <Sheet visible={visible} title="MCP servers" onClose={onClose}>
      {servers.length === 0 ? (
        <Text variant="sm" color={colors.text2}>
          No MCP servers in this session.
        </Text>
      ) : (
        servers.map((s, i) => <McpServerRow key={`${s.name}:${i}`} server={s} {...actions} />)
      )}
      <Text variant="xs" color={colors.text3}>
        Adding or removing servers and clearing credentials stay on the PC.
      </Text>
    </Sheet>
  );
}

function McpServerRow({
  server,
  onAuth,
  onAuthCallback,
  onReconnect,
  onOpenUrl,
}: Omit<McpSheetProps, 'visible' | 'servers' | 'onClose'> & { server: McpServerState }) {
  const [pasted, setPasted] = useState('');
  const st = MCP_STATUS[server.status] ?? MCP_STATUS.pending;
  const auth = server.auth;
  const flows = server.authFlows ?? [];
  const act = server.status === 'needs-auth' || server.status === 'failed' || !!auth;
  const working = auth?.state === 'starting' || auth?.state === 'completing';
  const url = auth?.state === 'awaiting-callback' && auth.url && /^https?:\/\//i.test(auth.url) ? auth.url : undefined;
  return (
    <View style={styles.mcpRow}>
      <View style={styles.mcpHead}>
        <StatusDot tone={st.tone} />
        <Text variant="smStrong" numberOfLines={1} style={styles.flex}>
          {server.name}
        </Text>
        <Text variant="xs" color={colors.text2}>
          {st.label}
        </Text>
      </View>
      {server.error ? (
        <Text variant="xs" color={colors.text2}>
          {server.error}
        </Text>
      ) : null}
      {auth ? (
        <Text variant="xs" color={auth.state === 'failed' ? colors.error : colors.text2}>
          {AUTH_STATE[auth.state] ?? auth.state}
          {auth.error ? `: ${auth.error}` : ''}
        </Text>
      ) : null}
      {act && flows.includes('desktop') ? (
        <Button
          label="Sign in on PC"
          size="sm"
          variant="secondary"
          disabled={working}
          onPress={() => onAuth(server.name, 'desktop')}
          accessibilityLabel={`Sign in on PC: ${server.name}`}
        />
      ) : null}
      {act && flows.includes('phone-callback') ? (
        url ? (
          <View style={styles.gapSm}>
            <Text variant="mono" color={colors.text2} selectable numberOfLines={4}>
              {url}
            </Text>
            <Button label="Open sign-in page" size="sm" onPress={() => onOpenUrl(url)} />
            <TextInput
              value={pasted}
              onChangeText={setPasted}
              placeholder={auth?.callbackPrefix ? `Paste the ${auth.callbackPrefix}… address` : 'Paste the callback address'}
              placeholderTextColor={colors.text3}
              autoCapitalize="none"
              autoCorrect={false}
              maxLength={4096}
              accessibilityLabel={`Callback address for ${server.name}`}
              style={styles.input}
            />
            <Button
              label="Finish sign-in"
              size="sm"
              disabled={working || !isCallbackUrl(pasted, auth?.callbackPrefix)}
              onPress={() => onAuthCallback(server.name, pasted.trim())}
              accessibilityLabel={`Finish sign-in: ${server.name}`}
            />
          </View>
        ) : (
          <Button
            label="Finish sign-in on this phone"
            size="sm"
            variant="secondary"
            disabled={working}
            onPress={() => onAuth(server.name, 'phone-callback')}
            accessibilityLabel={`Finish sign-in on this phone: ${server.name}`}
          />
        )
      ) : null}
      {act && flows.includes('claude-ai') ? (
        <Button
          label="Manage on claude.ai"
          size="sm"
          variant="secondary"
          onPress={() => onOpenUrl(CLAUDE_AI_CONNECTORS)}
          accessibilityLabel={`Manage on claude.ai: ${server.name}`}
        />
      ) : null}
      {server.status === 'failed' ? (
        <Button
          label="Reconnect"
          size="sm"
          variant="secondary"
          onPress={() => onReconnect(server.name)}
          accessibilityLabel={`Reconnect: ${server.name}`}
        />
      ) : null}
    </View>
  );
}

const MODE_HINT: Record<PermissionModeChoice, string> = {
  default: 'Asks before editing files or running commands.',
  acceptEdits: 'Applies file edits without asking; commands still ask.',
  plan: 'Reads and plans only; nothing changes until you approve the plan.',
};

export interface ModeChipProps {
  /** `LiveState.permissionMode` */
  mode?: string;
  /** `Agent.live.permissionModes`; only default / acceptEdits / plan are ever offered */
  modes?: string[];
  onChange: (mode: PermissionModeChoice) => void;
  /** on the black session header */
  dark?: boolean;
}

/** Claude's permission mode with a picker; read-only when the agent offers no modes. */
export function ModeChip({ mode, modes, onChange, dark = false }: ModeChipProps) {
  const [open, setOpen] = useState(false);
  const choices = (modes ?? []).filter(isModeChoice);
  const label = modeLabel(mode);
  return (
    <>
      <Pressable
        onPress={() => setOpen(true)}
        disabled={!choices.length}
        accessibilityRole="button"
        accessibilityLabel={`Permission mode: ${label}`}
        accessibilityState={{ disabled: !choices.length }}
        style={({ pressed }) => [styles.chip, dark && styles.chipDark, pressed && styles.pressed]}
      >
        <RNText style={[typography.xs, styles.chipText, dark && styles.chipTextDark]}>{label}</RNText>
        {choices.length ? <Icon name="chevron-down" size={12} color={dark ? colors.white : colors.text} /> : null}
      </Pressable>
      <Sheet visible={open} title="Permission mode" onClose={() => setOpen(false)}>
        {choices.map((m) => {
          const on = m === mode;
          return (
            <Pressable
              key={m}
              onPress={() => {
                setOpen(false);
                if (!on) onChange(m);
              }}
              accessibilityRole="radio"
              accessibilityState={{ checked: on }}
              accessibilityLabel={modeLabel(m)}
              style={[styles.modeRow, on && styles.modeRowOn]}
            >
              <View style={styles.flex}>
                <Text variant="bodyStrong">{modeLabel(m)}</Text>
                <Text variant="xs" color={colors.text2}>
                  {MODE_HINT[m]}
                </Text>
              </View>
              {on ? <Icon name="check" size={18} /> : null}
            </Pressable>
          );
        })}
      </Sheet>
    </>
  );
}

/* ------------------------------------------------------------------ */
/* Styles                                                              */
/* ------------------------------------------------------------------ */

const styles = StyleSheet.create({
  flex: { flex: 1 },
  gapXs: { gap: spacing.xs },
  flexShrink: { flexShrink: 1 },
  replyHead: { flexDirection: 'row', alignItems: 'center', gap: spacing.sm },
  working: { flexDirection: 'row', alignItems: 'center', gap: spacing.sm, minHeight: 44, paddingTop: spacing.xs },
  gapSm: { gap: spacing.sm },
  gapMd: { gap: spacing.md },
  pressed: { opacity: 0.7 },
  ink: { color: colors.text },
  bold: { fontWeight: '700' },
  italic: { fontStyle: 'italic' },
  strike: { textDecorationLine: 'line-through' },
  link: { textDecorationLine: 'underline' },
  inlineCode: {
    fontFamily: typography.mono.fontFamily,
    fontSize: typography.mono.fontSize,
    backgroundColor: colors.surface2,
  },

  message: { gap: spacing.sm },
  list: { gap: spacing.xs },
  listItem: { flexDirection: 'row', gap: spacing.sm },
  marker: { minWidth: spacing.xl, textAlign: 'right' },
  rule: { height: StyleSheet.hairlineWidth, backgroundColor: colors.border, marginVertical: spacing.xs },
  codeBlock: { backgroundColor: colors.black, borderRadius: radii.sm, padding: spacing.md, gap: spacing.xs },

  user: { backgroundColor: colors.surface2, borderRadius: radii.md, padding: spacing.md, gap: spacing.xs },
  thinkingRow: { flexDirection: 'row', alignItems: 'center', gap: spacing.sm, minHeight: 36 },
  notice: { paddingVertical: spacing.xs },
  footer: { flexDirection: 'row', alignItems: 'center', gap: spacing.sm, paddingVertical: spacing.xs },
  footerLine: { flex: 1, height: StyleSheet.hairlineWidth, backgroundColor: colors.border },
  footerText: { flexShrink: 1, textAlign: 'center' },

  plan: {
    borderWidth: StyleSheet.hairlineWidth,
    borderColor: colors.border,
    borderRadius: radii.sm,
    padding: spacing.md,
    gap: spacing.sm,
  },
  planHead: { flexDirection: 'row', alignItems: 'center', justifyContent: 'space-between' },
  step: { flexDirection: 'row', alignItems: 'flex-start', gap: spacing.sm },
  stepMark: {
    width: 16,
    height: 16,
    borderRadius: 8,
    borderWidth: 1.5,
    borderColor: colors.border,
    alignItems: 'center',
    justifyContent: 'center',
    marginTop: 1,
  },
  stepDone: { backgroundColor: colors.black, borderColor: colors.black },
  stepActive: { borderColor: colors.black, borderWidth: 4 },

  diff: { borderWidth: StyleSheet.hairlineWidth, borderColor: colors.border, borderRadius: radii.sm, overflow: 'hidden' },
  diffHead: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: spacing.sm,
    padding: spacing.sm,
    borderBottomWidth: StyleSheet.hairlineWidth,
    borderBottomColor: colors.border,
  },
  diffBody: { backgroundColor: colors.surface2, padding: spacing.sm },
  diffNote: { padding: spacing.sm },
  showMore: { padding: spacing.sm, alignItems: 'center', borderTopWidth: StyleSheet.hairlineWidth, borderTopColor: colors.border },

  toolCard: {
    borderWidth: StyleSheet.hairlineWidth,
    borderColor: colors.border,
    borderRadius: radii.sm,
    backgroundColor: colors.white,
  },
  toolCardStrong: { borderWidth: 1.5, borderColor: colors.black },
  toolHead: { flexDirection: 'row', alignItems: 'center', gap: spacing.sm, paddingHorizontal: spacing.md, minHeight: 44 },
  toolName: { flexShrink: 0, maxWidth: '45%' },
  toolBody: { gap: spacing.sm, paddingHorizontal: spacing.md, paddingBottom: spacing.md },
  output: { backgroundColor: colors.surface2, borderRadius: radii.sm, padding: spacing.md, gap: spacing.xs },
  waiting: { paddingHorizontal: spacing.md, paddingBottom: spacing.md },
  children: {
    gap: spacing.sm,
    borderLeftWidth: StyleSheet.hairlineWidth,
    borderLeftColor: colors.border,
    paddingLeft: spacing.sm,
  },

  approval: { gap: spacing.md },
  buttonRow: { flexDirection: 'row', gap: spacing.sm },
  moreToggle: { flexDirection: 'row', alignItems: 'center', gap: spacing.xs, minHeight: 36, alignSelf: 'flex-start' },
  input: {
    minHeight: 44,
    borderWidth: 1,
    borderColor: colors.border,
    borderRadius: radii.sm,
    backgroundColor: colors.white,
    paddingHorizontal: spacing.md,
    paddingVertical: spacing.sm,
    fontSize: typography.body.fontSize,
    color: colors.text,
  },
  pathLine: { flexDirection: 'row', alignItems: 'center', gap: spacing.sm },
  chips: { flexDirection: 'row', flexWrap: 'wrap', gap: spacing.sm },
  option: {
    borderWidth: 1,
    borderColor: colors.border,
    borderRadius: radii.sm,
    backgroundColor: colors.white,
    paddingHorizontal: spacing.md,
    paddingVertical: spacing.sm,
    maxWidth: '100%',
  },
  optionOn: { backgroundColor: colors.black, borderColor: colors.black },
  planBox: { borderWidth: StyleSheet.hairlineWidth, borderColor: colors.border, borderRadius: radii.sm, padding: spacing.md },

  row: { paddingBottom: spacing.md },
  // a halo around the deep-linked card; the negative margins keep the layout still
  flash: {
    backgroundColor: colors.surface2,
    borderRadius: radii.sm,
    marginHorizontal: -spacing.sm,
    paddingHorizontal: spacing.sm,
    marginTop: -spacing.sm,
    paddingTop: spacing.sm,
  },
  listContent: { paddingTop: spacing.lg, paddingBottom: spacing.lg },
  jump: {
    position: 'absolute',
    bottom: spacing.md,
    alignSelf: 'center',
    flexDirection: 'row',
    alignItems: 'center',
    gap: spacing.xs,
    backgroundColor: colors.black,
    borderRadius: radii.pill,
    paddingHorizontal: spacing.lg,
    paddingVertical: spacing.sm,
    ...shadow.lift,
  },

  chip: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: spacing.xs,
    minHeight: 32,
    paddingHorizontal: spacing.md,
    borderRadius: radii.pill,
    borderWidth: StyleSheet.hairlineWidth,
    borderColor: colors.border,
    backgroundColor: colors.white,
  },
  chipDark: { backgroundColor: colors.black, borderColor: colors.text2 },
  chipText: { color: colors.text, fontWeight: '700' },
  chipTextDark: { color: colors.white },
  warnDot: { width: 7, height: 7, borderRadius: 4, backgroundColor: colors.warning },

  sheetWrap: { flex: 1, justifyContent: 'flex-end', backgroundColor: 'rgba(5,5,5,0.55)' },
  sheet: {
    maxHeight: '85%',
    backgroundColor: colors.white,
    borderTopLeftRadius: radii.lg,
    borderTopRightRadius: radii.lg,
    borderWidth: 1,
    borderColor: colors.black,
    paddingHorizontal: spacing.xl,
    paddingTop: spacing.lg,
    gap: spacing.md,
  },
  sheetHead: { flexDirection: 'row', alignItems: 'center', gap: spacing.md },
  sheetBody: { flexGrow: 0 },
  mcpRow: {
    gap: spacing.sm,
    paddingVertical: spacing.md,
    borderBottomWidth: StyleSheet.hairlineWidth,
    borderBottomColor: colors.border,
  },
  mcpHead: { flexDirection: 'row', alignItems: 'center', gap: spacing.sm },
  modeRow: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: spacing.md,
    padding: spacing.md,
    borderRadius: radii.sm,
    borderWidth: 1,
    borderColor: colors.border,
  },
  modeRowOn: { borderColor: colors.black, borderWidth: 1.5 },
});
