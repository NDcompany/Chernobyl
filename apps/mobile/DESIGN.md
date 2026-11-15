# CHERNOBYL mobile UI — agent design contract

**Audience:** AI agents (and humans) changing `apps/mobile`. Read this before any UI edit.
**Look source of truth:** `design/reference-ui.png` (monochrome, thin contours, generous whitespace).
**Code source of truth:** `src/theme/tokens.ts` + `src/ui/`. This file is the map, not the values — when they disagree, the code wins, then fix this file.

**Maintenance rule:** change a token, primitive, route, or form pattern → update this file in the same commit.

## Rule zero

Black + white. State colors exist but are **never decorative**. Never hardcode a hex, spacing
number, radius, or font size — import from `@/theme/tokens`. Borders are hairlines
(`StyleSheet.hairlineWidth`) or 1px `colors.border`; strong outlines are `colors.black`.

## Tokens (`src/theme/tokens.ts`)

- `colors`: `black #050505` (primary fill), `white`, `presentation #D0D0D0`, `surface2 #F3F3F3`
  (subtle fill), `text #111111` / `text2 #666666` / `text3 #8B8B8B` (3-step hierarchy),
  `border #D2D2D2`, `borderStrong` (= black). State only: `success #2F7D4F`(+Bg), `warning #9A6B00`(+Bg), `error #A32B2B`(+Bg).
- `agentColors` (identity, used **only** by `AgentMark`, never for text or state): `claude #D97757`
  terracotta · `codex #0F8A7E` teal · `opencode #5B5FC7` indigo · `antigravity #8B5CF6` violet ·
  `generic #666666`. Each keeps ≥3:1
  on both white and black (asserted in `agentIdentity.test.ts`); agent names stay in text colours.
- `spacing`: xs 4 · sm 8 · md 12 · lg 16 · xl 20 · xxl 24 · xxxl 32 · huge 40.
- `radii`: screen 34 · lg 24 · md 16 · sm 12 · pill 999.
- `typography` (via `Text variant=`): display · h1 · h2 · h3 · body · bodyStrong · sm · smStrong ·
  xs · eyebrow (uppercase tracked) · mono (monospace, for commands/paths/output).
- `shadow`: `soft` (cards) · `lift` (modals/toasts). `TAB_BAR_HEIGHT = 64`.

## Primitives (`src/ui/`, import from `@/ui`)

**`primitives.tsx`** — `Text` (variant/color/center/accessibilityRole ∈ text|header|button|none|alert),
`Button` (variant primary|secondary|ghost, size md|sm, loading, icon, fullWidth, `ringColor` focus
ring), `Chip` (active/count filter; optional leading `icon`, e.g. an `AgentMark`), `StatusDot` (tone), `StatusPill` (protocol `SessionStatus` →
label/colors), `RiskPill` (low|medium|high), `SimulatedTag` (grey "Simulated" provenance pill, ADR-009),
`LivePcTag` (ADR-018: "Live on PC" pill, `success` dot, black hairline outline; `inverse` on the black header),
`ProgressBar`, `StatCard`, `SearchField`, `SectionTitle`,
`TextField` (RHF-backed — takes `form` + `name`, renders label/error/hint itself).

**`surface.tsx`** — `Screen` (layout shell: safe-area + scroll + `header`/`footer` slots +
`safeTop=false` when the screen draws its own dark header + `bottomInset` for tab clearance +
`scrollRef` for live-output autoscroll; a `KeyboardAvoidingView` with `padding` on both platforms
keeps the footer above the keyboard, since edge-to-edge Android no longer resizes the window), `PageHeader` (`subtitleIcon` sits before the subtitle — the session screens put the agent's mark there; `onBack`: undefined→`router.back()`, null→no
button; `dark` variant for black headers; a `right` element such as a status pill sizes itself, the 44 px slot is only a minimum), `Card` (`strong` = black outline), `EmptyState` (default
`contour="bottom"`), `ToastHost` (global, reads `appStore.toasts`, tab-bar aware — mounted once in
root layout), `ConfirmModal` (destructive confirmations; `note` = scope/safety line).

