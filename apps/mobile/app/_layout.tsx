// first: the E2E handshake needs crypto.getRandomValues (Hermes has none)
import '@/platform/random';
import { Stack } from 'expo-router';
import { StatusBar } from 'expo-status-bar';
import { QueryClientProvider } from '@tanstack/react-query';
import { SafeAreaProvider } from 'react-native-safe-area-context';
import { queryClient } from '@/data/state';
import { TransportProvider } from '@/data/TransportProvider';
// registers the Android headless task before any service start (ADR-011 §4)
import '@/platform/connectionService';
import { ToastHost, SimulationBadge } from '@/ui/surface';
import { colors } from '@/theme/tokens';

export default function RootLayout() {
  return (
    <QueryClientProvider client={queryClient}>
      <SafeAreaProvider>
        <TransportProvider>
          <Stack
            screenOptions={{
              headerShown: false,
              contentStyle: { backgroundColor: colors.white },
            }}
          >
            <Stack.Screen name="(tabs)" />
            <Stack.Screen name="onboarding" options={{ gestureEnabled: false }} />
            <Stack.Screen name="sign-in" options={{ gestureEnabled: false }} />
            <Stack.Screen
              name="connect-host"
              options={{ presentation: 'modal', gestureEnabled: true }}
            />
            <Stack.Screen
              name="new-task"
              options={{ presentation: 'modal', gestureEnabled: true }}
            />
            <Stack.Screen name="agent/[id]" />
            <Stack.Screen name="session/[id]" />
            <Stack.Screen name="permission/[id]" />
            <Stack.Screen name="settings/[section]" />
            <Stack.Screen name="files/[workspace]" />
            <Stack.Screen name="file/[workspace]" />
            <Stack.Screen name="changes/[workspace]" />
          </Stack>
          <StatusBar style="dark" />
          <SimulationBadge />
          <ToastHost />
        </TransportProvider>
      </SafeAreaProvider>
    </QueryClientProvider>
  );
}
