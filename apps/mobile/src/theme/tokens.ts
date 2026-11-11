/**
 * CHERNOBYL design tokens — taken from the monochrome reference:
 * black + white, thin contours, restrained borders, generous whitespace.
 */
import type { TextStyle, ViewStyle } from 'react-native';

export const colors = {
  black: '#050505',
  white: '#FFFFFF',
  presentation: '#D0D0D0',
  surface2: '#F3F3F3',
  text: '#111111',
  text2: '#666666',
  text3: '#8B8B8B',
  border: '#D2D2D2',
  borderStrong: '#050505',
  /** restrained state colors — never decorative */
  success: '#2F7D4F',
  successBg: '#EAF4EE',
  warning: '#9A6B00',
  warningBg: '#FBF1DC',
  error: '#A32B2B',
  errorBg: '#FBECEB',
} as const;

/**
 * Agent identity accents — used only by `AgentMark` (`src/ui/agentMark.tsx`), never for text or
 * state. Each keeps ≥3:1 against both `white` and `black` (non-text contrast), so a mark reads on
 * light cards and on the dark header alike.
 */
export const agentColors = {
  /** Claude Code: warm terracotta (3.1:1 on white, 6.5:1 on black) */
  claude: '#D97757',
  /** Codex: cool teal (4.2:1 on white, 4.8:1 on black) */
  codex: '#0F8A7E',
  /** OpenCode: indigo (5.4:1 on white, 3.8:1 on black) */
  opencode: '#5B5FC7',
  /** every other agent (browser, local/Ollama, custom, unknown) */
  generic: '#666666',
} as const;

export const spacing = {
  xs: 4,
  sm: 8,
  md: 12,
  lg: 16,
  xl: 20,
  xxl: 24,
  xxxl: 32,
  huge: 40,
} as const;

export const radii = {
  screen: 34,
  lg: 24,
  md: 16,
  sm: 12,
  pill: 999,
} as const;

export type TextVariant =
  | 'display'
  | 'h1'
  | 'h2'
  | 'h3'
  | 'body'
  | 'bodyStrong'
  | 'sm'
  | 'smStrong'
  | 'xs'
  | 'eyebrow'
  | 'mono';

export const typography: Record<TextVariant, TextStyle> = {
  display: { fontSize: 40, lineHeight: 46, fontWeight: '400', letterSpacing: -0.6 },
  h1: { fontSize: 27, lineHeight: 33, fontWeight: '700', letterSpacing: -0.5 },
  h2: { fontSize: 21, lineHeight: 27, fontWeight: '700', letterSpacing: -0.3 },
  h3: { fontSize: 17, lineHeight: 23, fontWeight: '600', letterSpacing: -0.2 },
  body: { fontSize: 15, lineHeight: 22, fontWeight: '400' },
  bodyStrong: { fontSize: 15, lineHeight: 22, fontWeight: '600' },
  sm: { fontSize: 13, lineHeight: 18, fontWeight: '400' },
  smStrong: { fontSize: 13, lineHeight: 18, fontWeight: '600' },
  xs: { fontSize: 11.5, lineHeight: 15, fontWeight: '500' },
  eyebrow: {
    fontSize: 11,
    lineHeight: 14,
    fontWeight: '700',
    letterSpacing: 1.4,
    textTransform: 'uppercase',
  },
  mono: {
    fontSize: 13,
    lineHeight: 19,
    fontFamily: 'monospace',
  },
};

export const shadow = {
  soft: {
    shadowColor: '#050505',
    shadowOpacity: 0.08,
    shadowRadius: 16,
    shadowOffset: { width: 0, height: 8 },
    elevation: 3,
  },
  lift: {
    shadowColor: '#050505',
    shadowOpacity: 0.14,
    shadowRadius: 10,
    shadowOffset: { width: 0, height: 4 },
    elevation: 5,
  },
} satisfies Record<string, ViewStyle>;

/** Tab bar height including the home-indicator inset budget. */
export const TAB_BAR_HEIGHT = 64;