**`lists.tsx`** — `AgentCard` (tile|row), `SessionRow` (agent mark before the agent · host line; ADR-018: `LivePcTag` instead
of the status pill while `session.livePc`, a `text3` "Forked from “…”" line when `forkedFrom`), `MonoBlock` (command on black), `PermissionCard`
(compact|full, `resolved` dims; a separate "From a PC session" line under the agent · host line when `fromPc`, so a long session title never cuts it), `ActivityRow`, `PromptComposer` (RHF+zod, 32,000-char limit, Stop
while `busy`; `promptWhileBusy` `merge`|`queue` keeps Send enabled mid-turn with "Joins the current
turn" / "Runs after this turn", `none` blocks it), `Timeline`, `OutputBlock` (stderr→error color,
system→italic), `FileRow` (+add/−del), `DetailRow`, `SegmentedTabs`. ADR-017 composer extras, each
only when the parent passes it (from `Agent.live.composer`): `slashCommands` (a leading `/` lists up
to 8 matching names above the field, prefix matches first; a tap inserts `/<name> `), `canAttach`
(a camera icon button; `PhotoCapture` is a full-screen black modal on `expo-camera` that shoots at
about 1280x960 (a fixed `pictureSize` from mount: setting it later rebinds the camera right after it opens) with `takePictureAsync({quality, base64})`, shooting again at falling quality until it fits the base64 budget
(never `pictureRef` + `savePictureAsync`: expo-camera 58 on Android encodes any quality below 1 at 0); 56 px
thumbnails with a black × above the field, "N of 4 photos" as the caption; before sending, the
sealed frame size is checked and "Too large to send in one message" shows in `error`). No gallery:
that needs `expo-image-picker`, which the app does not ship. Pure helpers in `src/utils/composer.ts`
(unit-tested).

