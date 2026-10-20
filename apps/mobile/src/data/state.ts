/**
 * Server-state wiring: one TanStack Query holds the snapshot; transport
 * events merge into its cache through the reducer. Screens read with
 * useSnapshot() and write through useCommands().
 */
import { QueryClient, useQuery } from '@tanstack/react-query';
import { createSeedSnapshot, type ServerMessage, type Snapshot } from '@chernobyl/protocol';
import { applyServerMessage } from './reducer';
import { EMPTY_SNAPSHOT } from './selectors';

export const STATE_KEY = ['chernobyl-state'] as const;

export const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      staleTime: Infinity,
      gcTime: Infinity,
      retry: false,
      refetchOnMount: false,
      refetchOnWindowFocus: false,
    },
  },
});

// seed once so the first paint has realistic data (the host hello_ack replaces it)
queryClient.setQueryData<Snapshot>(STATE_KEY, createSeedSnapshot());

export function useSnapshot(): Snapshot {
  const { data } = useQuery<Snapshot>({
    queryKey: STATE_KEY,
    queryFn: async () => queryClient.getQueryData<Snapshot>(STATE_KEY) ?? createSeedSnapshot(),
  });
  return data ?? EMPTY_SNAPSHOT;
}

export function applyMessage(msg: ServerMessage): void {
  queryClient.setQueryData<Snapshot>(STATE_KEY, (prev) =>
    applyServerMessage(prev ?? createSeedSnapshot(), msg),
  );
}
