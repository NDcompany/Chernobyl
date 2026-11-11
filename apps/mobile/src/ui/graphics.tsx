/**
 * Brand graphics: thin organic contour lines and large flowing black shapes
 * that curve into white surfaces — the two signature elements of the
 * reference design. Both are decorative, clipped to their container and
 * never intercept touches.
 */
import { useMemo } from 'react';
import { StyleSheet, Text, View } from 'react-native';
import { Svg, Path, Rect } from 'react-native-svg';
import type { StyleProp, ViewStyle } from 'react-native';
import { colors } from '@/theme/tokens';
import type { AgentKind } from '@chernobyl/protocol';
import { AgentMark } from '@/ui/agentMark';
import { LOGO_MARK_PATH } from '@/ui/logoPath';

/* ------------------------------------------------------------------ */
/* Contours                                                            */
/* ------------------------------------------------------------------ */

const CONTOURS: Record<ContourVariant, readonly string[]> = {
  /** top-right cluster + bottom-left swirl (the reference's main frame) */
  frame: [
    'M412 34 C 352 66 364 126 334 162 C 304 198 340 242 306 274 C 272 306 284 348 254 374 C 230 395 198 400 168 392',
    'M408 84 C 356 116 374 156 348 192 C 322 228 354 262 322 298 C 292 332 304 366 276 394',
    'M402 -14 C 330 20 300 84 324 136 C 344 180 302 210 262 214',
    'M-16 512 C 40 538 72 596 56 652 C 42 704 92 736 148 726 C 204 716 238 752 222 794',
    'M-16 566 C 34 594 56 646 42 692 C 30 738 72 774 128 768',
    'M-10 640 C 44 664 68 706 58 748',
  ],
  /** top band only (headers) */
  top: [
    'M412 20 C 348 54 360 122 328 158 C 296 194 332 240 298 272 C 266 302 278 344 248 370',
    'M406 72 C 352 106 370 148 344 184 C 318 220 350 256 318 292 C 290 324 302 358 274 386',
    'M400 -18 C 326 16 296 82 320 134',
    'M-14 -10 C 60 30 96 96 74 156 C 56 206 96 240 150 234',
  ],
  /** bottom-left swirl only (empty states / footers) */
  bottom: [
    'M-16 380 C 44 410 76 470 58 528 C 42 582 94 616 152 604 C 210 592 246 630 230 674',
    'M-16 436 C 36 464 58 516 44 562 C 30 610 74 646 132 640',
    'M-10 520 C 46 546 72 590 62 634',
  ],
  /** long loose sweeps for the presentation background */
  ambient: [
    'M-20 96 C 90 62 168 140 146 236 C 124 332 202 396 306 372 C 388 354 426 416 402 500 C 380 576 414 640 470 660',
    'M-30 470 C 60 500 116 566 96 640 C 78 706 132 748 210 742',
    'M240 -20 C 214 60 258 108 330 106 C 396 104 428 150 420 214',
  ],
};

export type ContourVariant = 'frame' | 'top' | 'bottom' | 'ambient';

export interface ContoursProps {
  variant?: ContourVariant;
  color?: string;
  strokeWidth?: number;
  style?: StyleProp<ViewStyle>;
}

export function Contours({
  variant = 'frame',
  color = colors.black,
  strokeWidth = 1,
  style,
}: ContoursProps) {
  const paths = useMemo(() => CONTOURS[variant], [variant]);
  return (
    <View
      pointerEvents="none"
      style={[StyleSheet.absoluteFill, style]}
    >
      <Svg width="100%" height="100%" viewBox="0 0 390 780" preserveAspectRatio="xMidYMid slice">
        {paths.map((d, i) => (
          <Path
            key={i}
            d={d}
            stroke={color}
            strokeWidth={strokeWidth}
            fill="none"
            strokeLinecap="round"
          />
        ))}
      </Svg>
    </View>
  );
}

/* ------------------------------------------------------------------ */
/* Black flowing shapes                                                */
/* ------------------------------------------------------------------ */

const SHAPES: Record<BlackShapeVariant, { fill: string; line?: string; vb: string }> = {
  /** sign-in: black mass across the top with a white echo line */
  signin: {
    vb: '0 0 390 300',
    fill: 'M0 0 H390 V120 C 330 104 316 166 256 178 C 196 190 150 150 90 158 C 60 162 30 176 0 166 Z',
    line: 'M390 134 C 334 120 320 178 260 190 C 202 202 152 163 92 171 C 63 175 32 188 0 179',
  },
  /** dashboard / profile header: black block with an organic bottom edge */
  header: {
    vb: '0 0 390 260',
    fill: 'M0 0 H390 V168 C 340 198 300 158 250 174 C 204 188 174 216 124 218 C 80 220 40 204 0 214 Z',
  },
  /** agent detail header: shorter slab */
  detail: {
    vb: '0 0 390 240',
    fill: 'M0 0 H390 V132 C 344 158 308 144 262 160 C 216 176 190 204 144 208 C 96 212 52 196 0 206 Z',
    line: 'M390 146 C 348 170 312 158 266 174 C 220 190 194 218 148 221 C 100 224 54 209 0 219',
  },
};

export type BlackShapeVariant = 'signin' | 'header' | 'detail';

export interface BlackShapeProps {
  variant?: BlackShapeVariant;
  style?: StyleProp<ViewStyle>;
}

