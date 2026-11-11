package com.chernobyl.kitkat;

import android.content.Context;
import android.graphics.Typeface;
import android.os.Handler;
import android.os.Looper;
import android.text.InputFilter;
import android.text.InputType;
import android.view.Gravity;
import android.view.View;
import android.view.ViewGroup;
import android.widget.AbsListView;
import android.widget.BaseAdapter;
import android.widget.Button;
import android.widget.EditText;
import android.widget.HorizontalScrollView;
import android.widget.LinearLayout;
import android.widget.ListView;
import android.widget.TextView;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.HashSet;
import java.util.List;
import java.util.Map;
import java.util.Set;
import org.json.JSONArray;
import org.json.JSONObject;

/**
 * One conversation (ADR-015 §5): the live transcript when the session streams
 * one and the companion speaks transcript.v1, else the legacy output lines;
 * inline approvals, turn footers, the working row and the composer.
 */
final class ConversationView extends LinearLayout implements App.Change {
    final String sessionId;
    /** Built for one pane (portrait): has its own Back button. */
    final boolean hasBack;
    private final App app = App.app;
    private final boolean live;
    private final Model.View view;
    private final ListView list;
    private final Adapter adapter = new Adapter();
    private final List<Object> rows = new ArrayList<Object>();
    private final Set<String> expanded = new HashSet<String>();
    private final LinearLayout header;
    private final LinearLayout working;
    private final Ui.Mark workingMark;
    private final TextView workingText;
    private final EditText input;
    private final Button send, stop;
    private final TextView hint;
    private final Handler tick = new Handler(Looper.getMainLooper());
    private String focusPermission;
    private boolean disposed;

    private static final class Footer {
        final JSONObject turn;
        Footer(JSONObject t) { turn = t; }
    }

    private static final class Standalone {
        final JSONObject permission;
        Standalone(JSONObject p) { permission = p; }
    }

    private static final class Line {
        final JSONObject line;
        Line(JSONObject l) { line = l; }
    }

    ConversationView(Context c, String sessionId, String focusPermission, View.OnClickListener onBack) {
        super(c);
        setOrientation(VERTICAL);
        setBackgroundColor(Ui.WHITE);
        this.sessionId = sessionId;
        this.hasBack = onBack != null;
        this.focusPermission = focusPermission;
        JSONObject s = app.model.session(sessionId);
        live = app.model.has("transcript.v1") && s != null && s.optBoolean("transcript");
        view = live ? app.openView(sessionId) : null;

        header = Ui.row(c);
        int p = Ui.dp(c, 12);
        header.setPadding(p, p, p, p);
        header.setBackgroundColor(Ui.BLACK);
        addView(header, Ui.lp(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT));
        if (onBack != null) {
            Button back = Ui.button(c, "‹ Back", 2, onBack);
            back.setTextColor(Ui.WHITE);
            header.addView(back);
        }

        list = new ListView(c);
        list.setDivider(null);
        list.setTranscriptMode(AbsListView.TRANSCRIPT_MODE_NORMAL);
        list.setItemsCanFocus(true);
        list.setPadding(p, p, p, p);
        list.setClipToPadding(false);
        list.setSelector(android.R.color.transparent);
        working = Ui.row(c);
        working.setPadding(0, Ui.dp(c, 8), 0, Ui.dp(c, 16));
        JSONObject agent = s == null ? null : app.model.agent(s.optString("agentId"));
        workingMark = new Ui.Mark(c, agent == null ? null : agent.optString("kind"), 24);
        workingMark.setState("working");
        working.addView(workingMark);
        working.addView(Ui.gap(c, 10));
        workingText = Ui.text(c, "", 14, Ui.TEXT2, false);
        working.addView(workingText);
        list.addFooterView(working, null, false);
        list.setAdapter(adapter);
        addView(list, new LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, 0, 1));

