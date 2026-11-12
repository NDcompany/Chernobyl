/**
 * Agent identity marks: one ORIGINAL 8×8 pixel character per runtime family,
 * drawn here (never a company's logo or mascot) and keyed by `Agent.kind`.
 * The 8-cell grid lands on whole pixels at 16, 24, 32 and 48 px. Eyes and
 * gaps are holes, so the surface behind (white card, black tile or header)
 * shows through. The name is always written next to the mark in text.
 *
 * States: idle/done = still; thinking = slow bob + sparkle; working = quicker
 * bob + sparkle; waiting = calm fade pulse; error = dimmed. Motion is
 * transform/opacity only on the native driver (no JS frames, no re-renders),
 * and reduced motion (OS or app setting) always gets the still frame.
 */
import { memo, useEffect, useRef } from 'react';
import { Animated, Easing, Platform, View } from 'react-native';
import { Svg, Path } from 'react-native-svg';
import { agentColors } from '@/theme/tokens';
import { useReducedMotion } from '@/hooks/useReducedMotion';
import { markFor, type MarkId, type MarkState } from '@/utils/agentIdentity';

export type { MarkState } from '@/utils/agentIdentity';

/** '#' = filled cell. Original characters: ember, bot, braces, saucer, blob. */
const SPRITES: Record<MarkId, readonly string[]> = {
  // Claude Code: a diamond-shaped ember with two eyes
  claude: ['...##...', '..####..', '.######.', '##.##.##', '########', '.######.', '..####..', '...##...'],
  // Codex: a little antenna bot with a mouth and feet
  codex: ['...##...', '.######.', '.#.##.#.', '.######.', '.##..##.', '.######.', '..#..#..', '.##..##.'],
  // OpenCode: curly braces around a dot
  opencode: ['.##..##.', '.#....#.', '.#....#.', '#..##..#', '#..##..#', '.#....#.', '.#....#.', '.##..##.'],
  // Antigravity: a little flying saucer with two eyes, hovering on its beam
  antigravity: ['..####..', '.#.##.#.', '########', '#.#..#.#', '########', '..#..#..', '.#....#.', '#......#'],
  // anything else: a round blob
  generic: ['..####..', '.######.', '##.##.##', '########', '########', '########', '.######.', '.#.##.#.'],
};

/** Each row's runs of '#' in one path: one fill, so no hairline seams between cells. */
function spritePath(rows: readonly string[]): string {
  let d = '';
  rows.forEach((row, y) => {
    for (let x = 0; x < row.length; x++) {
      if (row[x] !== '#') continue;
      let w = 1;
      while (row[x + w] === '#') w++;
      d += `M${x} ${y}h${w}v1h${-w}z`;
      x += w - 1;
    }
  });
  return d;
}

const PATHS: Record<MarkId, string> = {
  claude: spritePath(SPRITES.claude),
  codex: spritePath(SPRITES.codex),
  opencode: spritePath(SPRITES.opencode),
  antigravity: spritePath(SPRITES.antigravity),
  generic: spritePath(SPRITES.generic),
};

const SPARKLE = 'M4 0L5 3L8 4L5 5L4 8L3 5L0 4L3 3Z';
const NATIVE = Platform.OS !== 'web';
/** full loop length per animated state */
const PERIOD: Partial<Record<MarkState, number>> = { thinking: 1600, working: 800, waiting: 2400 };

export interface AgentMarkProps {
  /** `Agent.kind`; unknown or missing kinds get the generic mark */
  kind?: string;
  /** px; multiples of 8 stay pixel-crisp */
  size?: number;
  state?: MarkState;
}

/** Decorative: hidden from screen readers; say the agent's name in text beside it. */
export const AgentMark = memo(function AgentMark({ kind, size = 24, state = 'idle' }: AgentMarkProps) {
  const reduced = useReducedMotion();
  const id = markFor(kind);
  const color = agentColors[id];
  const period = reduced ? undefined : PERIOD[state];
  const t = useRef(new Animated.Value(0)).current;

  useEffect(() => {
    t.setValue(0);
    if (!period) return;
    const half = { duration: period / 2, easing: Easing.inOut(Easing.sin), useNativeDriver: NATIVE };
    const loop = Animated.loop(
      Animated.sequence([Animated.timing(t, { toValue: 1, ...half }), Animated.timing(t, { toValue: 0, ...half })]),
    );
    loop.start();
    return () => loop.stop();
  }, [period, t]);

  const motion =
    !period
      ? undefined
      : state === 'waiting'
        ? { opacity: t.interpolate({ inputRange: [0, 1], outputRange: [1, 0.4] }) }
        : { transform: [{ translateY: t.interpolate({ inputRange: [0, 1], outputRange: [0, -size / 8] }) }] };
  const spark = size * 0.375;

  return (
    <View
      pointerEvents="none"
      accessibilityElementsHidden
      importantForAccessibility="no-hide-descendants"
      style={{ width: size, height: size, opacity: state === 'error' ? 0.45 : 1 }}
    >
      <Animated.View style={motion}>
        <Svg width={size} height={size} viewBox="0 0 8 8">
          <Path d={PATHS[id]} fill={color} />
        </Svg>
      </Animated.View>
      {period && state !== 'waiting' ? (
        <Animated.View
          style={{
            position: 'absolute',
            top: -spark / 3,
            right: -spark / 3,
            opacity: t,
            transform: [{ scale: t.interpolate({ inputRange: [0, 1], outputRange: [0.4, 1] }) }],
          }}
        >
          <Svg width={spark} height={spark} viewBox="0 0 8 8">
            <Path d={SPARKLE} fill={color} />
          </Svg>
        </Animated.View>
      ) : null}
    </View>
  );
});
