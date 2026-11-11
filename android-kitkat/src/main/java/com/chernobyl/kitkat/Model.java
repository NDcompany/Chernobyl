package com.chernobyl.kitkat;

import java.util.ArrayList;
import java.util.Collections;
import java.util.Comparator;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import org.json.JSONArray;
import org.json.JSONObject;

/**
 * App state folded from the companion's messages: the snapshot reducer
 * (apps/mobile/src/data/reducer.ts), its selectors, and one transcript view per
 * open conversation (packages/protocol/src/transcript.ts). Memory only: nothing
 * here is ever persisted (threat model T2). Main thread only.
 */
public final class Model {
    public final List<JSONObject> hosts = new ArrayList<JSONObject>();
    public final List<JSONObject> agents = new ArrayList<JSONObject>();
    public final List<JSONObject> sessions = new ArrayList<JSONObject>();
    public final List<JSONObject> permissions = new ArrayList<JSONObject>();
    public final List<JSONObject> activity = new ArrayList<JSONObject>();
    public final List<String> features = new ArrayList<String>();
    /** A hello_ack arrived on this connection (before it: nothing to show yet). */
    public boolean synced;
    public final Map<String, View> views = new HashMap<String, View>();

    public void clear() {
        hosts.clear();
        agents.clear();
        sessions.clear();
        permissions.clear();
        activity.clear();
        features.clear();
        views.clear();
        synced = false;
    }

    private static void fill(List<JSONObject> list, JSONArray arr) {
        list.clear();
        if (arr == null) return;
        for (int i = 0; i < arr.length(); i++) {
            JSONObject o = arr.optJSONObject(i);
            if (o != null && o.opt("id") instanceof String) list.add(o);
        }
    }

    /** reducer.ts upsert: replace in place, or insert at the front. */
    static void upsert(List<JSONObject> list, JSONObject item) {
        if (item == null || !(item.opt("id") instanceof String)) return;
        String id = item.optString("id");
        for (int i = 0; i < list.size(); i++) {
            if (id.equals(list.get(i).optString("id"))) {
                list.set(i, item);
                return;
            }
        }
        list.add(0, item);
    }

    /** Folds one server message; returns false for messages that change nothing here. */
    public boolean apply(JSONObject m) {
        String type = m.optString("type");
        switch (type) {
            case "hello_ack":
            case "state": {
                JSONObject s = m.optJSONObject("snapshot");
                if (s == null) return false;
                fill(hosts, s.optJSONArray("hosts"));
                fill(agents, s.optJSONArray("agents"));
                fill(sessions, s.optJSONArray("sessions"));
                fill(permissions, s.optJSONArray("permissions"));
                fill(activity, s.optJSONArray("activity"));
                if (type.equals("hello_ack")) {
                    features.clear();
                    JSONArray f = m.optJSONArray("features");
                    if (f != null) for (int i = 0; i < f.length(); i++) if (f.opt(i) instanceof String) features.add(f.optString(i));
                    synced = true;
                }
                return true;
            }
            case "host_updated":
                upsert(hosts, m.optJSONObject("host"));
                return true;
            case "agent_updated":
                upsert(agents, m.optJSONObject("agent"));
                return true;
            case "session_created":
            case "session_updated":
                upsert(sessions, m.optJSONObject("session"));
                return true;
            case "session_output": {
                JSONObject s = byId(sessions, m.optString("sessionId"));
                if (s == null) return false;
                try {
                    JSONArray out = s.optJSONArray("output");
                    if (out == null) out = new JSONArray();
                    JSONArray lines = m.optJSONArray("lines");
                    if (lines != null) for (int i = 0; i < lines.length(); i++) out.put(lines.opt(i));
                    JSONArray keep = new JSONArray();
                    for (int i = Math.max(0, out.length() - 300); i < out.length(); i++) keep.put(out.opt(i));
                    s.put("output", keep);
                    s.put("progress", m.optInt("progress", s.optInt("progress")));
                    if (m.opt("status") instanceof String) s.put("status", m.optString("status"));
                    s.put("lastActivityAt", System.currentTimeMillis());
                } catch (Exception ignored) {
                    return false;
                }
                return true;
            }
            case "permission_requested":
            case "permission_resolved":
                upsert(permissions, m.optJSONObject("permission"));
                return true;
            case "activity": {
                JSONObject e = m.optJSONObject("event");
                if (e == null) return false;
                activity.add(0, e);
                while (activity.size() > 80) activity.remove(activity.size() - 1);
                return true;
            }
            case "transcript":
            case "transcript_snapshot":
                return true; // folded per view by the caller (needs resubscribe handling)
            default:
                return false;
        }
    }

