/**
 * ADR-018 pure helpers for the Terminal screen (unit-tested in terminal.test.ts).
 */
import type { ServerMessage } from '@chernobyl/protocol';

type TerminalExit = Extract<ServerMessage, { type: 'terminal_exit' }>;

/** Command lines kept per workspace, on this phone only. */
export const HISTORY_MAX = 20;

/** `line` first, without an earlier copy, at most HISTORY_MAX; a blank line changes nothing. */
export function remember(history: readonly string[], line: string, max = HISTORY_MAX): string[] {
  const l = line.trim();
  if (!l) return [...history];
  return [l, ...history.filter((h) => h !== l)].slice(0, max);
}

/** One line for the end of a run: exit code, timeout or stop, and the output cap. */
export function exitLabel(exit: Pick<TerminalExit, 'exitCode' | 'reason' | 'truncated'>, timeoutS = 120): string {
  const why =
    exit.reason === 'timeout'
      ? `Timed out after ${timeoutS} s, stopped`
      : exit.reason === 'stopped'
        ? 'Stopped'
        : exit.exitCode === null
          ? 'Ended without an exit code'
          : `Exit code ${exit.exitCode}`;
  return exit.truncated ? `${why} · output cut at 1 MiB` : why;
}

/** A clean exit (code 0); everything else is shown in the error colour. */
export function exitOk(exit: Pick<TerminalExit, 'exitCode' | 'reason'>): boolean {
  return exit.reason === 'exit' && exit.exitCode === 0;
}
