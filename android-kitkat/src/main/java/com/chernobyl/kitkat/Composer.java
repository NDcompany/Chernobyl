package com.chernobyl.kitkat;

import java.util.ArrayList;
import java.util.List;
import java.util.Locale;
import java.util.regex.Matcher;
import java.util.regex.Pattern;
import org.json.JSONArray;
import org.json.JSONObject;

/**
 * Pure helpers behind the composer (ADR-017), PC sessions (ADR-018), the
 * working row, git pull/push labels (ADR-016) and the terminal (ADR-020).
 * Ports of apps/mobile/src/utils/{composer,agentIdentity,terminal,workspace}.ts
 * and the bounds of packages/protocol. No Android imports: unit-tested on the JVM.
 */
public final class Composer {
    private Composer() {}

    /** packages/protocol: PROMPT_IMAGES_MAX, PROMPT_IMAGES_DATA_MAX, SESSION_TITLE_MAX, TERMINAL_COMMAND_MAX. */
    public static final int IMAGES_MAX = 4, IMAGES_DATA_MAX = 180_000, TITLE_MAX = 80, TERMINAL_COMMAND_MAX = 4096;
    /** The companion reads one WebSocket frame of at most this many bytes. */
    public static final int FRAME_MAX_BYTES = 256_000;
    /** Photos are shot/scaled to at most this long edge, re-encoded at falling JPEG quality. */
    public static final int SHOT_EDGE = 1280;
    public static final int[] SHOT_QUALITIES = {70, 50, 35, 20, 10};
    /** Terminal command lines kept per workspace on this tablet. */
    public static final int HISTORY_MAX = 20;

    /* ---------------- slash commands ---------------- */

    private static final Pattern SLASH = Pattern.compile("^/(\\S*)$");

    /** The partial command name while the text is "/name" (may be ""); null otherwise. */
    public static String slashQuery(String text) {
        Matcher m = SLASH.matcher(text);
        return m.matches() ? m.group(1) : null;
    }

    /** Names starting with the query first, then those containing it; at most `max`. */
    public static List<String> filterSlash(List<String> commands, String query, int max) {
        String q = query.toLowerCase(Locale.ROOT);
        List<String> out = new ArrayList<String>();
        for (String c : commands) if (c.toLowerCase(Locale.ROOT).startsWith(q)) out.add(c);
        if (!q.isEmpty()) for (String c : commands) {
            String l = c.toLowerCase(Locale.ROOT);
            if (!l.startsWith(q) && l.contains(q)) out.add(c);
        }
        return out.size() > max ? out.subList(0, max) : out;
    }

    /* ---------------- images and the frame ---------------- */

