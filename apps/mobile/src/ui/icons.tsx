/**
 * Monochrome stroke icon set (24×24 grid). One component, many names —
 * keeps the bundle small and the visual language consistent.
 */
import { Svg, Path } from 'react-native-svg';
import type { StyleProp, ViewStyle } from 'react-native';
import { colors } from '@/theme/tokens';

export type IconName =
  | 'home'
  | 'bot'
  | 'layers'
  | 'shield-check'
  | 'person'
  | 'search'
  | 'chevron-right'
  | 'chevron-left'
  | 'arrow-right'
  | 'arrow-left'
  | 'plus'
  | 'send'
  | 'stop'
  | 'check'
  | 'x'
  | 'terminal'
  | 'folder'
  | 'file'
  | 'bell'
  | 'lock'
  | 'monitor'
  | 'activity'
  | 'clock'
  | 'refresh'
  | 'link'
  | 'phone'
  | 'eye'
  | 'logout'
  | 'spark'
  | 'code'
  | 'grid'
  | 'globe'
  | 'chip'
  | 'sliders';

const PATHS: Record<IconName, readonly string[]> = {
  home: ['M4 10.6 12 4l8 6.6V19a1.5 1.5 0 0 1-1.5 1.5H15V15H9v5.5H5.5A1.5 1.5 0 0 1 4 19z'],
  bot: [
    'M12 3v2.5',
    'M5.5 7.5h13v9a2.5 2.5 0 0 1-2.5 2.5H8a2.5 2.5 0 0 1-2.5-2.5z',
    'M9.2 12h.01',
    'M14.8 12h.01',
  ],
  layers: ['M12 3.5 3.5 8 12 12.5 20.5 8z', 'M3.5 13.2 12 17.7l8.5-4.5', 'M3.5 17.6 12 22l8.5-4.4'],
  'shield-check': ['M12 3.2l7 2.9v5.1c0 4.4-2.9 7.9-7 9.6-4.1-1.7-7-5.2-7-9.6V6.1z', 'M9 12.2l2.2 2.2 4-4.3'],
  person: [
    'M12 12.2a4.1 4.1 0 1 0 0-8.2 4.1 4.1 0 0 0 0 8.2z',
    'M4.5 20.2c1.3-3.4 4-5.1 7.5-5.1s6.2 1.7 7.5 5.1',
  ],
  search: ['M11 17.8a6.8 6.8 0 1 0 0-13.6 6.8 6.8 0 0 0 0 13.6z', 'M20.5 20.5 16 16'],
  'chevron-right': ['M9.5 5.5 16 12l-6.5 6.5'],
  'chevron-left': ['M14.5 5.5 8 12l6.5 6.5'],
  'arrow-right': ['M4 12h14.5', 'M13 6l6 6-6 6'],
  'arrow-left': ['M20 12H5.5', 'M11 6l-6 6 6 6'],
  plus: ['M12 5v14', 'M5 12h14'],
  send: ['M21 3 10.5 13.5', 'M21 3l-6.6 18-3.9-7.5L3 9.6z'],
  stop: ['M7.5 7.5h9v9h-9z'],
  check: ['M5 12.6l4.6 4.6L19 7'],
  x: ['M6.5 6.5l11 11', 'M17.5 6.5l-11 11'],
  terminal: ['M5 7.2 10.2 12 5 16.8', 'M12.5 17H19'],
  folder: [
    'M4 7.5A2 2 0 0 1 6 5.5h3.4l2 2.6H18a2 2 0 0 1 2 2v6.4a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2z',
  ],
  file: ['M7 3.5h7l4 4v13H7z', 'M14 3.5v4h4'],
  bell: ['M6.2 16.2V11a5.8 5.8 0 0 1 11.6 0v5.2l1.4 2.3H4.8z', 'M10 20.4a2.2 2.2 0 0 0 4 0'],
  lock: ['M6.5 11h11v8.5h-11z', 'M8.8 11V8.2a3.2 3.2 0 0 1 6.4 0V11'],
  monitor: ['M4 5.5h16v9.5H4z', 'M9.5 19.5h5', 'M12 15v4.5'],
  activity: ['M3 12.2h3.8L9.4 6l4 12 2.5-5.8H21'],
  clock: ['M12 20.8a8.8 8.8 0 1 0 0-17.6 8.8 8.8 0 0 0 0 17.6z', 'M12 7.6V12l3 2'],
  refresh: ['M19.6 12a7.6 7.6 0 1 1-2.3-5.4', 'M19.6 4.4v4.4h-4.4'],
  link: [
    'M9.6 14.4l4.8-4.8',
    'M8.2 11.2 6.4 13a3.6 3.6 0 0 0 5.1 5.1l1.8-1.8',
    'M15.8 12.8l1.8-1.8a3.6 3.6 0 0 0-5.1-5.1l-1.8 1.8',
  ],
  phone: ['M7.5 3.5h9a1 1 0 0 1 1 1v15a1 1 0 0 1-1 1h-9a1 1 0 0 1-1-1v-15a1 1 0 0 1 1-1z', 'M11 17.6h2'],
  eye: [
    'M2.8 12S6.4 6.4 12 6.4 21.2 12 21.2 12 17.6 17.6 12 17.6 2.8 12 2.8 12z',
    'M12 15a3 3 0 1 0 0-6 3 3 0 0 0 0 6z',
  ],
  logout: ['M14 5.2H6.5a1 1 0 0 0-1 1v11.6a1 1 0 0 0 1 1H14', 'M18 12H9.8', 'M15.4 9 18.4 12l-3 3'],
  spark: ['M12 4v16', 'M5.2 8l13.6 8', 'M18.8 8 5.2 16'],
  code: ['M8.6 8 4.4 12l4.2 4', 'M15.4 8l4.2 4-4.2 4'],
  grid: ['M5 5h5.4v5.4H5z', 'M13.6 5H19v5.4h-5.4z', 'M5 13.6h5.4V19H5z', 'M13.6 13.6H19V19h-5.4z'],
  globe: [
    'M12 20.8a8.8 8.8 0 1 0 0-17.6 8.8 8.8 0 0 0 0 17.6z',
    'M3.4 12h17.2',
    'M12 3.2c2.4 2.6 3.6 5.5 3.6 8.8S14.4 18.2 12 20.8c-2.4-2.6-3.6-5.5-3.6-8.8S9.6 5.8 12 3.2z',
  ],
  chip: ['M8.4 8.4h7.2v7.2H8.4z', 'M12 3.6v4.8', 'M12 15.6v4.8', 'M3.6 12h4.8', 'M15.6 12h4.8'],
  sliders: [
    'M4 8h9',
    'M17.5 8H20',
    'M4 16h4.5',
    'M13 16h7',
    'M15.5 5.6v4.8',
    'M10.8 13.6v4.8',
  ],
};

export interface IconProps {
  name: IconName;
  size?: number;
  color?: string;
  strokeWidth?: number;
  style?: StyleProp<ViewStyle>;
}

export function Icon({ name, size = 20, color = colors.black, strokeWidth = 1.6, style }: IconProps) {
  return (
    <Svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      style={style}
    >
      {PATHS[name].map((d, i) => (
        <Path
          key={i}
          d={d}
          stroke={color}
          strokeWidth={strokeWidth}
          fill="none"
          strokeLinecap="round"
          strokeLinejoin="round"
        />
      ))}
    </Svg>
  );
}