    /* ---------------- selectors (selectors.ts) ---------------- */

    public static JSONObject byId(List<JSONObject> list, String id) {
        if (id == null) return null;
        for (JSONObject o : list) if (id.equals(o.optString("id"))) return o;
        return null;
    }

    public JSONObject agent(String id) {
        return byId(agents, id);
    }

    public JSONObject session(String id) {
        return byId(sessions, id);
    }

    public JSONObject permission(String id) {
        return byId(permissions, id);
    }

    public static boolean isPending(JSONObject p, long now) {
        return "pending".equals(p.optString("status")) && now < p.optLong("expiresAt", Long.MAX_VALUE);
    }

    public List<JSONObject> pending(long now) {
        List<JSONObject> out = new ArrayList<JSONObject>();
        for (JSONObject p : permissions) if (isPending(p, now)) out.add(p);
        return out;
    }

    /** Pending requests of one session. */
    public List<JSONObject> pendingFor(String sessionId, long now) {
        List<JSONObject> out = new ArrayList<JSONObject>();
        for (JSONObject p : permissions) if (isPending(p, now) && sessionId.equals(p.optString("sessionId"))) out.add(p);
        return out;
    }

    public List<JSONObject> sessionsByActivity() {
        List<JSONObject> out = new ArrayList<JSONObject>(sessions);
        Collections.sort(out, new Comparator<JSONObject>() {
            @Override
            public int compare(JSONObject a, JSONObject b) {
                long x = a.optLong("lastActivityAt", a.optLong("createdAt")), y = b.optLong("lastActivityAt", b.optLong("createdAt"));
                return x < y ? 1 : x > y ? -1 : 0;
            }
        });
        return out;
    }

    public int count(List<JSONObject> list, String field, String value) {
        int n = 0;
        for (JSONObject o : list) if (value.equals(o.optString(field))) n++;
        return n;
    }

    public boolean hasSimulated() {
        for (JSONObject h : hosts) if (h.optBoolean("simulated")) return true;
        for (JSONObject a : agents) if (a.optBoolean("simulated")) return true;
        return false;
    }

    public boolean has(String feature) {
        return features.contains(feature);
    }

    /** All workspaces any live agent advertises, keyed by id (ADR-016 workspace ids). */
    public List<JSONObject> workspaces() {
        List<JSONObject> out = new ArrayList<JSONObject>();
        List<String> seen = new ArrayList<String>();
        for (JSONObject a : agents) {
            JSONObject live = a.optJSONObject("live");
            JSONArray ws = live == null ? null : live.optJSONArray("workspaces");
            if (ws == null) continue;
            for (int i = 0; i < ws.length(); i++) {
                JSONObject w = ws.optJSONObject(i);
                if (w != null && !seen.contains(w.optString("id"))) {
                    seen.add(w.optString("id"));
                    out.add(w);
                }
            }
        }
        return out;
    }

    /* ---------------- transcript view (transcript.ts applyTranscript) ---------------- */

    public static final class View {
        /** '' until the first snapshot. */
        public String epoch = "";
        public long seq;
        public final List<JSONObject> items = new ArrayList<JSONObject>();
        public final Map<String, Integer> index = new HashMap<String, Integer>();
        public final Map<String, JSONObject> turns = new HashMap<String, JSONObject>();
        public JSONObject state = new JSONObject();
        public boolean truncatedBefore;
        /** The companion refused the subscription. */
        public String error;
        /** Bumps on every change (cheap redraw check). */
        public int version;
    }

    static boolean renderable(JSONObject raw) {
        String k = raw.optString("kind");
        if (k.equals("user") || k.equals("text") || k.equals("thinking") || k.equals("notice")) return raw.opt("text") instanceof String;
        if (k.equals("tool")) return raw.optJSONObject("tool") != null && raw.optJSONObject("tool").opt("name") instanceof String;
        if (k.equals("plan")) return raw.optJSONArray("steps") != null;
        return false;
    }