**`transcript.tsx`** — ADR-015 conversation view, presentational only (data + callbacks as props;
pure logic in `src/utils/transcript.ts`, unit-tested by `npm test -w @chernobyl/mobile`).
`TranscriptList` (FlatList keyed by item id; footer after each ended turn; standalone approval
cards at the end for pending requests whose tool item is not in view; a notice that repeats its
turn's footer ("Stopped") is dropped; sticks to the bottom (native `scrollToEnd`, so new cards and
text growing in place stay in view) until the user's own drag or fling leaves it, then a "Jump to
latest" pill; announces turn ends only, never deltas;
`focusItemId` = deep link: once that item is in view it scrolls its card (or the top-level card it
is nested in, or its standalone approval) into the upper third and gives it a brief `surface2`
halo),
`MessageText` (markdown subset: paragraphs, headings, bold, italic, inline code, fenced code mono on
black with sideways scroll, lists, rules; no HTML, images or remote loads; links show the full URL
and open only after a `ConfirmModal`; streaming-safe: unclosed syntax renders as if closed and
dangling markers wait, so text only grows), `UserBubble` (`surface2`; "Queued" / "Joins the current
turn"), `ThinkingRow` (collapsed one-line preview → `text2` italic), `ToolCard` (name verbatim, title,
a status mark per `ToolStatus`; expands to input `MonoBlock`, output with truncated marker, `DiffView`,
"Auto-approved (…)", nested subagent items; a pending approval outlines it black and keeps it open;
`awaiting-approval` without a record shows "Waiting for approval…"), `DiffView` (+/− in
`success`/`error`, hunks `text3`, first 200 lines then "Show more"; like code blocks, each line
ends in a space so Android's pixel rounding cannot wrap the widest line out of the box), `InlineApproval` /
`StandaloneApproval` (detail by kind; eyebrow "Approval needed · from a PC session" when `fromPc`; primary + secondary + "More…" with session/persistent grants and
"Deny and tell <agent> why"; persistent grants confirm the exact rule and file), `PlanCard`,
`NoticeRow`, `TurnFooter` ("Done · 12.4 s · 4.2k tokens · $0.01"; a value that is 0 or missing, as on a mirrored PC turn, is left out, so just "Done"), reply heads (the first agent item after a prompt or of a new turn —
`replyStarts` — shows a 16 px `AgentMark` + the agent's name; nested work and notices never do),
`WorkingRow` (the list footer while a turn runs: animated mark, a status verb from `WORKING_VERBS`
rotating every 3 s, "Running <tool>…" while any tool of the open turn runs (notices after the card do not hide it), "Waiting for you" while a request is
pending (wins over both), elapsed seconds and live tokens/cost when the turn reports them; gone when the turn ends, the
footer stays. It ticks on its own `useNow(1000)`, so rows never re-render for it. One polite live
region whose label only flips between "<agent> is working" and "<agent> is waiting for you"; iOS
gets the same label via `announceForAccessibility`), `McpChip` ("MCP 4/5" + warning dot), `McpSheet` (status + §4.7 sign-in
actions), `ModeChip` (permission mode picker; `dark` variant for the black header; ADR-021: with
`agentKind` `opencode` the modes read "Build" / "Plan", OpenCode's built-in agents, with their own hints).
ADR-017: `UserBubble` shows an outlined "1 image" / "N images" chip (camera icon) above the text
when the prompt carried photos (only the count comes back); `ModelChip` (current model, sheet of the
runtime's models or the `default`/`sonnet`/`opus`/`haiku` aliases), `ContextChip` ("Context N%",
warning dot from 80%, hidden while unknown) and `RenameChip` (pencil; sheet with a text field,
80 characters) share the chip style and its `dark` variant. ADR-021: `ModelChip` and `RenameChip`
take `agentName` ("Claude Code" by default, "OpenCode") for their sheet notes.

**`workspace.tsx`** — ADR-016 Files / Changes pieces, presentational only (pure logic in
`src/utils/workspace.ts`, unit-tested): `WorkspaceUnavailable` ("Connect a PC" without
`workspace.v1`, "Workspace not found" for an unknown id), `CodeView` (a text file mono on black
like the transcript's code blocks, sideways scroll, rendered in 200-line chunks, each line ending
in a space), `GitFileRow` (porcelain letter, path cut in the middle via RN `Text`'s
`ellipsizeMode`, a pill Stage/Unstage toggle; tap the path to open its diff). Diffs reuse the
transcript's `DiffView`.

**`agentMark.tsx`** — `AgentMark({ kind, size, state })`, the one place agent identity is drawn. Keyed
by `Agent.kind` via `markFor` (`@/utils/agentIdentity`): `claude-code` → ember (diamond spark with
eyes), `codex` → antenna bot, `opencode` → braces around a dot, `antigravity` → a flying saucer with
two eyes on its beam (ADR-022; also in the KitKat client's `Ui.java`), everything else (browser, local/Ollama,
custom, unknown kinds) → round blob. ORIGINAL 8×8 pixel characters — never trace or copy a company's
logo or mascot; the agent's name is always written beside the mark in text. Sizes 16/24/32/48 stay
pixel-crisp; eyes are holes so the surface shows through. States: `idle`/`done` still · `thinking`
slow bob + sparkle · `working` quicker bob + sparkle · `waiting` calm fade pulse · `error` dimmed.
Motion is transform/opacity on the native driver only, and `useReducedMotion` always gets the still
frame. Decorative (hidden from screen readers). Demo agents keep their mark *and* the `SimulatedTag`.

**`icons.tsx`** — `Icon` + `IconName` (40 names, 24×24 stroke grid, `strokeWidth` prop). New icon =
add path to `PATHS`, never inline Svg in a screen.

**`graphics.tsx`** — `Contours` (frame|top|bottom|ambient — decorative, clipped, never touchable,
~0.5 opacity) and `AgentIcon` (kind: the agent's `AgentMark` on a black or outlined tile, sized to a
multiple of 8). The two signature brand elements. `LogoMark` (the owner's
mark, one colour: `black` on light, `white` on the dark header) and `Wordmark` (mark + letterspaced
CHERNOBYL).

**Logo files** — the master is `design/Chernobyl Logo.png` (white tile, black mark). Never edit the
derived files by hand: `node scripts/brand/make-logo-assets.mjs` regenerates all of them — the app
icon, adaptive foreground/monochrome/background, splash, favicon, `src/ui/logoPath.ts` (the traced
vector `LogoMark` draws), the status-bar glyph `ic_stat_chernobyl.xml`, the companion's
`chernobyl.ico` and the `design/logo/` kit (tile, dark tile, mark, white mark; PNG + SVG). The mark
always keeps the tile's proportion (68% of the tile) and is never recoloured beyond black/white.
Plain-text surfaces (terminal TUI, logs) keep the `[·]` glyph as the text fallback.

**`components/BottomNav.tsx`** — custom tab bar (`expo-router/js-tabs` `BottomTabBarProps`), live
pending-permissions badge on Approvals.

## Layout conventions

Root (`app/_layout.tsx`): QueryClientProvider → SafeAreaProvider → TransportProvider → Stack
(`headerShown: false`) + `ToastHost`. Tabs (`app/(tabs)/_layout.tsx`) gate on `onboarded`/`authed`
zustand flags with redirects. Dark headers = `<Screen safeTop={false} header={<PageHeader dark …/>}>`.

## Forms

`zodHandler(schema, form, onValid)` from `@/forms/zodBridge` — returns the runner; wire
`onPress={submit}` (never call `submit()` imperatively). It clears RHF errors before validating
(no-resolver quirk); if you bypass it with plain `handleSubmit` + `setError`, a stale custom error
permanently blocks the success callback.

## State & transport honesty

Screens read state via `useSnapshot()` (`@/data/state`, TanStack Query single snapshot) and UI state
via `useAppStore` (`@/store/appStore`, which also keeps `hello_ack.features`). Never add a second
data path. The one second *reducer* is the ADR-015 live transcript store (`@/data/live`): it folds
`transcript` / `transcript_snapshot` frames with `applyTranscript` from the **same** transport
message stream, fed next to the snapshot reducer in `TransportProvider`; it is not a second
connection or fetch path. Its views are memory-only (threat model T2): created when a conversation
screen focuses, dropped when the last one blurs, never persisted. Commands go through the
transport (commandId dedupe); `session_subscribe` / `session_unsubscribe` go straight to
`getTransport().send()` because they must never toast. **ADR-009: mock mode is permanent** —
mock-host hints (e.g. the PromptComposer caption, the new-task note) and the simulation badge must
stay honest; never render mock data as live, never remove an offline/stale state to make a screen
look alive.

**Live sessions (ADR-015 §5):** the conversation view shows only for a session with
`transcript: true` on a host that advertised `transcript.v1`; everything else keeps the classic
screen. `useLiveSession(sessionId)` subscribes while the screen is focused and again whenever the
transport returns to `connected` (sending the view's `epoch` and last `seq`, so the companion
replays the gap or sends a snapshot), re-subscribes when the reducer says `resubscribe` (at most
once a second), and unsubscribes on blur (ref-counted per session). Inline decisions, prompts, Stop,
mode, MCP and End go through `useCommands`, so ack errors toast and a lost ack stays a visible
unknown outcome; nothing in the conversation shows a success toast (the transcript is the outcome).
Links and MCP sign-in pages open only http(s) URLs, through `Linking.openURL`, after the component's
own confirm or full-URL display. The app's mock runs `MockEngine` in live mode: Claude Code's
billing session plays one scripted transcript (thinking, plan, streamed text, an Edit diff, an
inline `npm test` approval), badged Simulated because its agent and host are `simulated`.

**Files and Changes (ADR-016):** shown only when `hello_ack.features` has `workspace.v1` and the
agent lists the workspace (`Agent.live.workspaces`); the simulation never advertises it, so the
entry points are hidden and a deep link shows `WorkspaceUnavailable` (ADR-009: no fake files).
Reads (`@/data/workspace` `useWorkspaceRead`) are request/response on the same transport: the
`workspace_result` arrives right before its ack, nothing is cached or persisted, and an error is
shown inline (never a toast). Git changes go through `useCommands` (ack errors toast, a lost ack
is an unknown outcome) via `useGitChange`, which resolves with the command's `command_result`;
git's own failure text shows in a "Git said" card, and the status is re-read after every change.
Push always confirms with the exact `branch → upstream`; Pull (fast-forward only, ADR-016 §7)
with the exact `upstream → branch` (`pullSource` in `@/utils/workspace`). A Pull whose `command_result`
carries `agentConfigChanged` adds a `warning`-outlined "Agent settings changed" card (`agentConfigWarning`: the
paths, five at most, then "review them on your PC"); the next change clears it.

**Terminal (ADR-020):** shown only when `hello_ack.features` has `terminal.v1` (the PC opted in;
off by default) and the workspace is listed; the simulation never advertises it, and a deep link
says "The terminal is off on this PC". The screen always shows the `warning`/`warningBg` banner
"Runs on your PC without approval prompts" (state colour, because it is a real risk, not
decoration). `useTerminal` (`@/data/terminal`) sends `terminal_run` through `useCommands` (ack
errors toast, a lost ack is an unknown outcome), subscribes before sending, appends
`terminal_output` chunks into screen state (never persisted) and shows `terminal_exit` through
`exitLabel` (`@/utils/terminal`, unit-tested): `success` for exit 0, `error` otherwise. Stop sends
`terminal_stop` straight to the transport (ack-only, never toasts on success). Output renders in
the Files `CodeView`. The last 20 command lines per workspace (`remember`) persist in
`appStore.terminalHistory` (local only) and show as `Chip`s that fill the field.

**Provenance travels with the data:** `Host`/`Agent` carry `simulated: true` when they come from a
mock seed/engine (the app's offline mock or a companion's demo `mock` adapter); real adapters omit the
key. `SimulationBadge` shows while the transport is the app's mock OR the snapshot has any simulated
host/agent (`hasSimulatedData`). Host/agent rows and cards, and sessions/approvals whose agent or
host is simulated, carry a `SimulatedTag` (and ", simulated" in their accessibility label).

**Mandate section 9 (visible uncertainty about command outcomes):** a command whose ack timed out
or whose connection dropped after send has an UNKNOWN outcome (the host may have executed it). The
UI shows it as "outcome unknown, will reconcile on reconnect" (not failed, not done), and resolves
it to the real result when the replayed command's ack/command_result arrives after reconnect.
Rejected before send ('offline') is a definite not-sent state. Never resubmit with a new
commandId automatically.

**Phase 6 endpoint-list host records (conflict C14):** in Settings > Connected hosts, a real stored
pairing is displayed as a real record (host id, device id, ordered endpoint list with primary first,
and live transport status) labelled Real, visually distinct from mock or `simulated` hosts, which
carry a `SimulatedTag`. Derived from the keystore pairing and store transport status without new storage.

## Gotchas (each cost real debugging)

| Rule | Why |
|---|---|
| a11y props on `View`/`Pressable`, **never on `Svg`** | web passes them raw to the DOM → console error storms |
| Type tab bars via `expo-router/js-tabs` | there is no `@react-navigation` root import |
| `StyleSheet.absoluteFill`, not `absoluteFillObject` | the latter is a static object, breaks fn styles |
| `Screen scrollRef` is `RefObject<ScrollViewInstance \| null>` | RN's ScrollView ref type |
| Back button on dark headers | `PageHeader dark` renders its own; a ghost `Button` is black-on-black → use `variant="primary"` + `ringColor={colors.white}` if a custom dark-surface button is unavoidable |
| `Text` roles are RN-limited | only text/header/button/none/alert |
| Status/risk pills take protocol types | `StatusPill status={session.status}` — don't stringify your own labels |
| Approval buttons come only from the `PermissionRequest` record | `options` (labels verbatim) or legacy `supportedDecisions`, never transcript text (ADR-015 T3); a persistent option is never primary |
| Nested markdown spans use RN `Text`, not the `Text` primitive | the primitive's default `body` variant would override the heading size it sits in |
| A text block containing a link is not `selectable` | Android's selectable TextView swallows the nested link's `onPress` |
| Unit tests run on Node's type stripping | test files import with `.ts` extensions and only `import type` from packages; they are excluded from the app tsconfig (no Node globals in a Hermes app) and typechecked by `tsconfig.test.json` |

## Routes (`app/`)

Tabs: `(tabs)/index` Home · `agents` → `agent/[id]` · `sessions` → `session/[id]` · `approvals` →
`permission/[id]` · `profile` → `settings/[section]`. Modals: `connect-host` (Pair-with-QR camera scan via `expo-camera` with honest not-asked/denied/unavailable states + paste fallback; explicit confirm step before save), `new-task` (agents with `live.workspaces` get a workspace picker instead of the project field; `create_session.project` = the workspace id; other real agents get neither, only simulated agents keep the free-text project field; agents with `live.composer` `model` get a Model chip row, Default sending no `model`; the row lists `live.models` when the agent sends them (OpenCode's `provider/model` ids, `newTaskModels`), else Claude Code's aliases, and switching agents resets it to Default).
`permission/[id]` · `profile` → `settings/[section]`. Modals: `connect-host` (Pair-with-QR camera scan via `expo-camera` with honest not-asked/denied/unavailable states + paste fallback; explicit confirm step before save), `new-task` (agents with `live.workspaces` get a workspace picker instead of the project field; `create_session.project` = the workspace id; other real agents get neither, only simulated agents keep the free-text project field; agents with `live.composer` `model` get a Model chip row, Default sending no `model`; the chips are the agent's own `live.models` when it lists them (Antigravity, ADR-022), else Claude's aliases).
`permission/[id]` · `profile` → `settings/[section]`. Modals: `connect-host` (Pair-with-QR camera scan via `expo-camera` with honest not-asked/denied/unavailable states + paste fallback; explicit confirm step before save), `new-task` (agents with `live.workspaces` get a workspace picker instead of the project field; `create_session.project` = the workspace id; other real agents get neither, only simulated agents keep the free-text project field; Claude Code agents with `live.composer` `model` get a Model chip row, Default sending no `model`; Codex picks its own models in the session).
Full-screen (no gesture back): `onboarding`, `sign-in`.
Android system Back pops the stack and leaves the app only from a root: `predictiveBackGestureEnabled` stays
`false` in `app.json` (RN 0.88 registers its back callback only on Android 16+, so with the flag on, Android 13-15
left the app from any screen).
Workspace (ADR-016): `files/[workspace]?path=` (one screen per folder, so Back walks up),
`file/[workspace]?path=` (text viewer), `changes/[workspace]` (git), `terminal/[workspace]`
(ADR-020, only with `terminal.v1`). Entry points: the agent screen's Workspaces section (Files,
Changes, and Terminal when enabled) and the Files / Changes chips in the live conversation header
(the session's workspace is the one whose `path` equals `Session.project`).

`session/[id]` is the live conversation (see Live sessions) or the classic workspace; its black
header's chip row wraps: MCP, permission mode, model, context, then rename and End session on the
right (model, context, rename, slash commands and the camera only for agents whose
`live.composer` lists them: Claude, and OpenCode since ADR-021). ADR-018: a `livePc` session (a terminal or VS Code is
writing it) shows `LivePcTag` first in the chip row and hides mode, model, rename, End and Stop;
the transcript is the companion's live mirror. Its composer reads "Continue on phone (fork)…"
with the one-line hint "Live on PC: sending forks it into a new session here; the PC session is
untouched", sends `submit_prompt {fork: true}` and, on an ok ack naming another session,
`router.replace`s to it; a forked session shows "Forked from “…”; the PC session is unchanged."
in the notes. Idle sessions from the PC keep the plain composer (they resume in place);
`?focus=<toolItemId>` scrolls the conversation to that card. Requests open through
`permissionHref` (`@/data/selectors`): the Approvals tab, Home's pending list and notification taps
open a request from a live conversation at its card, any other request at `permission/[id]`. That screen draws
a record with `options` through the same `InlineApproval` (labels verbatim: a PC-session hook request is Allow / Deny);
only a legacy record without options gets Approve Once / Reject, plus "Approve for Session" when its
`supportedDecisions` has it and it is not `fromPc`. Local
notifications carry that route in `data.url` (a `fromPc` request is titled "Approval needed (PC session)"); `onNotificationOpen` (`@/platform/notify`, wired in
`TransportProvider`) opens it on tap, also when the tap launched the app, and accepts only
`/session/…` and `/permission/…` routes.

## Security hardening (security review)

- **Pairing confirm step** (`connect-host`, `pairingNotices` in `@/utils/pairing`): `warning`-coloured `xs` alert lines
  above the host details. "This replaces your current host <host>" whenever a pairing is stored; "Opened from
  another app or link — only continue if you started this pairing" when the payload came from a
  `chernobyl://connect-host?payload=` link (editing the field or scanning clears that mark). Saving a pairing
  also clears the stored host token, so an old token is never appended to the new endpoints.
