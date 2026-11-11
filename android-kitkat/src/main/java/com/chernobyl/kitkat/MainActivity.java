package com.chernobyl.kitkat;

import android.app.Activity;
import android.app.AlertDialog;
import android.content.Context;
import android.content.DialogInterface;
import android.content.Intent;
import android.content.pm.ApplicationInfo;
import android.content.res.Configuration;
import android.os.Bundle;
import android.text.InputType;
import android.util.Log;
import android.view.Gravity;
import android.view.View;
import android.view.ViewGroup;
import android.widget.AdapterView;
import android.widget.ArrayAdapter;
import android.widget.BaseAdapter;
import android.widget.Button;
import android.widget.CheckBox;
import android.widget.CompoundButton;
import android.widget.EditText;
import android.widget.FrameLayout;
import android.widget.LinearLayout;
import android.widget.ListView;
import android.widget.ScrollView;
import android.widget.Spinner;
import android.widget.TextView;
import java.net.Socket;
import java.util.ArrayList;
import java.util.List;
import javax.net.ssl.SSLSocket;
import org.json.JSONArray;
import org.json.JSONObject;

/**
 * The one screen: a black navigation rail and the content beside it. Sessions
 * are two-pane (list + conversation) when the tablet is 720 dp or wider
 * (landscape), one pane with Back otherwise.
 */
public final class MainActivity extends Activity implements App.Change {
    static final int HOME = 0, SESSIONS = 1, APPROVALS = 2, FILES = 3, SETTINGS = 4, PAIR = 5;
    private static final int SCAN = 7, PHOTO = 8;

    private final App app = App.app;
    private int screen = HOME;
    private LinearLayout rail;
    private FrameLayout content;
    private ConversationView conversation;
    private String openSession;
    private ScrollView scroll;
    private SessionAdapter sessionAdapter;
    private EditText payloadField;
    private TextView pairStatus;