/** Absolutely positioned black shape; parent must clip (overflow hidden). */
export function BlackShape({ variant = 'header', style }: BlackShapeProps) {
  const shape = SHAPES[variant];
  return (
    <View
      pointerEvents="none"
      style={[StyleSheet.absoluteFill, { overflow: 'hidden' }, style]}
    >
      <Svg
        width="100%"
        height="100%"
        viewBox={shape.vb}
        preserveAspectRatio="none"
      >
        <Path d={shape.fill} fill={colors.black} />
        {shape.line ? (
          <Path
            d={shape.line}
            stroke={colors.white}
            strokeWidth={1.4}
            fill="none"
            vectorEffect="non-scaling-stroke"
            strokeLinecap="round"
          />
        ) : null}
      </Svg>
    </View>
  );
}

/* ------------------------------------------------------------------ */
/* Wordmark + logo mark                                                 */
/* ------------------------------------------------------------------ */

/** The CHERNOBYL mark (design/Chernobyl Logo.png, traced): one colour, any size. */
export function LogoMark({ size = 24, color = colors.black }: { size?: number; color?: string }) {
  return (
    <Svg width={size} height={size} viewBox="0 0 100 100" accessible={false}>
      <Path d={LOGO_MARK_PATH} fill={color} fillRule="evenodd" />
    </Svg>
  );
}

export interface WordmarkProps {
  size?: number;
  color?: string;
  showMark?: boolean;
  style?: StyleProp<ViewStyle>;
}

/** CHERNOBYL wordmark: the mark + letterspaced uppercase name. */
export function Wordmark({ size = 15, color = colors.black, showMark = true, style }: WordmarkProps) {
  return (
    <View style={[{ flexDirection: 'row', alignItems: 'center', gap: size * 0.55 }, style]}>
      {showMark ? <LogoMark size={size * 1.25} color={color} /> : null}
      <Text
        style={{
          fontSize: size,
          lineHeight: size * 1.3,
          fontWeight: '700',
          letterSpacing: size * 0.22,
          color,
        }}
      >
        CHERNOBYL
      </Text>
    </View>
  );
}

/* ------------------------------------------------------------------ */
/* Agent icon                                                          */
/* ------------------------------------------------------------------ */

export interface AgentIconProps {
  kind: AgentKind;
  size?: number;
  /** solid: black tile (default). outline: white tile + black border. Either way the agent's mark sits inside. */
  variant?: 'solid' | 'outline';
}

/** The agent's mark (`AgentMark`) on a tile, sized to a multiple of 8 so its pixels stay crisp. */
export function AgentIcon({ kind, size = 44, variant = 'solid' }: AgentIconProps) {
  const solid = variant === 'solid';
  return (
    <View
      style={{
        width: size,
        height: size,
        borderRadius: size * 0.27,
        backgroundColor: solid ? colors.black : colors.white,
        borderWidth: solid ? 0 : 1,
        borderColor: colors.black,
        alignItems: 'center',
        justifyContent: 'center',
      }}
    >
      <AgentMark kind={kind} size={Math.max(8, Math.round((size * 0.55) / 8) * 8)} />
    </View>
  );
}

export interface IllustrationProps {
  size?: number;
  color?: string;
}

/**
 * Minimal monochrome "agent at work" illustration used on onboarding —
 * a device slab with a node graph, echoing the reference's flat artwork.
 */
export function AgentIllustration({ size = 180, color = colors.black }: IllustrationProps) {
  return (
    <View
      accessible
      accessibilityRole="image"
      accessibilityLabel="Abstract illustration of connected AI agents"
    >
      <Svg width={size} height={size * 0.86} viewBox="0 0 200 172">
        {/* device slab */}
      <Rect x={34} y={26} width={132} height={94} rx={12} fill="none" stroke={color} strokeWidth={2} />
      <Rect x={34} y={26} width={132} height={20} rx={12} fill={color} />
      <Rect x={34} y={36} width={132} height={10} fill={color} />
      <Rect x={46} y={32} width={6} height={6} rx={3} fill="#FFFFFF" />
      <Rect x={58} y={32} width={6} height={6} rx={3} fill="#FFFFFF" />
      {/* node graph inside the slab */}
      <Path
        d="M64 78 L100 62 L136 82 L118 106 L78 104 Z"
        fill="none"
        stroke={color}
        strokeWidth={2}
        strokeLinejoin="round"
      />
      <Path d="M64 78 L118 106 M100 62 L78 104 M136 82 L64 78" fill="none" stroke={color} strokeWidth={1.2} />
      <Path d="M100 62 L136 82" fill="none" stroke={color} strokeWidth={1.2} />
      {/* nodes */}
      <Path d="M64 78a6 6 0 1 0 0-.1z" fill={color} />
      <Path d="M100 62a6 6 0 1 0 0-.1z" fill={color} />
      <Path d="M136 82a6 6 0 1 0 0-.1z" fill={color} />
      <Path d="M118 106a6 6 0 1 0 0-.1z" fill={color} />
      <Path d="M78 104a6 6 0 1 0 0-.1z" fill={color} />
      {/* stand */}
      <Path d="M84 120v14h32v-14" fill="none" stroke={color} strokeWidth={2} strokeLinejoin="round" />
      <Path d="M70 138h60" stroke={color} strokeWidth={3} strokeLinecap="round" />
      {/* orbit accents */}
      <Path d="M20 108c-8 14-6 32 6 44" fill="none" stroke={color} strokeWidth={1.4} strokeLinecap="round" />
      <Path d="M180 44c9 12 10 28 3 40" fill="none" stroke={color} strokeWidth={1.4} strokeLinecap="round" />
      <Path d="M14 60c4-8 11-14 20-16" fill="none" stroke={color} strokeWidth={1.4} strokeLinecap="round" />
      </Svg>
    </View>
  );
}