    public static int utf8Bytes(String s) {
        int n = 0;
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c < 0x80) n += 1;
            else if (c < 0x800) n += 2;
            else if (Character.isHighSurrogate(c) && i + 1 < s.length() && Character.isLowSurrogate(s.charAt(i + 1))) { n += 4; i++; }
            else n += 3;
        }
        return n;
    }

    /** The sealed WebSocket frame this JSON text becomes (+16 tag bytes, base64, envelope), rounded up. */
    public static int sealedFrameBytes(String json) {
        return (utf8Bytes(json) + 16 + 2) / 3 * 4 + 64;
    }

    /** Base64 characters still free for another photo. */
    public static int budgetLeft(List<String> images, int totalMax) {
        int used = 0;
        for (String d : images) used += d.length();
        return Math.max(0, totalMax - used);
    }

    /** The camera size to shoot at: the largest with long edge ≤ maxEdge, else the smallest. */
    public static int[] pickPictureSize(List<int[]> sizes, int maxEdge) {
        int[] fit = null, small = null;
        for (int[] s : sizes) {
            long area = (long) s[0] * s[1];
            if (Math.max(s[0], s[1]) <= maxEdge && (fit == null || area > (long) fit[0] * fit[1])) fit = s;
            if (small == null || area < (long) small[0] * small[1]) small = s;
        }
        return fit != null ? fit : small;
    }

    /** Power-of-two decode step that keeps the long edge at or above `maxEdge` (BitmapFactory inSampleSize). */
    public static int sampleSize(int w, int h, int maxEdge) {
        int s = 1;
        while (Math.max(w, h) / (s * 2) >= maxEdge) s *= 2;
        return s;
    }

    /** The size to scale to so the long edge is at most maxEdge (aspect kept). */
    public static int[] scaledSize(int w, int h, int maxEdge) {
        int edge = Math.max(w, h);
        if (edge <= maxEdge) return new int[] {w, h};
        return new int[] {Math.max(1, Math.round(w * (float) maxEdge / edge)), Math.max(1, Math.round(h * (float) maxEdge / edge))};
    }

    public interface Encoder {
        /** The photo as JPEG at this quality (0-100), base64 without line breaks. */
        String jpegBase64(int quality);
    }

    /** The first quality whose base64 is a JPEG within `budget`; null when even the lowest is too big. */
    public static String fitImage(Encoder e, int budget) {
        for (int q : SHOT_QUALITIES) {
            String d = e.jpegBase64(q);
            if (d != null && d.startsWith("/9j/") && d.length() <= budget) return d;
        }
        return null;
    }

    /* ---------------- session chips ---------------- */

    /** Context use as a whole percent (0-100), or -1 when unknown. */
    public static int contextPercent(JSONObject state) {
        JSONObject c = state == null ? null : state.optJSONObject("context");
        if (c == null) return -1;
        double max = c.optDouble("maxTokens", 0), used = c.optDouble("usedTokens", -1);
        if (!(max > 0) || !(used >= 0)) return -1;
        return (int) Math.min(100, Math.round(used / max * 100));
    }

    /** {value, label} pairs: the CLI's list, else Claude Code's aliases. */
    public static List<String[]> modelChoices(JSONObject state) {
        List<String[]> out = new ArrayList<String[]>();
        JSONArray ms = state == null ? null : state.optJSONArray("models");
        for (int i = 0; ms != null && i < ms.length(); i++) {
            JSONObject m = ms.optJSONObject(i);
            if (m != null && m.opt("value") instanceof String) out.add(new String[] {m.optString("value"), m.optString("label", m.optString("value"))});
        }
        if (out.isEmpty()) {
            out.add(new String[] {"default", "Default"});
            out.add(new String[] {"sonnet", "Sonnet"});
            out.add(new String[] {"opus", "Opus"});
            out.add(new String[] {"haiku", "Haiku"});
        }
        return out;
    }

    /** New session's Model row (ADR-021/022): Default (no model sent), then the agent's own list when it sends one, else Claude Code's aliases. */
    public static List<String[]> newSessionModels(JSONObject live) {
        List<String[]> out = modelChoices(live);
        if (live == null || live.optJSONArray("models") == null || live.optJSONArray("models").length() == 0) return out;
        List<String[]> list = new ArrayList<String[]>();
        list.add(new String[] {"default", "Default (the PC's setting)"});
        for (String[] m : out) if (!"default".equals(m[0])) list.add(m);
        return list;
    }

    private static final Pattern MODEL_ID = Pattern.compile("^claude-([a-z]+)-(\\d+)(?:-(\\d{1,2}))?(?:-\\d{8})?$");

    /** "claude-haiku-5-5" → "Haiku 5.5"; a listed value → its label; empty → "Model". */
    public static String modelLabel(String model, List<String[]> choices) {
        if (model == null || model.isEmpty()) return "Model";
        for (String[] c : choices) if (c[0].equals(model)) return c[1];
        Matcher m = MODEL_ID.matcher(model);
        if (m.matches()) return Character.toUpperCase(m.group(1).charAt(0)) + m.group(1).substring(1) + " " + m.group(2) + (m.group(3) != null ? "." + m.group(3) : "");
        return model;
    }

    /** The only modes the tablet may set (ADR-015 §3.4); never a bypass mode. */
    public static boolean isModeChoice(String mode) {
        return "default".equals(mode) || "acceptEdits".equals(mode) || "plan".equals(mode);
    }

    public static String modeLabel(String mode) {
        if ("default".equals(mode)) return "Ask before edits";
        if ("acceptEdits".equals(mode)) return "Accept edits";
        if ("plan".equals(mode)) return "Plan";
        return mode == null || mode.isEmpty() ? "Permission mode" : mode;
    }

    public static String modeHint(String mode) {
        if ("default".equals(mode)) return "Asks before editing files or running commands.";
        if ("acceptEdits".equals(mode)) return "Applies file edits without asking; commands still ask.";
        return "Reads and plans only; nothing changes until you approve the plan.";
    }

    /** {connected, enabled total, attention 0/1}: attention = a server needs sign-in or failed. */
    public static int[] mcpSummary(JSONArray servers) {
        int connected = 0, total = 0, attention = 0;
        for (int i = 0; servers != null && i < servers.length(); i++) {
            JSONObject s = servers.optJSONObject(i);
            if (s == null || "disabled".equals(s.optString("status"))) continue;
            total++;
            if ("connected".equals(s.optString("status"))) connected++;
            if ("needs-auth".equals(s.optString("status")) || "failed".equals(s.optString("status"))) attention = 1;
        }
        return new int[] {connected, total, attention};
    }

    /** Client-side precheck of a pasted OAuth callback; the companion checks it fully. */
    public static boolean isCallbackUrl(String url, String prefix) {
        String u = url.trim();
        return u.length() <= 4096 && u.startsWith("http://") && (prefix == null || prefix.isEmpty() || u.startsWith(prefix));
    }

    /** 1-80 UTF-16 units, not blank, no control characters. */
    public static boolean isSessionTitle(String t) {
        if (t == null || t.length() < 1 || t.length() > TITLE_MAX || t.trim().isEmpty()) return false;
        for (int i = 0; i < t.length(); i++) {
            char c = t.charAt(i);
            if (c <= 0x1f || (c >= 0x7f && c <= 0x9f)) return false;
        }
        return true;
    }

    /* ---------------- working row (agentIdentity.ts) ---------------- */

    static final String[] VERBS = {"Thinking", "Pondering", "Brewing", "Reticulating", "Mulling", "Tinkering", "Noodling",
        "Percolating", "Untangling", "Sketching", "Simmering", "Distilling", "Conjuring", "Whittling", "Puzzling", "Assembling",
        "Calibrating", "Composing", "Sifting", "Kneading", "Weaving", "Musing"};
    static final long VERB_MS = 3000;

    /** The verb for this moment of a turn; the turn id picks where each turn starts. */
    public static String workingVerb(long elapsedMs, String seed) {
        long h = 0;
        for (int i = 0; i < seed.length(); i++) h = (h * 31 + seed.charAt(i)) & 0xffffffffL;
        long step = Math.max(0, elapsedMs) / VERB_MS;
        return VERBS[(int) ((h + step) % VERBS.length)];
    }

    /** What the newest running turn does: {state thinking|working|waiting, tool or null, turn}; null when none runs. */
    public static Object[] workingStatus(Model.View v, List<JSONObject> pending) {
        JSONObject turn = null;
        for (JSONObject t : v.turns.values())
            if ("running".equals(t.optString("status")) && (turn == null || t.optLong("startedAt") >= turn.optLong("startedAt"))) turn = t;
        if (turn == null) return null;
        if (!pending.isEmpty()) return new Object[] {"waiting", null, turn};
        JSONObject last = null;
        for (int i = v.items.size() - 1; i >= 0 && last == null; i--) if (turn.optString("id").equals(v.items.get(i).optString("turnId"))) last = v.items.get(i);
        JSONObject tool = last == null ? null : last.optJSONObject("tool");
        if (tool != null) {
            String st = tool.optString("status");
            if (st.equals("awaiting-approval")) return new Object[] {"waiting", null, turn};
            if (st.equals("running") || st.equals("pending")) return new Object[] {"working", tool.optString("name"), turn};
        }
        return new Object[] {"thinking", null, turn};
    }

    /** "Pondering…", "Running Bash…" or "Waiting for you". */
    public static String workingLabel(String state, long elapsedMs, String seed, String tool) {
        if ("waiting".equals(state)) return "Waiting for you";
        if ("working".equals(state)) return "Running " + (tool == null || tool.isEmpty() ? "a tool" : tool) + "…";
        return workingVerb(elapsedMs, seed) + "…";
    }

    /** "8s", "1m 05s", "1h 02m". */
    public static String formatElapsed(long ms) {
        long s = Math.max(0, ms) / 1000;
        if (s < 60) return s + "s";
        if (s < 3600) return (s / 60) + "m " + String.format(Locale.US, "%02d", s % 60) + "s";
        return (s / 3600) + "h " + String.format(Locale.US, "%02d", (s / 60) % 60) + "m";
    }

    /* ---------------- git (workspace.ts) ---------------- */

    /** {label, "1" if possible}: "origin/main → main", or why there is nothing to pull from. */
    public static String[] pullSource(JSONObject s) {
        if (s.isNull("branch") || !s.has("branch")) return new String[] {"Detached HEAD: check out a branch on the PC", null};
        if (!s.has("upstream") || s.optString("upstream").isEmpty()) return new String[] {s.optString("branch") + " has no upstream: set one on the PC", null};
        return new String[] {s.optString("upstream") + " → " + s.optString("branch"), "1"};
    }

    /** {label, "1" if possible}: "main → origin/main", or why there is nowhere to push. */
    public static String[] pushTarget(JSONObject s) {
        if (s.isNull("branch") || !s.has("branch")) return new String[] {"Detached HEAD: check out a branch on the PC", null};
        if (!s.has("upstream") || s.optString("upstream").isEmpty()) return new String[] {s.optString("branch") + " has no upstream: set one on the PC", null};
        return new String[] {s.optString("branch") + " → " + s.optString("upstream"), "1"};
    }

    /**
     * After a Pull: the warning for changed agent configuration (`agentConfigChanged` on the
     * command_result), or null when none changed. At most five paths are named.
     */
    public static String agentConfigWarning(JSONArray paths) {
        if (paths == null || paths.length() == 0) return null;
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < paths.length() && i < 5; i++) sb.append(i > 0 ? ", " : "").append(paths.optString(i));
        String more = paths.length() > 5 ? " and " + (paths.length() - 5) + " more" : "";
        return "This pull changed agent settings: " + sb + more + ". Agents in this workspace follow them, "
                + "including tool permissions and hooks \u2014 review them on your PC.";
    }

    /* ---------------- tls ---------------- */

    /**
     * The TLS suites to enable: of the platform's supported ones only ECDHE with AES-GCM or
     * ChaCha20 (and TLS 1.3's own AES / ChaCha20 suites). Empty when the platform offers none:
     * the caller then keeps its current list, so the connection still works.
     */
    public static String[] tlsSuites(String[] supported) {
        List<String> out = new ArrayList<String>();
        for (String c : supported) {
            boolean modern = c.contains("_ECDHE_") && (c.contains("_GCM_") || c.contains("CHACHA20"));
            if (modern || c.startsWith("TLS_AES_") || c.startsWith("TLS_CHACHA20")) out.add(c);
        }
        return out.toArray(new String[0]);
    }

    /* ---------------- terminal (terminal.ts, data/terminal.ts) ---------------- */

    /** `line` first, without an earlier copy, at most HISTORY_MAX; a blank line changes nothing. */
    public static List<String> remember(List<String> history, String line) {
        List<String> out = new ArrayList<String>();
        String l = line.trim();
        if (!l.isEmpty()) out.add(l);
        for (String h : history) if (!h.equals(l) && out.size() < HISTORY_MAX) out.add(h);
        return out;
    }

    /** One line for the end of a run: exit code, timeout or stop, and the output cap. */
    public static String exitLabel(JSONObject exit) {
        String reason = exit.optString("reason");
        String why = reason.equals("timeout") ? "Timed out after 120 s, stopped" : reason.equals("stopped") ? "Stopped"
                : exit.isNull("exitCode") || !exit.has("exitCode") ? "Ended without an exit code" : "Exit code " + exit.optInt("exitCode");
        return exit.optBoolean("truncated") ? why + " · output cut at 1 MiB" : why;
    }

    public static boolean exitOk(JSONObject exit) {
        return "exit".equals(exit.optString("reason")) && exit.has("exitCode") && !exit.isNull("exitCode") && exit.optInt("exitCode") == 0;
    }

    /** One terminal run as the screen shows it, folded from its messages (matched by commandId). */
    public static final class TerminalRun {
        public final String commandId, command;
        public final StringBuilder output = new StringBuilder();
        public JSONObject exit;
        public boolean running = true;
        public String failed;

        public TerminalRun(String commandId, String command) {
            this.commandId = commandId;
            this.command = command;
        }

        /** Folds one server message; true when it changed this run. */
        public boolean apply(JSONObject m) {
            if (!commandId.equals(m.optString("commandId"))) return false;
            String type = m.optString("type");
            if (type.equals("terminal_output")) {
                output.append(m.optString("data"));
                return true;
            }
            if (type.equals("terminal_exit")) {
                exit = m;
                running = false;
                return true;
            }
            if (type.equals("command_result") && exit == null) {
                // after a reconnect the output and terminal_exit are gone; the outcome is not
                running = false;
                failed = "The connection dropped during the run; it ended: " + m.optString("error", m.optString("status"));
                return true;
            }
            return false;
        }

        /** The ack refused the run, or none came. */
        public void refused(String error) {
            running = false;
            failed = error != null ? error : "Not run: the outcome is unknown or nothing was sent";
        }
    }
}