    /** Known well-formed kinds as-is; anything else kept (order, ids) but flagged `unknown`. */
    static JSONObject toViewItem(Object o) {
        if (!(o instanceof JSONObject)) return null;
        JSONObject raw = (JSONObject) o;
        if (!(raw.opt("id") instanceof String) || raw.optString("id").isEmpty()) return null;
        if (renderable(raw)) return raw;
        try {
            JSONObject u = new JSONObject(raw.toString());
            u.put("originalKind", String.valueOf(raw.opt("kind")));
            u.put("kind", "unknown");
            return u;
        } catch (Exception e) {
            return null;
        }
    }

    static void put(View v, JSONObject item) {
        Integer at = v.index.get(item.optString("id"));
        if (at == null) {
            v.index.put(item.optString("id"), v.items.size());
            v.items.add(item);
        } else {
            v.items.set(at, item);
        }
    }

    static void grow(JSONObject item, String field, String text) {
        try {
            String k = item.optString("kind");
            if (k.equals("user") || k.equals("text") || k.equals("thinking") || k.equals("notice")) {
                if (field.equals("text")) item.put("text", item.optString("text") + text);
            } else if (k.equals("tool") && (field.equals("input") || field.equals("output"))) {
                JSONObject t = item.getJSONObject("tool");
                t.put(field, t.optString(field, "") + text);
            }
        } catch (Exception ignored) {
            // plans and unknown items take no deltas
        }
    }

    /**
     * ADR-015 §3.2 rules. Mutates `v`; returns true when the caller must
     * re-send session_subscribe from the view's cursor (gap or new epoch).
     */
    public static boolean applyTranscript(View v, JSONObject msg) {
        if ("transcript_snapshot".equals(msg.optString("type"))) {
            v.items.clear();
            v.index.clear();
            v.turns.clear();
            JSONArray items = msg.optJSONArray("items");
            if (items != null) for (int i = 0; i < items.length(); i++) {
                JSONObject it = toViewItem(items.opt(i));
                if (it != null) put(v, it);
            }
            JSONArray turns = msg.optJSONArray("turns");
            if (turns != null) for (int i = 0; i < turns.length(); i++) {
                JSONObject t = turns.optJSONObject(i);
                if (t != null && t.opt("id") instanceof String) v.turns.put(t.optString("id"), t);
            }
            v.epoch = msg.optString("epoch");
            v.seq = msg.optLong("seq");
            JSONObject st = msg.optJSONObject("state");
            v.state = st != null ? st : new JSONObject();
            v.truncatedBefore = msg.optBoolean("truncatedBefore");
            v.error = null;
            v.version++;
            return false;
        }
        if (!msg.optString("epoch").equals(v.epoch)) return true;
        JSONArray ops = msg.optJSONArray("ops");
        if (ops == null) return false;
        long seq = v.seq;
        int first = -1;
        for (int i = 0; i < ops.length(); i++) {
            JSONObject op = ops.optJSONObject(i);
            if (op != null && op.optLong("seq") > seq) { first = i; break; }
        }
        if (first < 0) return false;
        if (ops.optJSONObject(first).optLong("seq") != seq + 1) return true;
        boolean resubscribe = false;
        for (int i = first; i < ops.length(); i++) {
            JSONObject op = ops.optJSONObject(i);
            if (op == null) continue;
            long s = op.optLong("seq");
            if (s <= seq) continue;
            if (s != seq + 1) { resubscribe = true; break; }
            seq = s;
            String kind = op.optString("op");
            if (kind.equals("item")) {
                JSONObject it = toViewItem(op.opt("item"));
                if (it != null) put(v, it);
            } else if (kind.equals("append")) {
                Integer at = v.index.get(op.optString("id"));
                if (at != null && op.opt("text") instanceof String) grow(v.items.get(at), op.optString("field"), op.optString("text"));
            } else if (kind.equals("turn")) {
                JSONObject t = op.optJSONObject("turn");
                if (t != null && t.opt("id") instanceof String) v.turns.put(t.optString("id"), t);
            } else if (kind.equals("state")) {
                JSONObject st = op.optJSONObject("state");
                if (st != null) v.state = st;
            }
            // 'mcp' and op kinds from a newer companion: their seq still counts
        }
        v.seq = seq;
        v.version++;
        return resubscribe;
    }

    /** The running turn's id, if any (the "working" row). */
    public static JSONObject runningTurn(View v) {
        JSONObject latest = null;
        for (JSONObject t : v.turns.values()) {
            if ("running".equals(t.optString("status")) && (latest == null || t.optLong("startedAt") > latest.optLong("startedAt"))) latest = t;
        }
        return latest;
    }
}
