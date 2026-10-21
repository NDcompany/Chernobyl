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
- `spacing`: xs 4 · sm 8 · md 12 · lg 16 · xl 20 · xxl 24 · xxxl 32 · huge 40.
- `radii`: screen 34 · lg 24 · md 16 · sm 12 · pill 999.
- `typography` (via `Text variant=`): display · h1 · h2 · h3 · body · bodyStrong · sm · smStrong ·
  xs · eyebrow (uppercase tracked) · mono (monospace, for commands/paths/output).
- `shadow`: `soft` (cards) · `lift` (modals/toasts). `TAB_BAR_HEIGHT = 64`.

## Primitives (`src/ui/`, import from `@/ui`)

**`primitives.tsx`** — `Text` (variant/color/center/accessibilityRole ∈ text|header|button|none|alert),
`Button` (variant primary|secondary|ghost, size md|sm, loading, icon, fullWidth, `ringColor` focus
ring), `Chip` (active/count filter), `StatusDot` (tone), `StatusPill` (protocol `SessionStatus` →
label/colors), `RiskPill` (low|medium|high), `ProgressBar`, `StatCard`, `SearchField`, `SectionTitle`,
`TextField` (RHF-backed — takes `form` + `name`, renders label/error/hint itself).

**`surface.tsx`** — `Screen` (layout shell: safe-area + scroll + `header`/`footer` slots +
`safeTop=false` when the screen draws its own dark header + `bottomInset` for tab clearance +
`scrollRef` for live-output autoscroll), `PageHeader` (`onBack`: undefined→`router.back()`, null→no
button; `dark` variant for black headers), `Card` (`strong` = black outline), `EmptyState` (default
`contour="bottom"`), `ToastHost` (global, reads `appStore.toasts`, tab-bar aware — mounted once in
root layout), `ConfirmModal` (destructive confirmations; `note` = scope/safety line).

**`lists.tsx`** — `AgentCard` (tile|row), `SessionRow`, `MonoBlock` (command on black), `PermissionCard`
(compact|full, `resolved` dims), `ActivityRow`, `PromptComposer` (RHF+zod, shows Interrupt when
`busy`), `Timeline`, `OutputBlock` (stderr→error color, system→italic), `FileRow` (+add/−del),
`DetailRow`, `SegmentedTabs`.

**`icons.tsx`** — `Icon` + `IconName` (34 names, 24×24 stroke grid, `strokeWidth` prop). New icon =
add path to `PATHS`, never inline Svg in a screen.

**`graphics.tsx`** — `Contours` (frame|top|bottom|ambient — decorative, clipped, never touchable,
~0.5 opacity) and `AgentIcon` (kind). The two signature brand elements.

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
via `useAppStore` (`@/store/appStore`). Never add a second data path. Commands go through the
transport (commandId dedupe). **ADR-009: mock mode is permanent** — mock-host hints (e.g. the
PromptComposer caption) and the simulation badge must stay honest; never render mock data as live,
never remove an offline/stale state to make a screen look alive.

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

## Routes (`app/`)

Tabs: `(tabs)/index` Home · `agents` → `agent/[id]` · `sessions` → `session/[id]` · `approvals` →
`permission/[id]` · `profile` → `settings/[section]`. Modals: `connect-host`, `new-task`.
Full-screen (no gesture back): `onboarding`, `sign-in`.
