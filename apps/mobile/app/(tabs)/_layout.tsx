import { Redirect, Tabs } from 'expo-router';
import { BottomNav } from '@/components/BottomNav';
import { useAppStore } from '@/store/appStore';

export default function TabsLayout() {
  const onboarded = useAppStore((s) => s.onboarded);
  const authed = useAppStore((s) => s.authed);

  if (!onboarded) return <Redirect href="/onboarding" />;
  if (!authed) return <Redirect href="/sign-in" />;

  return (
    <Tabs
      screenOptions={{ headerShown: false }}
      tabBar={(props) => <BottomNav {...props} />}
    >
      <Tabs.Screen name="index" options={{ title: 'Home' }} />
      <Tabs.Screen name="agents" options={{ title: 'Agents' }} />
      <Tabs.Screen name="sessions" options={{ title: 'Sessions' }} />
      <Tabs.Screen name="approvals" options={{ title: 'Approvals' }} />
      <Tabs.Screen name="profile" options={{ title: 'Profile' }} />
    </Tabs>
  );
}