        LinearLayout composer = Ui.col(c);
        composer.setPadding(p, Ui.dp(c, 8), p, p);
        composer.setBackgroundDrawable(Ui.box(Ui.WHITE, Ui.BORDER, 0, c));
        hint = Ui.text(c, "", 12, Ui.TEXT3, false);
        composer.addView(hint);
        LinearLayout r = Ui.row(c);
        input = new EditText(c);
        input.setHint("Message the agent");
        input.setInputType(InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_FLAG_MULTI_LINE | InputType.TYPE_TEXT_FLAG_CAP_SENTENCES);
        input.setMaxLines(6);
        input.setFilters(new InputFilter[] {new InputFilter.LengthFilter(32000)});
        r.addView(input, Ui.weight(1));
        stop = Ui.button(c, "Stop", 1, new OnClickListener() {
            @Override public void onClick(View v) { app.command(App.msg("interrupt", "sessionId", ConversationView.this.sessionId), "Stop sent", null); }
        });
        r.addView(Ui.gap(c, 8));
        r.addView(stop);
        r.addView(Ui.gap(c, 8));
        send = Ui.button(c, "Send", 0, new OnClickListener() {
            @Override public void onClick(View v) { submit(); }
        });
        r.addView(send);
        composer.addView(r);
        addView(composer, Ui.lp(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT));
        app.watch(this);
        changed();
        tick.post(ticker);
    }

    private final Runnable ticker = new Runnable() {
        @Override
        public void run() {
            if (disposed) return;
            updateWorking();
            tick.postDelayed(this, 1000);
        }
    };

    void dispose() {
        disposed = true;
        app.unwatch(this);
        if (live) app.closeView(sessionId);
    }

    private void submit() {
        final String text = input.getText().toString().trim();
        if (text.isEmpty()) return;
        send.setEnabled(false);
        app.command(App.msg("submit_prompt", "sessionId", sessionId, "text", text), null, new App.Result() {
            @Override
            public void done(JSONObject ack, String error) {
                send.setEnabled(true);
                if (ack != null && ack.optBoolean("ok")) input.setText("");
            }
        });
    }

    private JSONObject agent() {
        JSONObject s = app.model.session(sessionId);
        return s == null ? null : app.model.agent(s.optString("agentId"));
    }

    private boolean running() {
        JSONObject s = app.model.session(sessionId);
        if (live) return "running".equals(view.state.optString("status")) || Model.runningTurn(view) != null;
        return s != null && "running".equals(s.optString("status"));
    }

    private void buildHeader() {
        Context c = getContext();
        while (header.getChildCount() > (header.getChildAt(0) instanceof Button ? 1 : 0)) header.removeViewAt(header.getChildCount() - 1);
        JSONObject s = app.model.session(sessionId);
        JSONObject a = agent();
        if (a != null) {
            header.addView(new Ui.Mark(c, a.optString("kind"), 24));
            header.addView(Ui.gap(c, 10));
        }
        LinearLayout t = Ui.col(c);
        t.addView(Ui.text(c, s == null ? "Session" : s.optString("title", "Session"), 17, Ui.WHITE, true));
        String sub = (a == null ? "Unknown agent" : a.optString("name")) + (s != null && !s.optString("project").isEmpty() ? " · " + s.optString("project") : "");
        if (live && view.state.has("model")) sub += " · " + view.state.optString("model");
        t.addView(Ui.text(c, sub, 13, 0xFFBBBBBB, false));
        header.addView(t, Ui.weight(1));
        if (a != null && a.optBoolean("simulated")) {
            header.addView(Ui.simulated(c));
            header.addView(Ui.gap(c, 6));
        }
        String st = live && !view.epoch.isEmpty() ? view.state.optString("status") : s == null ? "" : s.optString("status");
        header.addView(Ui.statusPill(c, st));
    }

    @Override
    public void changed() {
        if (disposed) return;
        buildHeader();
        rows.clear();
        long now = System.currentTimeMillis();
        List<JSONObject> pending = app.model.pendingFor(sessionId, now);
        JSONObject s = app.model.session(sessionId);
        if (live) {
            Map<String, Integer> lastOfTurn = new HashMap<String, Integer>();
            for (int i = 0; i < view.items.size(); i++) {
                String turn = view.items.get(i).optString("turnId");
                if (!turn.isEmpty()) lastOfTurn.put(turn, i);
            }
            for (int i = 0; i < view.items.size(); i++) {
                JSONObject it = view.items.get(i);
                if (!"unknown".equals(it.optString("kind"))) rows.add(it);
                String turn = it.optString("turnId");
                JSONObject ts = view.turns.get(turn);
                if (ts != null && lastOfTurn.get(turn) == i && !"running".equals(ts.optString("status"))) rows.add(new Footer(ts));
            }
            for (JSONObject p : pending) if (!view.index.containsKey(p.optString("toolItemId"))) rows.add(new Standalone(p));
        } else if (s != null) {
            if (!s.optString("prompt").isEmpty()) {
                try { rows.add(new JSONObject().put("id", "prompt").put("kind", "user").put("text", s.optString("prompt"))); } catch (Exception ignored) { }
            }
            JSONArray out = s.optJSONArray("output");
            for (int i = 0; out != null && i < out.length(); i++) if (out.optJSONObject(i) != null) rows.add(new Line(out.optJSONObject(i)));
            for (JSONObject p : pending) rows.add(new Standalone(p));
        }
        adapter.notifyDataSetChanged();
        // a live view focuses once its snapshot is in (before it, the card would only be standalone)
        if (focusPermission != null && !rows.isEmpty() && (!live || !view.epoch.isEmpty())) focus();
        updateWorking();
        JSONObject a = agent();
        JSONObject liveCaps = a == null ? null : a.optJSONObject("live");
        boolean busy = running();
        String mode = liveCaps == null ? "queue" : liveCaps.optString("promptWhileBusy", "none");
        boolean ended = live ? "ended".equals(view.state.optString("status")) : s != null && "completed".equals(s.optString("status"));
        stop.setVisibility(busy && (liveCaps == null || liveCaps.optBoolean("interrupt")) ? VISIBLE : GONE);
        boolean canSend = app.connected() && s != null && !ended && !(busy && mode.equals("none"));
        send.setEnabled(canSend);
        send.setAlpha(canSend ? 1f : 0.35f);
        hint.setText(!app.connected() ? "Offline: messages cannot be sent" : s == null ? "This session is not in the companion's list"
                : ended ? "This session has ended" : busy ? (mode.equals("merge") ? "Joins the current turn" : mode.equals("queue") ? "Queued: runs after this turn" : "Wait for this turn to finish")
                : live && view.error != null ? "Live view refused: " + view.error
                : live && view.epoch.isEmpty() ? "Loading the conversation…" : "");
    }

    private void focus() {
        for (int i = 0; i < rows.size(); i++) {
            Object r = rows.get(i);
            JSONObject p = app.model.permission(focusPermission);
            JSONObject tool = r instanceof JSONObject ? ((JSONObject) r).optJSONObject("tool") : null;
            boolean hit = r instanceof Standalone && ((Standalone) r).permission.optString("id").equals(focusPermission)
                    || tool != null && focusPermission.equals(tool.optString("permissionId"))
                    || r instanceof JSONObject && p != null && ((JSONObject) r).optString("id").equals(p.optString("toolItemId"));
            if (hit) {
                android.util.Log.i(App.TAG, "focus " + focusPermission + " at row " + i);
                final int at = i;
                list.post(new Runnable() { @Override public void run() { list.setSelection(at); } });
                focusPermission = null;
                return;
            }
        }
    }

    private void updateWorking() {
        boolean busy = running();
        working.setVisibility(busy ? VISIBLE : GONE);
        if (!busy) return;
        JSONObject a = agent();
        String name = a == null ? "Agent" : a.optString("name", "Agent");
        boolean waiting = !app.model.pendingFor(sessionId, System.currentTimeMillis()).isEmpty();
        workingMark.setState(waiting ? "waiting" : "working");
        String what = waiting ? name + " is waiting for you" : name + " is working";
        if (live && !waiting) {
            for (int i = view.items.size() - 1; i >= 0; i--) {
                JSONObject t = view.items.get(i).optJSONObject("tool");
                if (t != null && "running".equals(t.optString("status"))) { what = "Running " + t.optString("name") + "…"; break; }
                if (t != null) break;
            }
        }
        JSONObject turn = live ? Model.runningTurn(view) : null;
        if (turn != null) {
            what += " · " + Math.max(0, (System.currentTimeMillis() - turn.optLong("startedAt")) / 1000) + "s";
            JSONObject u = turn.optJSONObject("usage");
            if (u != null) what += " · " + tokens(u.optLong("outputTokens")) + " out";
        }
        workingText.setText(what);
    }

    static String tokens(long n) {
        return n >= 1000 ? String.format(java.util.Locale.US, "%.1fk", n / 1000.0) : String.valueOf(n);
    }

    /* ---------------- rows ---------------- */

    private final class Adapter extends BaseAdapter {
        @Override public int getCount() { return rows.size(); }
        @Override public Object getItem(int i) { return rows.get(i); }
        @Override public long getItemId(int i) { return i; }

        @Override
        public View getView(int i, View convert, ViewGroup parent) {
            Object r = rows.get(i);
            View v;
            if (r instanceof Footer) v = footer(((Footer) r).turn);
            else if (r instanceof Standalone) v = Ui.approval(getContext(), ((Standalone) r).permission, false);
            else if (r instanceof Line) v = line(((Line) r).line);
            else v = item((JSONObject) r, i);
            Ui.addMargin(v, 0, 4, 0, 4);
            LinearLayout wrap = new LinearLayout(getContext());
            wrap.addView(v);
            return wrap;
        }
    }

    private View line(JSONObject l) {
        String stream = l.optString("stream");
        TextView t = Ui.mono(getContext(), l.optString("text"), "stderr".equals(stream) ? Ui.ERROR : Ui.TEXT, 0);
        if ("system".equals(stream)) t.setTypeface(Typeface.create(Typeface.MONOSPACE, Typeface.ITALIC));
        return t;
    }

    /** First agent item after a prompt or of a new turn gets the mark + name. */
    private boolean replyStart(int i) {
        JSONObject it = (JSONObject) rows.get(i);
        if (it.has("parentId") || "user".equals(it.optString("kind")) || "notice".equals(it.optString("kind"))) return false;
        for (int k = i - 1; k >= 0; k--) {
            Object o = rows.get(k);
            if (!(o instanceof JSONObject)) return true;
            JSONObject prev = (JSONObject) o;
            if (prev.has("parentId")) continue;
            return "user".equals(prev.optString("kind")) || !prev.optString("turnId").equals(it.optString("turnId"));
        }
        return true;
    }

    private View item(final JSONObject it, int pos) {
        Context c = getContext();
        LinearLayout box = Ui.col(c);
        if (it.has("parentId")) box.setPadding(Ui.dp(c, 20), 0, 0, 0);
        if (live && replyStart(pos)) {
            JSONObject a = agent();
            LinearLayout head = Ui.row(c);
            head.addView(new Ui.Mark(c, a == null ? null : a.optString("kind"), 16));
            head.addView(Ui.gap(c, 6));
            head.addView(Ui.text(c, a == null ? "Agent" : a.optString("name"), 13, Ui.TEXT2, true));
            head.setPadding(0, Ui.dp(c, 8), 0, Ui.dp(c, 2));
            box.addView(head);
        }
        String kind = it.optString("kind");
        switch (kind) {
            case "user": {
                LinearLayout bubble = Ui.col(c);
                bubble.setBackgroundDrawable(Ui.box(Ui.SURFACE2, 0, 16, c));
                int p = Ui.dp(c, 12);
                bubble.setPadding(p, p, p, p);
                if (it.optInt("images") > 0) bubble.addView(Ui.text(c, it.optInt("images") == 1 ? "1 image" : it.optInt("images") + " images", 12, Ui.TEXT2, true));
                TextView t = Ui.text(c, it.optString("text"), 15, Ui.TEXT, false);
                t.setTextIsSelectable(true);
                bubble.addView(t);
                if (it.optBoolean("queued")) bubble.addView(Ui.text(c, "Queued", 12, Ui.TEXT3, false));
                LinearLayout right = Ui.row(c);
                right.setGravity(Gravity.END);
                right.addView(bubble);
                box.addView(right, Ui.lp(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT));
                break;
            }
            case "text":
                box.addView(Ui.markdown(c, it.optString("text")));
                if (it.optBoolean("truncated")) box.addView(Ui.text(c, "Shortened: the full text is on the PC", 12, Ui.TEXT3, false));
                break;
            case "thinking": {
                final String id = it.optString("id");
                boolean open = expanded.contains(id);
                String txt = it.optString("text");
                TextView t = Ui.text(c, open ? "Thinking\n" + txt : "Thinking · " + Ui.ellipsize(txt, 90), 13, Ui.TEXT2, false);
                t.setTypeface(Typeface.DEFAULT, Typeface.ITALIC);
                t.setOnClickListener(toggle(id));
                box.addView(t);
                break;
            }
            case "notice": {
                String level = it.optString("level");
                box.addView(Ui.text(c, it.optString("text"), 13, "error".equals(level) ? Ui.ERROR : "warning".equals(level) ? Ui.WARNING : Ui.TEXT2, false));
                break;
            }
            case "plan": {
                StringBuilder sb = new StringBuilder("Plan");
                JSONArray steps = it.optJSONArray("steps");
                for (int k = 0; steps != null && k < steps.length(); k++) {
                    JSONObject st = steps.optJSONObject(k);
                    if (st == null) continue;
                    String s = st.optString("status");
                    sb.append("\n").append(s.equals("completed") ? "✓ " : s.equals("in_progress") ? "▸ " : s.equals("cancelled") ? "✕ " : "○ ").append(st.optString("text"));
                }
                LinearLayout card = Ui.card(c, false);
                card.addView(Ui.text(c, sb.toString(), 14, Ui.TEXT, false));
                box.addView(card);
                break;
            }
            case "tool":
                box.addView(tool(it));
                break;
            default:
                box.addView(Ui.text(c, it.optString("text"), 14, Ui.TEXT, false));
        }
        return box;
    }

    private OnClickListener toggle(final String id) {
        return new OnClickListener() {
            @Override
            public void onClick(View v) {
                if (!expanded.remove(id)) expanded.add(id);
                changed();
            }
        };
    }

    static String toolGlyph(String status) {
        switch (status) {
            case "pending": return "○";
            case "running": return "…";
            case "awaiting-approval": return "!";
            case "completed": return "✓";
            case "failed": return "✕";
            case "denied": return "✕";
            case "cancelled": return "–";
            default: return "·";
        }
    }

    private View tool(JSONObject it) {
        Context c = getContext();
        JSONObject t = it.optJSONObject("tool");
        String status = t.optString("status");
        JSONObject permission = null;
        for (JSONObject p : app.model.pendingFor(sessionId, System.currentTimeMillis()))
            if (it.optString("id").equals(p.optString("toolItemId")) || p.optString("id").equals(t.optString("permissionId"))) permission = p;
        boolean open = permission != null || expanded.contains(it.optString("id"));
        LinearLayout card = Ui.card(c, permission != null);
        LinearLayout head = Ui.row(c);
        int glyphColor = status.equals("completed") ? Ui.SUCCESS : status.equals("failed") || status.equals("denied") ? Ui.ERROR
                : status.equals("awaiting-approval") ? Ui.WARNING : Ui.TEXT2;
        head.addView(Ui.text(c, toolGlyph(status), 15, glyphColor, true));
        head.addView(Ui.gap(c, 8));
        head.addView(Ui.text(c, t.optString("name"), 14, Ui.TEXT, true));
        head.addView(Ui.gap(c, 8));
        TextView title = Ui.text(c, Ui.ellipsize(t.optString("title"), 120), 13, Ui.TEXT2, false);
        title.setSingleLine(true);
        head.addView(title, Ui.weight(1));
        if (!status.equals("completed") && !status.equals("awaiting-approval")) {
            head.addView(Ui.text(c, status, 12, glyphColor, false));
            head.addView(Ui.gap(c, 8));
        }
        head.addView(Ui.text(c, open ? "▴" : "▾", 14, Ui.TEXT3, false));
        head.setOnClickListener(toggle(it.optString("id")));
        card.addView(head);
        if (open) {
            if (t.has("input") && !t.optString("input").isEmpty()) {
                card.addView(Ui.eyebrow(c, "Input"));
                card.addView(scroll(Ui.mono(c, Ui.ellipsizeKeep(t.optString("input"), 4000), Ui.WHITE, Ui.BLACK)));
            }
            if (t.has("output") && !t.optString("output").isEmpty()) {
                card.addView(Ui.eyebrow(c, "Output" + (t.optBoolean("outputTruncated") ? " (shortened)" : "")));
                card.addView(scroll(Ui.mono(c, Ui.ellipsizeKeep(t.optString("output"), 8000), Ui.TEXT, Ui.SURFACE2)));
            }
            if (t.has("error")) card.addView(Ui.text(c, t.optString("error"), 13, Ui.ERROR, false));
            JSONArray diffs = t.optJSONArray("diffs");
            for (int k = 0; diffs != null && k < diffs.length(); k++) {
                JSONObject d = diffs.optJSONObject(k);
                if (d == null) continue;
                View dv = Ui.diff(c, d.optString("path") + "  +" + d.optInt("additions") + " −" + d.optInt("deletions"), d.optString("patch"));
                Ui.addMargin(dv, 0, 8, 0, 0);
                card.addView(dv);
            }
            if (t.has("autoApproved")) card.addView(Ui.text(c, "Auto-approved (" + t.optString("autoApproved") + ")", 12, Ui.TEXT3, false));
            if (permission != null) {
                View a = Ui.approval(c, permission, true);
                Ui.addMargin(a, 0, 10, 0, 0);
                card.addView(a);
            } else if (status.equals("awaiting-approval")) {
                card.addView(Ui.text(c, "Waiting for approval…", 13, Ui.WARNING, false));
            }
        }
        return card;
    }

    private View scroll(View v) {
        HorizontalScrollView hs = new HorizontalScrollView(getContext());
        hs.addView(v);
        return hs;
    }

    private View footer(JSONObject t) {
        Context c = getContext();
        String st = t.optString("status");
        StringBuilder s = new StringBuilder(st.equals("completed") ? "Done" : st.equals("interrupted") ? "Stopped" : st.equals("failed") ? "Failed" : Ui.capital(st));
        if (t.has("endedAt") && t.has("startedAt")) s.append(" · ").append(String.format(java.util.Locale.US, "%.1fs", (t.optLong("endedAt") - t.optLong("startedAt")) / 1000.0));
        JSONObject u = t.optJSONObject("usage");
        if (u != null) s.append(" · ").append(tokens(u.optLong("inputTokens"))).append(" in / ").append(tokens(u.optLong("outputTokens"))).append(" out");
        if (t.has("costUsd")) s.append(String.format(java.util.Locale.US, " · $%.4f", t.optDouble("costUsd")));
        if (t.has("error")) s.append("\n").append(t.optString("error"));
        LinearLayout box = Ui.col(c);
        box.addView(Ui.text(c, s.toString(), 12, st.equals("failed") ? Ui.ERROR : Ui.TEXT3, false));
        JSONArray diffs = t.optJSONArray("diffs");
        for (int k = 0; diffs != null && k < diffs.length(); k++) {
            JSONObject d = diffs.optJSONObject(k);
            if (d != null) box.addView(Ui.text(c, "  " + d.optString("path") + "  +" + d.optInt("additions") + " −" + d.optInt("deletions"), 12, Ui.TEXT2, false));
        }
        box.setPadding(0, Ui.dp(c, 2), 0, Ui.dp(c, 10));
        return box;
    }
}