    @Override
    protected void onCreate(Bundle b) {
        super.onCreate(b);
        LinearLayout root = Ui.row(this);
        root.setGravity(Gravity.NO_GRAVITY);
        root.setBackgroundColor(Ui.WHITE);
        rail = Ui.col(this);
        rail.setBackgroundColor(Ui.BLACK);
        int p = Ui.dp(this, 8);
        rail.setPadding(p, Ui.dp(this, 16), p, p);
        root.addView(rail, Ui.lp(Ui.dp(this, 112), ViewGroup.LayoutParams.MATCH_PARENT));
        content = new FrameLayout(this);
        root.addView(content, new LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.MATCH_PARENT, 1));
        setContentView(root);
        screen = app.pairing == null ? PAIR : HOME;
        handle(getIntent());
        render();
    }

    @Override
    protected void onNewIntent(Intent i) {
        super.onNewIntent(i);
        setIntent(i);
        handle(i);
        render();
    }

    private boolean debuggable() {
        return (getApplicationInfo().flags & ApplicationInfo.FLAG_DEBUGGABLE) != 0;
    }

    private String prefillPayload, prefillToken;

    private void handle(Intent i) {
        if (i == null) return;
        String sid = i.getStringExtra("sessionId");
        if (sid != null && !sid.isEmpty()) {
            screen = SESSIONS;
            showConversation(sid, i.getStringExtra("permissionId"));
            i.removeExtra("sessionId");
        }
        final String probe = i.getStringExtra("tlsprobe");
        if (probe != null) {
            i.removeExtra("tlsprobe");
            new Thread(new Runnable() {
                @Override
                public void run() {
                    try {
                        Socket s = App.OPENER.open(probe, 443, true, 15000);
                        SSLSocket ssl = (SSLSocket) s;
                        Log.i(App.TAG, "tlsprobe OK " + probe + " " + ssl.getSession().getProtocol() + " " + ssl.getSession().getCipherSuite());
                        s.close();
                    } catch (Exception e) {
                        Log.i(App.TAG, "tlsprobe FAIL " + probe + " " + e);
                    }
                }
            }).start();
        }
        // adb test hook, debug builds only: prefill (never submit) the pairing form
        if (debuggable() && i.getStringExtra("pairPayload") != null) {
            prefillPayload = i.getStringExtra("pairPayload");
            prefillToken = i.getStringExtra("token");
            i.removeExtra("pairPayload");
            screen = PAIR;
        }
    }

    @Override
    protected void onResume() {
        super.onResume();
        app.visible++;
        app.watch(this);
        if (app.transport != null) app.transport.nudge();
        changed();
    }

    @Override
    protected void onPause() {
        super.onPause();
        app.visible--;
        app.unwatch(this);
    }

    @Override
    protected void onDestroy() {
        if (conversation != null) conversation.dispose();
        super.onDestroy();
    }

    @Override
    public void onConfigurationChanged(Configuration c) {
        super.onConfigurationChanged(c);
        render();
    }

    @Override
    public void onBackPressed() {
        if (screen == SESSIONS && openSession != null && !wide()) {
            closeConversation();
            render();
        } else if (screen != HOME) {
            screen = HOME;
            render();
        } else {
            super.onBackPressed();
        }
    }

    private boolean wide() {
        return getResources().getConfiguration().screenWidthDp >= 720;
    }

    /* ---------------- shell ---------------- */

    private void go(int s) {
        ((android.view.inputmethod.InputMethodManager) getSystemService(INPUT_METHOD_SERVICE))
                .hideSoftInputFromWindow(content.getWindowToken(), 0);
        screen = s;
        render();
    }

    private void buildRail() {
        rail.removeAllViews();
        TextView brand = Ui.text(this, "CHERNOBYL", 13, Ui.WHITE, true);
        brand.setGravity(Gravity.CENTER);
        rail.addView(brand, Ui.lp(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT));
        String st = app.status;
        TextView dot = Ui.text(this, "● " + (Transport.CONNECTED.equals(st) ? "Online" : app.pairing == null ? "Unpaired"
                : Transport.DENIED.equals(st) ? "Refused" : "Offline"), 11,
                Transport.CONNECTED.equals(st) ? 0xFF7FD19B : Transport.DENIED.equals(st) ? 0xFFE08080 : 0xFFAAAAAA, false);
        dot.setGravity(Gravity.CENTER);
        dot.setPadding(0, Ui.dp(this, 4), 0, Ui.dp(this, 16));
        rail.addView(dot, Ui.lp(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT));
        int pending = app.model.pending(System.currentTimeMillis()).size();
        String[] labels = {"Home", "Sessions", "Approvals" + (pending > 0 ? " (" + pending + ")" : ""), "Files", "Settings", "Pair"};
        for (int i = 0; i < labels.length; i++) {
            final int target = i;
            Button b = new Button(this);
            b.setText(labels[i]);
            b.setAllCaps(false);
            b.setTextSize(13);
            boolean on = screen == i;
            b.setTextColor(on ? Ui.BLACK : Ui.WHITE);
            b.setBackgroundDrawable(Ui.box(on ? Ui.WHITE : Ui.BLACK, 0, 14, this));
            b.setOnClickListener(new View.OnClickListener() {
                @Override public void onClick(View v) { go(target); }
            });
            LinearLayout.LayoutParams lp = Ui.lp(ViewGroup.LayoutParams.MATCH_PARENT, Ui.dp(this, 48));
            lp.bottomMargin = Ui.dp(this, 6);
            rail.addView(b, lp);
        }
    }

    private void render() {
        buildRail();
        content.removeAllViews();
        scroll = null;
        sessionAdapter = null;
        pairStatus = null;
        if (screen != SESSIONS && conversation != null) {
            // unsubscribed while another tab shows; reopened when Sessions is back
            String keep = openSession;
            closeConversation();
            openSession = keep;
        }
        if (screen == SESSIONS && conversation == null && openSession != null) showConversation(openSession, null);
        switch (screen) {
            case SESSIONS: renderSessions(); break;
            case APPROVALS: renderScroll(); break;
            case FILES: content.addView(new WorkspaceView(this)); break;
            case SETTINGS: renderSettings(); break;
            case PAIR: renderPair(); break;
            default: renderScroll();
        }
    }

    /** Called on every model change (coalesced by App). */
    @Override
    public void changed() {
        buildRail();
        if (screen == HOME || screen == APPROVALS) fillScroll();
        if (sessionAdapter != null) sessionAdapter.notifyDataSetChanged();
        if (pairStatus != null) pairStatus.setText(pairInfo());
    }

    private void renderScroll() {
        scroll = new ScrollView(this);
        content.addView(scroll);
        fillScroll();
    }

    private void fillScroll() {
        if (scroll == null) return;
        LinearLayout page = Ui.col(this);
        int p = Ui.dp(this, 24);
        page.setPadding(p, p, p, p);
        if (screen == HOME) home(page);
        else approvals(page);
        scroll.removeAllViews();
        scroll.addView(page);
    }

    /* ---------------- home ---------------- */

    private void home(LinearLayout page) {
        page.addView(Ui.text(this, "Home", 28, Ui.TEXT, true));
        page.addView(Ui.text(this, app.statusLabel(), 14, Ui.TEXT2, false));
        if (app.pairing == null) {
            LinearLayout card = Ui.card(this, true);
            card.addView(Ui.text(this, "Pair this tablet with your PC", 17, Ui.TEXT, true));
            card.addView(Ui.text(this, "Run chernobyl-companion --pair on the PC, then paste the payload line or scan its QR.", 14, Ui.TEXT2, false));
            card.addView(Ui.button(this, "Pair", 0, new View.OnClickListener() {
                @Override public void onClick(View v) { go(PAIR); }
            }));
            Ui.addMargin(card, 0, 16, 0, 0);
            page.addView(card);
            return;
        }
        if (!app.model.synced) {
            page.addView(Ui.text(this, Transport.DENIED.equals(app.status)
                    ? "The companion refused this tablet. Pair again from the Pair tab." : "Waiting for the companion…", 15, Ui.TEXT3, false));
            return;
        }
        if (app.model.hasSimulated()) {
            TextView demo = Ui.text(this, "Demo data: this companion runs its mock adapter. Items marked Simulated are not live agents.", 13, Ui.TEXT2, false);
            demo.setBackgroundDrawable(Ui.box(Ui.SURFACE2, 0, 12, this));
            int q = Ui.dp(this, 12);
            demo.setPadding(q, q, q, q);
            Ui.addMargin(demo, 0, 12, 0, 0);
            page.addView(demo);
        }
        long now = System.currentTimeMillis();
        int hostsOnline = app.model.count(app.model.hosts, "status", "connected");
        int agentsUp = 0;
        for (JSONObject a : app.model.agents) if (!"offline".equals(a.optString("state"))) agentsUp++;
        int running = app.model.count(app.model.sessions, "status", "running");
        int pending = app.model.pending(now).size();
        LinearLayout stats = Ui.row(this);
        stats.setPadding(0, Ui.dp(this, 16), 0, 0);
        stat(stats, "Hosts online", hostsOnline + " / " + app.model.hosts.size(), null);
        stat(stats, "Agents", agentsUp + " / " + app.model.agents.size(), null);
        stat(stats, "Running sessions", String.valueOf(running), new View.OnClickListener() {
            @Override public void onClick(View v) { go(SESSIONS); }
        });
        stat(stats, "Pending approvals", String.valueOf(pending), new View.OnClickListener() {
            @Override public void onClick(View v) { go(APPROVALS); }
        });
        page.addView(stats);
        page.addView(Ui.eyebrow(this, "Agents"));
        if (app.model.agents.isEmpty()) page.addView(Ui.text(this, "No agents on this companion", 14, Ui.TEXT3, false));
        for (JSONObject a : app.model.agents) {
            LinearLayout r = Ui.row(this);
            r.setPadding(0, Ui.dp(this, 6), 0, Ui.dp(this, 6));
            Ui.Mark m = new Ui.Mark(this, a.optString("kind"), 24);
            m.setState("busy".equals(a.optString("state")) ? "working" : "offline".equals(a.optString("state")) ? "error" : "idle");
            r.addView(m);
            r.addView(Ui.gap(this, 10));
            r.addView(Ui.text(this, a.optString("name"), 15, Ui.TEXT, true));
            r.addView(Ui.gap(this, 8));
            r.addView(Ui.text(this, a.optString("state"), 13, Ui.TEXT2, false), Ui.weight(1));
            if (a.optBoolean("simulated")) r.addView(Ui.simulated(this));
            page.addView(r);
        }
        if (!app.model.activity.isEmpty()) {
            page.addView(Ui.eyebrow(this, "Recent activity"));
            for (int i = 0; i < Math.min(6, app.model.activity.size()); i++) {
                JSONObject e = app.model.activity.get(i);
                page.addView(Ui.text(this, Ui.ago(e.optLong("ts")) + "  ·  " + e.optString("text"), 13, Ui.TEXT2, false));
            }
        }
    }

    private void stat(LinearLayout row, String label, String value, View.OnClickListener l) {
        LinearLayout c = Ui.card(this, false);
        c.addView(Ui.text(this, value, 26, Ui.TEXT, true));
        c.addView(Ui.text(this, label, 13, Ui.TEXT2, false));
        if (l != null) c.setOnClickListener(l);
        LinearLayout.LayoutParams lp = Ui.weight(1);
        lp.rightMargin = Ui.dp(this, 10);
        row.addView(c, lp);
    }

    /* ---------------- approvals ---------------- */

    private void approvals(LinearLayout page) {
        page.addView(Ui.text(this, "Approvals", 28, Ui.TEXT, true));
        List<JSONObject> pending = app.model.pending(System.currentTimeMillis());
        page.addView(Ui.text(this, pending.isEmpty() ? (app.model.synced ? "Nothing waiting for you" : "Not connected") : pending.size() + " waiting", 14, Ui.TEXT2, false));
        for (final JSONObject p : pending) {
            View card = Ui.approval(this, p, false);
            Ui.addMargin(card, 0, 16, 0, 4);
            page.addView(card);
            JSONObject s = app.model.session(p.optString("sessionId"));
            if (s != null) {
                page.addView(Ui.button(this, "Open " + Ui.ellipsize(s.optString("title"), 40), 2, new View.OnClickListener() {
                    @Override
                    public void onClick(View v) {
                        screen = SESSIONS;
                        showConversation(p.optString("sessionId"), p.optString("id"));
                        render();
                    }
                }));
            }
        }
        List<JSONObject> decided = new ArrayList<JSONObject>();
        for (JSONObject p : app.model.permissions) if ("approved".equals(p.optString("status")) || "rejected".equals(p.optString("status"))) decided.add(p);
        if (!decided.isEmpty()) {
            page.addView(Ui.eyebrow(this, "Decided"));
            for (int i = 0; i < Math.min(8, decided.size()); i++) {
                JSONObject p = decided.get(i);
                page.addView(Ui.text(this, ("approved".equals(p.optString("status")) ? "✓ " : "✕ ") + p.optString("title") + "  ·  "
                        + Ui.ellipsize(p.optString("command"), 60), 13, Ui.TEXT2, false));
            }
        }
    }

    /* ---------------- sessions ---------------- */

    private void showConversation(String sid, String focusPermission) {
        if (conversation != null) conversation.dispose();
        openSession = sid;
        conversation = new ConversationView(this, sid, focusPermission, wide() ? null : new View.OnClickListener() {
            @Override public void onClick(View v) { closeConversation(); render(); }
        });
    }

    /** A fork's new session (ADR-018), or any session to open from a child view. */
    void openSession(String sid) {
        screen = SESSIONS;
        showConversation(sid, null);
        render();
    }

    /** The session header's Files / Changes chips: that workspace on the Files screen. */
    void openWorkspace(String wsId, int tab) {
        app.filesWs = wsId;
        app.filesTab = tab;
        go(FILES);
    }

    private String photoFor;

    /** ADR-017: the camera screen in photo mode; the JPEG comes back base64 within `budget`. */
    void takePhoto(String sessionId, int budget) {
        photoFor = sessionId;
        startActivityForResult(new Intent(this, ScanActivity.class).putExtra("photoBudget", budget), PHOTO);
    }

    private void closeConversation() {
        if (conversation != null) conversation.dispose();
        conversation = null;
        openSession = null;
    }

    private void renderSessions() {
        boolean wide = wide();
        // rotated across the 720 dp line: rebuild for the other pane mode (Back button or not)
        if (conversation != null && conversation.hasBack == wide) showConversation(openSession, null);
        if (conversation != null && conversation.getParent() != null) ((ViewGroup) conversation.getParent()).removeView(conversation);
        if (!wide && conversation != null) {
            content.addView(conversation, new FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT));
            return;
        }
        LinearLayout split = Ui.row(this);
        split.setGravity(Gravity.NO_GRAVITY);
        LinearLayout left = Ui.col(this);
        int p = Ui.dp(this, 16);
        left.setPadding(p, p, p, 0);
        LinearLayout head = Ui.row(this);
        head.addView(Ui.text(this, "Sessions", 24, Ui.TEXT, true), Ui.weight(1));
        head.addView(Ui.button(this, "New", 0, new View.OnClickListener() {
            @Override public void onClick(View v) { newSession(); }
        }));
        left.addView(head);
        ListView lv = new ListView(this);
        lv.setDivider(null);
        sessionAdapter = new SessionAdapter();
        lv.setAdapter(sessionAdapter);
        lv.setOnItemClickListener(new AdapterView.OnItemClickListener() {
            @Override
            public void onItemClick(AdapterView<?> parent, View v, int pos, long id) {
                showConversation(sessionAdapter.list.get(pos).optString("id"), null);
                render();
            }
        });
        left.addView(lv, new LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, 0, 1));
        if (!wide) {
            content.addView(left);
            return;
        }
        split.addView(left, Ui.lp(Ui.dp(this, 340), ViewGroup.LayoutParams.MATCH_PARENT));
        View line = new View(this);
        line.setBackgroundColor(Ui.BORDER);
        split.addView(line, Ui.lp(Math.max(1, Ui.dp(this, 1)), ViewGroup.LayoutParams.MATCH_PARENT));
        if (conversation != null) {
            split.addView(conversation, new LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.MATCH_PARENT, 1));
        } else {
            TextView empty = Ui.text(this, app.model.synced ? "Pick a session, or start a new one." : "Waiting for the companion…", 15, Ui.TEXT3, false);
            empty.setGravity(Gravity.CENTER);
            split.addView(empty, new LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.MATCH_PARENT, 1));
        }
        content.addView(split);
    }

    private final class SessionAdapter extends BaseAdapter {
        List<JSONObject> list = new ArrayList<JSONObject>();

        @Override
        public void notifyDataSetChanged() {
            list = app.model.sessionsByActivity();
            super.notifyDataSetChanged();
        }

        SessionAdapter() {
            list = app.model.sessionsByActivity();
        }

        @Override public int getCount() { return list.size(); }
        @Override public Object getItem(int i) { return list.get(i); }
        @Override public long getItemId(int i) { return i; }

        @Override
        public View getView(int i, View convert, ViewGroup parent) {
            Context c = MainActivity.this;
            JSONObject s = list.get(i);
            JSONObject a = app.model.agent(s.optString("agentId"));
            boolean on = s.optString("id").equals(openSession);
            LinearLayout r = Ui.row(c);
            int p = Ui.dp(c, 10);
            r.setPadding(p, p, p, p);
            r.setBackgroundDrawable(Ui.box(on ? Ui.SURFACE2 : Ui.WHITE, 0, 12, c));
            Ui.Mark m = new Ui.Mark(c, a == null ? null : a.optString("kind"), 24);
            if ("running".equals(s.optString("status"))) m.setState("working");
            r.addView(m);
            r.addView(Ui.gap(c, 10));
            LinearLayout t = Ui.col(c);
            TextView title = Ui.text(c, s.optString("title", "Session"), 15, Ui.TEXT, true);
            title.setSingleLine(true);
            title.setEllipsize(android.text.TextUtils.TruncateAt.END);
            t.addView(title);
            String sub = (a == null ? "Unknown agent" : a.optString("name")) + (s.optString("project").isEmpty() ? "" : " · " + s.optString("project"))
                    + " · " + Ui.ago(s.optLong("lastActivityAt", s.optLong("createdAt")));
            TextView st = Ui.text(c, sub, 12, Ui.TEXT2, false);
            st.setSingleLine(true);
            st.setEllipsize(android.text.TextUtils.TruncateAt.END);
            t.addView(st);
            if (!s.optString("forkedFrom").isEmpty()) {
                TextView f = Ui.text(c, "Forked from “" + s.optString("forkedFrom") + "”", 12, Ui.TEXT2, false);
                f.setSingleLine(true);
                f.setEllipsize(android.text.TextUtils.TruncateAt.END);
                t.addView(f);
            }
            LinearLayout pills = Ui.row(c);
            pills.setPadding(0, Ui.dp(c, 4), 0, 0);
            // ADR-018: the tag replaces the status for a session the PC is writing right now
            pills.addView(s.optBoolean("livePc") ? Ui.livePc(c, false) : Ui.statusPill(c, s.optString("status")));
            if (s.optBoolean("transcript")) {
                pills.addView(Ui.gap(c, 4));
                pills.addView(Ui.pill(c, "Live", Ui.TEXT, Ui.WHITE));
            }
            if (a != null && a.optBoolean("simulated")) {
                pills.addView(Ui.gap(c, 4));
                pills.addView(Ui.simulated(c));
            }
            t.addView(pills);
            r.addView(t, Ui.weight(1));
            return r;
        }
    }

    private void newSession() {
        final List<JSONObject> agents = new ArrayList<JSONObject>();
        for (JSONObject a : app.model.agents) if (a.optBoolean("acceptsNewSession") && !"offline".equals(a.optString("state"))) agents.add(a);
        if (agents.isEmpty()) {
            app.toast(app.model.synced ? "No agent is accepting new sessions" : "Not connected");
            return;
        }
        final LinearLayout form = Ui.col(this);
        int p = Ui.dp(this, 16);
        form.setPadding(p, p, p, p);
        form.addView(Ui.text(this, "Agent", 13, Ui.TEXT2, true));
        final Spinner agentPick = new Spinner(this);
        String[] names = new String[agents.size()];
        for (int i = 0; i < names.length; i++) names[i] = agents.get(i).optString("name") + (agents.get(i).optBoolean("simulated") ? " (simulated)" : "");
        agentPick.setAdapter(spinnerAdapter(names));
        form.addView(agentPick);
        final TextView wsLabel = Ui.text(this, "Workspace", 13, Ui.TEXT2, true);
        final Spinner wsPick = new Spinner(this);
        final TextView modelLabel = Ui.text(this, "Model", 13, Ui.TEXT2, true);
        final Spinner modelPick = new Spinner(this);
        final String[] models = {"default", "sonnet", "opus", "haiku"};
        modelPick.setAdapter(spinnerAdapter(new String[] {"Default (the PC's setting)", "Sonnet", "Opus", "Haiku"}));
        final EditText project = new EditText(this);
        project.setHint("Project (optional)");
        form.addView(wsLabel);
        form.addView(wsPick);
        form.addView(project);
        form.addView(modelLabel);
        form.addView(modelPick);
        final EditText title = new EditText(this);
        title.setHint("Task title (3-80 characters)");
        title.setSingleLine(true);
        form.addView(title);
        final EditText prompt = new EditText(this);
        prompt.setHint("First prompt");
        prompt.setInputType(InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_FLAG_MULTI_LINE | InputType.TYPE_TEXT_FLAG_CAP_SENTENCES);
        prompt.setMinLines(3);
        form.addView(prompt);
        final List<JSONObject> workspaces = new ArrayList<JSONObject>();
        agentPick.setOnItemSelectedListener(new AdapterView.OnItemSelectedListener() {
            @Override
            public void onItemSelected(AdapterView<?> parent, View v, int pos, long id) {
                JSONObject live = agents.get(pos).optJSONObject("live");
                workspaces.clear();
                JSONArray ws = live == null ? null : live.optJSONArray("workspaces");
                for (int i = 0; ws != null && i < ws.length(); i++) if (ws.optJSONObject(i) != null) workspaces.add(ws.optJSONObject(i));
                String[] labels = new String[workspaces.size()];
                for (int i = 0; i < labels.length; i++) labels[i] = workspaces.get(i).optString("label") + "  —  " + workspaces.get(i).optString("path");
                wsPick.setAdapter(spinnerAdapter(labels));
                int wsVis = workspaces.isEmpty() ? View.GONE : View.VISIBLE;
                wsLabel.setVisibility(wsVis);
                wsPick.setVisibility(wsVis);
                project.setVisibility(workspaces.isEmpty() ? View.VISIBLE : View.GONE);
                JSONArray composer = live == null ? null : live.optJSONArray("composer");
                boolean pickModel = composer != null && composer.toString().contains("\"model\"");
                modelLabel.setVisibility(pickModel ? View.VISIBLE : View.GONE);
                modelPick.setVisibility(pickModel ? View.VISIBLE : View.GONE);
            }

            @Override public void onNothingSelected(AdapterView<?> parent) { }
        });
        ScrollView sv = new ScrollView(this);
        sv.addView(form);
        final AlertDialog d = new AlertDialog.Builder(this).setTitle("New session").setView(sv)
                .setPositiveButton("Start", null).setNegativeButton("Cancel", null).create();
        d.setOnShowListener(new DialogInterface.OnShowListener() {
            @Override
            public void onShow(DialogInterface di) {
                d.getButton(AlertDialog.BUTTON_POSITIVE).setOnClickListener(new View.OnClickListener() {
                    @Override
                    public void onClick(View v) {
                        String t = title.getText().toString().trim(), pr = prompt.getText().toString().trim();
                        if (t.length() < 3 || t.length() > 80) { title.setError("3-80 characters"); return; }
                        if (pr.isEmpty()) { prompt.setError("Write the first prompt"); return; }
                        JSONObject agent = agents.get(agentPick.getSelectedItemPosition());
                        String proj = workspaces.isEmpty() ? project.getText().toString().trim()
                                : workspaces.get(Math.max(0, wsPick.getSelectedItemPosition())).optString("id");
                        String model = modelPick.getVisibility() == View.VISIBLE ? models[modelPick.getSelectedItemPosition()] : "default";
                        JSONObject m = App.msg("create_session", "agentId", agent.optString("id"), "title", t, "prompt", pr,
                                "project", proj.isEmpty() ? null : proj, "model", "default".equals(model) ? null : model);
                        d.dismiss();
                        app.command(m, null, new App.Result() {
                            @Override
                            public void done(JSONObject ack, String error) {
                                if (ack != null && ack.optBoolean("ok") && ack.has("sessionId")) {
                                    screen = SESSIONS;
                                    showConversation(ack.optString("sessionId"), null);
                                    render();
                                }
                            }
                        });
                    }
                });
            }
        });
        d.show();
    }

    private ArrayAdapter<String> spinnerAdapter(String[] items) {
        ArrayAdapter<String> a = new ArrayAdapter<String>(this, android.R.layout.simple_spinner_item, items);
        a.setDropDownViewResource(android.R.layout.simple_spinner_dropdown_item);
        return a;
    }

    /* ---------------- pair ---------------- */

    private String pairInfo() {
        StringBuilder s = new StringBuilder("Status: ").append(app.statusLabel());
        if (app.pairing != null) {
            s.append("\nDevice: ").append(app.pairing.deviceId).append("\nEndpoints: ");
            for (String e : app.pairing.endpoints) s.append("\n  ").append(e);
        }
        s.append("\nKeys stored in: ").append(app.store.mode());
        return s.toString();
    }

    private void renderPair() {
        ScrollView sv = new ScrollView(this);
        LinearLayout page = Ui.col(this);
        int p = Ui.dp(this, 24);
        page.setPadding(p, p, p, p);
        page.addView(Ui.text(this, "Pair with your PC", 28, Ui.TEXT, true));
        page.addView(Ui.text(this, "On the PC run  chernobyl-companion --pair  and paste the line that starts with payload: (or scan its QR). "
                + "Enter the access token if the companion uses one (--token).", 14, Ui.TEXT2, false));
        pairStatus = Ui.text(this, pairInfo(), 13, Ui.TEXT, false);
        pairStatus.setTypeface(android.graphics.Typeface.MONOSPACE);
        pairStatus.setPadding(0, Ui.dp(this, 12), 0, Ui.dp(this, 12));
        page.addView(pairStatus);
        payloadField = new EditText(this);
        payloadField.setHint(app.pairing == null ? "payload: {\"v\":1, ...}" : "Leave empty to keep this pairing and only change the token");
        payloadField.setMinLines(3);
        payloadField.setTypeface(android.graphics.Typeface.MONOSPACE);
        payloadField.setTextSize(13);
        if (prefillPayload != null) payloadField.setText(prefillPayload);
        page.addView(payloadField);
        final EditText token = new EditText(this);
        token.setHint("Access token (optional)");
        token.setSingleLine(true);
        token.setInputType(InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_VARIATION_PASSWORD);
        if (prefillToken != null) token.setText(prefillToken);
        prefillPayload = null;
        prefillToken = null;
        page.addView(token);
        LinearLayout actions = Ui.row(this);
        actions.setPadding(0, Ui.dp(this, 12), 0, 0);
        actions.addView(Ui.button(this, "Pair and connect", 0, new View.OnClickListener() {
            @Override
            public void onClick(View v) {
                try {
                    if (payloadField.getText().toString().trim().isEmpty() && app.pairing != null) {
                        app.setToken(token.getText().toString()); // keep the pairing, change the token
                        go(HOME);
                        return;
                    }
                    app.pair(payloadField.getText().toString(), token.getText().toString());
                    app.toast("Paired. Connecting…");
                    go(HOME);
                } catch (E2e.E2eException e) {
                    payloadField.setError(e.getMessage());
                }
            }
        }));
        actions.addView(Ui.gap(this, 8));
        actions.addView(Ui.button(this, "Scan QR", 1, new View.OnClickListener() {
            @Override public void onClick(View v) { startActivityForResult(new Intent(MainActivity.this, ScanActivity.class), SCAN); }
        }));
        if (app.pairing != null) {
            actions.addView(Ui.gap(this, 8));
            actions.addView(Ui.button(this, "Reconnect", 1, new View.OnClickListener() {
                @Override public void onClick(View v) { app.start(); }
            }));
            actions.addView(Ui.gap(this, 8));
            actions.addView(Ui.button(this, "Unpair", 2, new View.OnClickListener() {
                @Override
                public void onClick(View v) {
                    new AlertDialog.Builder(MainActivity.this).setTitle("Unpair this tablet?")
                            .setMessage("The stored key and token are deleted. Revoke the device on the PC too (--revoke).")
                            .setPositiveButton("Unpair", new DialogInterface.OnClickListener() {
                                @Override public void onClick(DialogInterface d, int w) { app.unpair(); render(); }
                            }).setNegativeButton("Cancel", null).show();
                }
            }));
        }
        page.addView(actions);
        sv.addView(page);
        content.addView(sv);
    }

    /* ---------------- settings: alerts, background, audit ---------------- */

    private void toggle(LinearLayout page, String label, String caption, final String key, boolean value) {
        CheckBox b = new CheckBox(this);
        android.text.SpannableStringBuilder t = new android.text.SpannableStringBuilder(label + "\n" + caption);
        t.setSpan(new android.text.style.ForegroundColorSpan(Ui.TEXT2), label.length() + 1, t.length(), 0);
        t.setSpan(new android.text.style.RelativeSizeSpan(0.85f), label.length() + 1, t.length(), 0);
        b.setText(t);
        b.setTextSize(15);
        b.setTextColor(Ui.TEXT);
        b.setChecked(value);
        b.setOnCheckedChangeListener(new CompoundButton.OnCheckedChangeListener() {
            @Override
            public void onCheckedChanged(CompoundButton v, boolean on) {
                app.prefs().edit().putBoolean(key, on).commit();
                if (key.equals("keepAwake") && app.transport != null) ConnService.start(MainActivity.this);
            }
        });
        page.addView(b);
    }

    private void renderSettings() {
        ScrollView sv = new ScrollView(this);
        LinearLayout page = Ui.col(this);
        int p = Ui.dp(this, 24);
        page.setPadding(p, p, p, p);
        page.addView(Ui.text(this, "Settings", 28, Ui.TEXT, true));
        page.addView(Ui.eyebrow(this, "Alerts"));
        page.addView(Ui.text(this, "Notifications on this tablet while no screen of the app is visible.", 14, Ui.TEXT2, false));
        toggle(page, "Approval alerts", "When an agent requests permission", "approvalAlerts", app.setting("approvalAlerts"));
        toggle(page, "Session finished", "When a task completes", "sessionAlerts", app.setting("sessionAlerts"));
        page.addView(Ui.eyebrow(this, "Background"));
        toggle(page, "Keep the connection awake while the screen is off",
                "Alerts arrive at once; uses more battery", "keepAwake", ConnService.keepAwake(this));
        page.addView(Ui.text(this, "While paired, a background connection (the ongoing notification) keeps alerts coming with the app in the "
                + "background. Nothing arrives while that connection is down, or after a restart until you open the app.", 12, Ui.TEXT3, false));
        page.addView(Ui.eyebrow(this, "Audit history"));
        page.addView(Ui.text(this, "Every approval decision recorded on this tablet, newest first.", 14, Ui.TEXT2, false));
        List<JSONObject> rows = app.auditRows();
        if (rows.isEmpty()) page.addView(Ui.text(this, "No decisions yet. Approve or reject a request and the entry appears here.", 14, Ui.TEXT3, false));
        for (int i = 0; i < Math.min(50, rows.size()); i++) {
            JSONObject r = rows.get(i);
            String d = r.optString("decision");
            boolean ok = d.startsWith("approve") || d.equals("approved");
            boolean no = d.equals("reject") || d.equals("rejected");
            LinearLayout row = Ui.row(this);
            row.setPadding(0, Ui.dp(this, 8), 0, Ui.dp(this, 8));
            LinearLayout t = Ui.col(this);
            LinearLayout head = Ui.row(this);
            head.addView(Ui.text(this, ok ? "Approved" : no ? "Rejected" : Ui.capital(d), 14, ok ? Ui.SUCCESS : no ? Ui.ERROR : Ui.TEXT2, true));
            head.addView(Ui.gap(this, 8));
            head.addView(Ui.text(this, r.optString("agent") + " · " + r.optString("risk") + " risk" + (r.optBoolean("fromPc") ? " · from a PC session" : ""), 12, Ui.TEXT3, false));
            t.addView(head);
            TextView cmd = Ui.mono(this, Ui.ellipsize(r.optString("command"), 160), Ui.TEXT2, 0);
            cmd.setMaxLines(2);
            t.addView(cmd);
            row.addView(t, Ui.weight(1));
            row.addView(Ui.text(this, Ui.ago(r.optLong("ts")), 12, Ui.TEXT3, false));
            page.addView(row);
        }
        page.addView(Ui.text(this, "Stored in app-private storage on this tablet only.", 12, Ui.TEXT3, false));
        sv.addView(page);
        content.addView(sv);
    }

    @Override
    protected void onActivityResult(int req, int result, Intent data) {
        super.onActivityResult(req, result, data);
        if (req == SCAN && result == RESULT_OK && data != null) {
            prefillPayload = data.getStringExtra("text");
            screen = PAIR;
            render();
        }
        if (req == PHOTO && result == RESULT_OK && data != null && photoFor != null && data.getStringExtra("image") != null) {
            List<String> d = app.draft(photoFor);
            if (d.size() < Composer.IMAGES_MAX) d.add(data.getStringExtra("image"));
            if (conversation != null && conversation.sessionId.equals(photoFor)) conversation.refreshThumbs();
        }
    }
}
