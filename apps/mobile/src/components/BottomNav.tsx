/** Mobile bottom navigation: monochrome icons, bold active state, live approvals badge. */
import { Pressable, StyleSheet, View } from 'react-native';
import { useSafeAreaInsets } from 'react-native-safe-area-context';
import type { BottomTabBarProps } from 'expo-router/js-tabs';
import { colors, spacing } from '@/theme/tokens';
import { Icon, type IconName } from '@/ui/icons';
import { Text } from '@/ui/primitives';
import { useSnapshot } from '@/data/state';

const ITEMS: { name: string; label: string; icon: IconName }[] = [
  { name: 'index', label: 'Home', icon: 'home' },
  { name: 'agents', label: 'Agents', icon: 'bot' },
  { name: 'sessions', label: 'Sessions', icon: 'layers' },
  { name: 'approvals', label: 'Approvals', icon: 'shield-check' },
  { name: 'profile', label: 'Profile', icon: 'person' },
];

export function BottomNav({ state, navigateToTab }: BottomTabBarProps) {
  const snapshot = useSnapshot();
  const insets = useSafeAreaInsets();
  const pending = snapshot.permissions.filter((p) => p.status === 'pending').length;

  return (
    <View style={[styles.bar, { paddingBottom: Math.max(insets.bottom, 10) }]}>
      {state.routes.map((route, index) => {
        const item = ITEMS.find((x) => x.name === route.name);
        if (!item) return null;
        const focused = state.index === index;
        const badge = item.name === 'approvals' && pending > 0 ? pending : undefined;

        return (
          <Pressable
            key={route.key}
            focusable
            accessible
            accessibilityRole="button"
            accessibilityState={focused ? { selected: true } : {}}
            accessibilityLabel={badge ? `${item.label}, ${badge} pending` : item.label}
            onPress={() => {
              if (!focused) navigateToTab(route.key);
            }}
            style={styles.item}
          >
            <View>
              <Icon
                name={item.icon}
                size={22}
                color={focused ? colors.black : colors.text3}
                strokeWidth={focused ? 1.9 : 1.5}
              />
              {badge ? (
                <View style={styles.badge}>
                  <Text variant="xs" color={colors.white} style={{ fontWeight: '700' }}>
                    {badge > 9 ? '9+' : badge}
                  </Text>
                </View>
              ) : null}
            </View>
            <Text
              variant="xs"
              color={focused ? colors.black : colors.text3}
              style={{ fontWeight: focused ? '700' : '500' }}
            >
              {item.label}
            </Text>
          </Pressable>
        );
      })}
    </View>
  );
}

const styles = StyleSheet.create({
  bar: {
    flexDirection: 'row',
    backgroundColor: colors.white,
    borderTopWidth: StyleSheet.hairlineWidth,
    borderTopColor: colors.border,
    paddingTop: spacing.sm,
  },
  item: {
    flex: 1,
    alignItems: 'center',
    justifyContent: 'center',
    gap: 3,
    paddingVertical: 2,
  },
  badge: {
    position: 'absolute',
    top: -6,
    right: -10,
    minWidth: 17,
    height: 17,
    borderRadius: 9,
    backgroundColor: colors.black,
    alignItems: 'center',
    justifyContent: 'center',
    paddingHorizontal: 4,
  },
});
