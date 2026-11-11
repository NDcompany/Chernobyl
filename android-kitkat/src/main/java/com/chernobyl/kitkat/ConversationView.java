package com.chernobyl.kitkat;

import android.app.AlertDialog;
import android.content.Context;
import android.content.DialogInterface;
import android.graphics.Bitmap;
import android.graphics.BitmapFactory;
import android.graphics.Typeface;
import android.os.Handler;
import android.os.Looper;
import android.text.Editable;
import android.text.InputFilter;
import android.text.InputType;
import android.text.TextWatcher;
import android.util.Base64;
import android.view.Gravity;
import android.view.View;
import android.view.ViewGroup;
import android.widget.AbsListView;
import android.widget.BaseAdapter;
import android.widget.Button;
import android.widget.EditText;
import android.widget.FrameLayout;
import android.widget.HorizontalScrollView;
import android.widget.ImageView;
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
 * inline approvals, turn footers, the working row and the composer. Live
 * sessions get the ADR-017 chips (MCP, mode, model, context, rename, End),
 * photos and the slash menu; a session live on the PC (ADR-018) is mirrored
 * and a prompt forks it into a new session.
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
    private final LinearLayout header, chips, notes;
    private final LinearLayout working;
    private final Ui.Mark workingMark;
    private final TextView workingText, workingMeta;
    private final EditText input;
    private final Button send, stop, photo;
    private final TextView hint;
    private final LinearLayout slashMenu, thumbs;
    private final Handler tick = new Handler(Looper.getMainLooper());
    private String focusPermission;
    private boolean disposed;
    private String tooLarge;
    private AlertDialog mcpDialog;
    private LinearLayout mcpBody;
    private String mcpShown;
    private final Map<String, String> pasted = new HashMap<String, String>();

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

        LinearLayout top = Ui.col(c);
        top.setBackgroundColor(Ui.BLACK);
        header = Ui.row(c);
        int p = Ui.dp(c, 12);
        header.setPadding(p, p, p, p);
        top.addView(header, Ui.lp(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT));
        if (onBack != null) {
            Button back = Ui.button(c, "‹ Back", 2, onBack);
            back.setTextColor(Ui.WHITE);
            header.addView(back);
        }
        chips = Ui.row(c);
        HorizontalScrollView chipScroll = new HorizontalScrollView(c);
        chipScroll.setHorizontalScrollBarEnabled(false);
        chipScroll.addView(chips);
        chipScroll.setPadding(p, 0, p, p);
        top.addView(chipScroll);
        addView(top, Ui.lp(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT));
        notes = Ui.col(c);
        notes.setPadding(p, 0, p, 0);
        addView(notes, Ui.lp(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT));

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
        workingText = Ui.text(c, "", 14, Ui.TEXT2, true);
        workingText.setSingleLine(true);
        working.addView(workingText);
        working.addView(Ui.gap(c, 10));
        workingMeta = Ui.text(c, "", 12, Ui.TEXT3, false);
        working.addView(workingMeta);
        list.addFooterView(working, null, false);
        list.setAdapter(adapter);
        addView(list, new LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, 0, 1));

        LinearLayout composer = Ui.col(c);
        composer.setPadding(p, Ui.dp(c, 8), p, p);
        composer.setBackgroundDrawable(Ui.box(Ui.WHITE, Ui.BORDER, 0, c));
        slashMenu = Ui.col(c);
        composer.addView(slashMenu);
        thumbs = Ui.row(c);
        composer.addView(thumbs);
        hint = Ui.text(c, "", 12, Ui.TEXT3, false);
        composer.addView(hint);
        LinearLayout r = Ui.row(c);
        photo = Ui.button(c, "Photo", 1, new OnClickListener() {
            @Override
            public void onClick(View v) {
                int budget = Composer.budgetLeft(app.draft(ConversationView.this.sessionId), Composer.IMAGES_DATA_MAX);
                if (budget < 4096) { app.toast("No room for another photo: send what you have"); return; }
                ((MainActivity) getContext()).takePhoto(ConversationView.this.sessionId, budget);
            }
        });
        r.addView(photo);
        r.addView(Ui.gap(c, 8));
        input = new EditText(c);
        input.setHint("Message the agent");
        input.setInputType(InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_FLAG_MULTI_LINE | InputType.TYPE_TEXT_FLAG_CAP_SENTENCES);
        input.setMaxLines(6);
        input.setFilters(new InputFilter[] {new InputFilter.LengthFilter(32000)});
        input.addTextChangedListener(new TextWatcher() {
            @Override public void beforeTextChanged(CharSequence s, int a, int b, int n) { }
            @Override public void onTextChanged(CharSequence s, int a, int b, int n) { }
            @Override public void afterTextChanged(Editable e) { slash(); }
        });
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
        refreshThumbs();
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
        if (mcpDialog != null) mcpDialog.dismiss();
        if (live) app.closeView(sessionId);
    }

    private JSONObject session() {
        return app.model.session(sessionId);
    }

    private boolean livePc() {
        JSONObject s = session();
        return s != null && s.optBoolean("livePc");
    }

    /** ADR-017 extras this runtime takes (`Agent.live.composer`); the rest stay hidden. */
    private boolean offers(String feature) {
        JSONObject a = agent();
        JSONObject l = a == null ? null : a.optJSONObject("live");
        JSONArray cs = l == null ? null : l.optJSONArray("composer");
        for (int i = 0; live && cs != null && i < cs.length(); i++) if (feature.equals(cs.optString(i))) return true;
        return false;
    }

    private void submit() {
        final String text = input.getText().toString().trim();
        if (text.isEmpty()) return;
        final boolean fork = livePc();
        final List<String> images = app.draft(sessionId);
        JSONArray imgs = null;
        if (!images.isEmpty()) {
            imgs = new JSONArray();
            for (String d : images) {
                try { imgs.put(new JSONObject().put("mediaType", "image/jpeg").put("data", d)); } catch (Exception ignored) { }
            }
        }
        JSONObject m = App.msg("submit_prompt", "sessionId", sessionId, "text", text, "images", imgs, "fork", fork ? Boolean.TRUE : null);
        // one WebSocket frame carries the whole prompt (ADR-017): check before sending
        tooLarge = imgs != null && Composer.sealedFrameBytes(m.toString()) > Composer.FRAME_MAX_BYTES
                ? "Too large for one message: remove a photo or shorten the text" : null;
        if (tooLarge != null) { changed(); return; }
        send.setEnabled(false);
        app.command(m, null, new App.Result() {
            @Override
            public void done(JSONObject ack, String error) {
                send.setEnabled(true);
                if (ack == null || !ack.optBoolean("ok")) return;
                input.setText("");
                images.clear();
                refreshThumbs();
                list.post(new Runnable() { @Override public void run() { list.setSelection(adapter.getCount()); } });
                String next = ack.optString("sessionId");
                // ADR-018: the fork is a new session; the PC one is untouched
                if (fork && !next.isEmpty() && !next.equals(sessionId)) ((MainActivity) getContext()).openSession(next);
            }
        });
    }

    /** A photo from the camera screen (MainActivity.onActivityResult). */
    void refreshThumbs() {
        Context c = getContext();
        thumbs.removeAllViews();
        final List<String> images = app.draft(sessionId);
        for (int i = 0; i < images.size(); i++) {
            final String data = images.get(i);
            FrameLayout f = new FrameLayout(c);
            ImageView iv = new ImageView(c);
            iv.setScaleType(ImageView.ScaleType.CENTER_CROP);
            BitmapFactory.Options o = new BitmapFactory.Options();
            o.inSampleSize = 8;
            byte[] raw = Base64.decode(data, Base64.DEFAULT);
            Bitmap b = BitmapFactory.decodeByteArray(raw, 0, raw.length, o);
            iv.setImageBitmap(b);
            int s = Ui.dp(c, 64);
            f.addView(iv, new FrameLayout.LayoutParams(s, s));
            TextView x = Ui.text(c, "✕", 13, Ui.WHITE, true);
            x.setBackgroundDrawable(Ui.box(Ui.BLACK, 0, 999, c));
            x.setGravity(Gravity.CENTER);
            x.setContentDescription("Remove photo " + (i + 1));
            x.setOnClickListener(new OnClickListener() {
                @Override
                public void onClick(View v) {
                    images.remove(data);
                    tooLarge = null;
                    refreshThumbs();
                }
            });
            f.addView(x, new FrameLayout.LayoutParams(Ui.dp(c, 24), Ui.dp(c, 24), Gravity.TOP | Gravity.END));
            LinearLayout.LayoutParams lp = Ui.lp(s, s);
            lp.setMargins(0, Ui.dp(c, 4), Ui.dp(c, 8), Ui.dp(c, 4));
            thumbs.addView(f, lp);
        }
        if (!images.isEmpty()) thumbs.addView(Ui.text(c, images.size() + " of " + Composer.IMAGES_MAX + " photos", 12, Ui.TEXT2, false));
        if (!disposed) changed();
    }

    /** "/" at the start lists the session's slash commands (ADR-017). */
    private void slash() {
        slashMenu.removeAllViews();
        if (!offers("commands")) return;
        String q = Composer.slashQuery(input.getText().toString());
        JSONArray names = view.state.optJSONArray("slashCommands");
        if (q == null || names == null) return;
        List<String> all = new ArrayList<String>();
        for (int i = 0; i < names.length(); i++) if (names.opt(i) instanceof String) all.add(names.optString(i));
        Context c = getContext();
        for (final String name : Composer.filterSlash(all, q, 8)) {
            // not Ui.mono: un-selecting a selectable TextView overflows KitKat's stack under accessibility
            TextView t = Ui.text(c, "/" + name, 13, Ui.TEXT, false);
            t.setTypeface(Typeface.MONOSPACE);
            t.setPadding(Ui.dp(c, 12), Ui.dp(c, 8), Ui.dp(c, 12), Ui.dp(c, 8));
            t.setOnClickListener(new OnClickListener() {
                @Override
                public void onClick(View v) {
                    input.setText("/" + name + " ");
                    input.setSelection(input.getText().length());
                }
            });
            slashMenu.addView(t);
        }
        if (slashMenu.getChildCount() > 0) slashMenu.setBackgroundDrawable(Ui.box(Ui.WHITE, Ui.BORDER, 12, c));
    }

    private JSONObject agent() {
        JSONObject s = session();
        return s == null ? null : app.model.agent(s.optString("agentId"));
    }

    private boolean running() {
        JSONObject s = session();
        if (live) return "running".equals(view.state.optString("status")) || "waiting".equals(view.state.optString("status")) || Model.runningTurn(view) != null;
        return s != null && ("running".equals(s.optString("status")) || "waiting".equals(s.optString("status")));
    }

    private void buildHeader() {
        Context c = getContext();
        while (header.getChildCount() > (header.getChildAt(0) instanceof Button ? 1 : 0)) header.removeViewAt(header.getChildCount() - 1);
        JSONObject s = session();
        JSONObject a = agent();
        if (a != null) {
            header.addView(new Ui.Mark(c, a.optString("kind"), 24));
            header.addView(Ui.gap(c, 10));
        }
        LinearLayout t = Ui.col(c);
        t.addView(Ui.text(c, s == null ? "Session" : s.optString("title", "Session"), 17, Ui.WHITE, true));
        String sub = (a == null ? "Unknown agent" : a.optString("name")) + (s != null && !s.optString("project").isEmpty() ? " · " + s.optString("project") : "");
        TextView subtitle = Ui.text(c, sub, 13, 0xFFBBBBBB, false);
        subtitle.setSingleLine(true);
        subtitle.setEllipsize(android.text.TextUtils.TruncateAt.MIDDLE);
        t.addView(subtitle);
        header.addView(t, Ui.weight(1));
        if (a != null && a.optBoolean("simulated")) {
            header.addView(Ui.simulated(c));
            header.addView(Ui.gap(c, 6));
        }
        if (livePc()) {
            header.addView(Ui.livePc(c, true));
        } else {
            String st = live && !view.epoch.isEmpty() ? view.state.optString("status") : s == null ? "" : s.optString("status");
            header.addView(Ui.statusPill(c, st));
        }
        buildChips(s, a);
    }

    /** The chip row under the title, like the main app's session header. */
    private void buildChips(JSONObject s, JSONObject a) {
        final Context c = getContext();
        chips.removeAllViews();
        if (!live || s == null) return;
        final boolean pc = livePc();
        boolean ended = "ended".equals(view.state.optString("status"));
        JSONObject caps = a == null ? null : a.optJSONObject("live");
        int[] mcp = Composer.mcpSummary(view.mcp);
        if (view.mcp.length() > 0) {
            chips.addView(Ui.chip(c, "MCP " + mcp[0] + "/" + mcp[1] + (mcp[2] == 1 ? "  ●" : ""), new OnClickListener() {
                @Override public void onClick(View v) { openMcp(); }
            }));
        }
        final JSONObject ws = app.model.has("workspace.v1") ? app.model.workspaceForProject(s.optString("project")) : null;
        if (ws != null) {
            chips.addView(Ui.chip(c, "Files", new OnClickListener() {
                @Override public void onClick(View v) { ((MainActivity) getContext()).openWorkspace(ws.optString("id"), 0); }
            }));
            chips.addView(Ui.chip(c, "Changes", new OnClickListener() {
                @Override public void onClick(View v) { ((MainActivity) getContext()).openWorkspace(ws.optString("id"), 1); }
            }));
        }
        final JSONArray modes = caps == null ? null : caps.optJSONArray("permissionModes");
        if (modes != null && !pc) {
            final String mode = view.state.optString("permissionMode", null);
            chips.addView(Ui.chip(c, Composer.modeLabel(mode) + "  ▾", new OnClickListener() {
                @Override public void onClick(View v) { pickMode(mode, modes); }
            }));
        }
        if (offers("model") && !ended && !pc) {
            final List<String[]> choices = Composer.modelChoices(view.state);
            final String model = view.state.optString("model", "");
            chips.addView(Ui.chip(c, Composer.modelLabel(model, choices) + "  ▾", new OnClickListener() {
                @Override public void onClick(View v) { pickModel(model, choices); }
            }));
        }
        int pct = offers("context") ? Composer.contextPercent(view.state) : -1;
        if (pct >= 0) chips.addView(Ui.chip(c, (pct >= 80 ? "● " : "") + "Context " + pct + "%", null));
        if (offers("rename") && !ended && !pc) {
            chips.addView(Ui.chip(c, "✎ Rename", new OnClickListener() {
                @Override public void onClick(View v) { rename(); }
            }));
        }
        if (caps != null && caps.optBoolean("end") && !ended && !pc) {
            chips.addView(Ui.chip(c, "■ End session", new OnClickListener() {
                @Override public void onClick(View v) { confirmEnd(); }
            }));
        }
    }

    private void pickMode(final String current, JSONArray modes) {
        Context c = getContext();
        LinearLayout body = Ui.col(c);
        final AlertDialog d = Ui.sheet(c, "Permission mode", body);
        for (int i = 0; i < modes.length(); i++) {
            final String m = modes.optString(i);
            if (!Composer.isModeChoice(m)) continue; // never a bypass mode
            body.addView(Ui.choiceRow(c, Composer.modeLabel(m), Composer.modeHint(m), m.equals(current), new OnClickListener() {
                @Override
                public void onClick(View v) {
                    d.dismiss();
                    if (!m.equals(current)) app.command(App.msg("set_permission_mode", "sessionId", sessionId, "mode", m), null, null);
                }
            }));
        }
        d.show();
    }

    private void pickModel(final String current, final List<String[]> choices) {
        Context c = getContext();
        LinearLayout body = Ui.col(c);
        body.addView(Ui.text(c, "The next turn uses it. Claude Code on your PC decides which models your account may use.", 13, Ui.TEXT2, false));
        body.addView(Ui.gap(c, 8));
        final AlertDialog d = Ui.sheet(c, "Model", body);
        String label = Composer.modelLabel(current, choices);
        for (final String[] m : choices) {
            final boolean on = m[0].equals(current) || label.equals(m[1]);
            body.addView(Ui.choiceRow(c, m[1], m[0], on, new OnClickListener() {
                @Override
                public void onClick(View v) {
                    d.dismiss();
                    if (!on) app.command(App.msg("set_model", "sessionId", sessionId, "model", m[0]), null, null);
                }
            }));
        }
        d.show();
    }

    private void rename() {
        final Context c = getContext();
        JSONObject s = session();
        final String title = s == null ? "" : s.optString("title");
        LinearLayout body = Ui.col(c);
        final EditText field = new EditText(c);
        field.setSingleLine(true);
        field.setHint("Session name");
        field.setFilters(new InputFilter[] {new InputFilter.LengthFilter(Composer.TITLE_MAX)});
        field.setText(title);
        body.addView(field);
        final TextView count = Ui.text(c, "", 12, Ui.TEXT3, false);
        body.addView(count);
        final AlertDialog d = new AlertDialog.Builder(c).setTitle("Rename session").setView(body).setPositiveButton("Rename", null)
                .setNegativeButton("Cancel", null).create();
        int p = Ui.dp(c, 16);
        body.setPadding(p, p, p, 0);
        final Runnable check = new Runnable() {
            @Override
            public void run() {
                String next = field.getText().toString().trim();
                count.setText(next.length() + "/" + Composer.TITLE_MAX + " · the name shows in Claude Code on your PC too");
                Button ok = d.getButton(AlertDialog.BUTTON_POSITIVE);
                if (ok != null) ok.setEnabled(Composer.isSessionTitle(next) && !next.equals(title));
            }
        };
        field.addTextChangedListener(new TextWatcher() {
            @Override public void beforeTextChanged(CharSequence x, int a, int b, int n) { }
            @Override public void onTextChanged(CharSequence x, int a, int b, int n) { }
            @Override public void afterTextChanged(Editable e) { check.run(); }
        });
        d.setOnShowListener(new DialogInterface.OnShowListener() {
            @Override
            public void onShow(DialogInterface di) {
                check.run();
                d.getButton(AlertDialog.BUTTON_POSITIVE).setOnClickListener(new OnClickListener() {
                    @Override
                    public void onClick(View v) {
                        d.dismiss();
                        app.command(App.msg("rename_session", "sessionId", sessionId, "title", field.getText().toString().trim()), "Renamed", null);
                    }
                });
            }
        });
        d.show();
    }

    private void confirmEnd() {
        JSONObject a = agent();
        String name = a == null ? "The agent" : a.optString("name", "The agent");
        new AlertDialog.Builder(getContext()).setTitle("End this session?")
                .setMessage(name + " stops on your PC and this conversation takes no more prompts.\n\nThe conversation history stays on your PC.")
                .setPositiveButton("End session", new DialogInterface.OnClickListener() {
                    @Override
                    public void onClick(DialogInterface d, int w) {
                        app.command(App.msg("session_end", "sessionId", sessionId), null, null);
                    }
                }).setNegativeButton("Cancel", null).show();
    }

    /* ---------------- MCP sheet (§4.7) ---------------- */

    private void openMcp() {
        Context c = getContext();
        mcpBody = Ui.col(c);
        mcpDialog = Ui.sheet(c, "MCP servers", mcpBody);
        mcpDialog.setOnDismissListener(new DialogInterface.OnDismissListener() {
            @Override public void onDismiss(DialogInterface d) { mcpDialog = null; }
        });
        fillMcp();
        mcpDialog.show();
    }

    /** Server rows with their status and the actions the runtime offers; names are untrusted plain text. */
    private void fillMcp() {
        final Context c = getContext();
        mcpBody.removeAllViews();
        mcpShown = view.mcp.toString();
        if (view.mcp.length() == 0) mcpBody.addView(Ui.text(c, "No MCP servers in this session.", 14, Ui.TEXT2, false));
        for (int i = 0; i < view.mcp.length(); i++) {
            JSONObject s = view.mcp.optJSONObject(i);
            if (s == null) continue;
            final String name = s.optString("name");
            String st = s.optString("status");
            LinearLayout card = Ui.card(c, false);
            LinearLayout head = Ui.row(c);
            int tone = st.equals("connected") ? Ui.SUCCESS : st.equals("failed") ? Ui.ERROR : st.equals("needs-auth") ? Ui.WARNING : Ui.TEXT3;
            head.addView(Ui.text(c, "● ", 14, tone, true));
            TextView n = Ui.text(c, name, 15, Ui.TEXT, true);
            n.setSingleLine(true);
            head.addView(n, Ui.weight(1));
            head.addView(Ui.text(c, st.equals("connected") ? "Connected" : st.equals("pending") ? "Connecting…" : st.equals("failed") ? "Failed"
                    : st.equals("needs-auth") ? "Needs sign-in" : st.equals("disabled") ? "Disabled" : st, 13, Ui.TEXT2, false));
            card.addView(head);
            if (!s.optString("error").isEmpty()) card.addView(Ui.text(c, s.optString("error"), 12, Ui.TEXT2, false));
            JSONObject auth = s.optJSONObject("auth");
            String as = auth == null ? "" : auth.optString("state");
            if (auth != null) {
                String what = as.equals("starting") ? "Starting sign-in…" : as.equals("awaiting-browser") ? "Finish signing in in the browser on your PC"
                        : as.equals("awaiting-callback") ? "Sign in on this tablet, then paste the address the browser lands on"
                        : as.equals("completing") ? "Completing sign-in…" : as.equals("failed") ? "Sign-in failed" : as;
                card.addView(Ui.text(c, what + (auth.optString("error").isEmpty() ? "" : ": " + auth.optString("error")), 12, as.equals("failed") ? Ui.ERROR : Ui.TEXT2, false));
            }
            String flows = String.valueOf(s.optJSONArray("authFlows"));
            boolean act = st.equals("needs-auth") || st.equals("failed") || auth != null;
            boolean busy = as.equals("starting") || as.equals("completing");
            LinearLayout actions = Ui.col(c);
            if (act && flows.contains("\"desktop\"")) actions.addView(mcpButton("Sign in on PC", busy, App.msg("mcp_auth", "sessionId", sessionId, "server", name, "flow", "desktop")));
            if (act && flows.contains("\"phone-callback\"")) {
                final String url = as.equals("awaiting-callback") && auth.optString("url").matches("(?i)^https?://.*") ? auth.optString("url") : null;
                if (url == null) {
                    actions.addView(mcpButton("Finish sign-in on this tablet", busy, App.msg("mcp_auth", "sessionId", sessionId, "server", name, "flow", "phone-callback")));
                } else {
                    final String prefix = auth.optString("callbackPrefix", null);
                    actions.addView(Ui.mono(c, url, Ui.TEXT2, Ui.SURFACE2));
                    actions.addView(Ui.button(c, "Open sign-in page", 0, new OnClickListener() {
                        @Override public void onClick(View v) { Ui.confirmOpen(getContext(), url); }
                    }));
                    final EditText cb = new EditText(c);
                    cb.setSingleLine(true);
                    cb.setHint(prefix != null ? "Paste the " + prefix + "… address" : "Paste the callback address");
                    cb.setText(pasted.containsKey(name) ? pasted.get(name) : "");
                    cb.setFilters(new InputFilter[] {new InputFilter.LengthFilter(4096)});
                    actions.addView(cb);
                    final Button finish = Ui.button(c, "Finish sign-in", 1, new OnClickListener() {
                        @Override
                        public void onClick(View v) {
                            String u = cb.getText().toString().trim();
                            if (!Composer.isCallbackUrl(u, prefix)) { cb.setError("Paste the address that starts with " + (prefix != null ? prefix : "http://")); return; }
                            app.command(App.msg("mcp_auth_callback", "sessionId", sessionId, "server", name, "callbackUrl", u), null, null);
                        }
                    });
                    finish.setEnabled(!busy);
                    cb.addTextChangedListener(new TextWatcher() {
                        @Override public void beforeTextChanged(CharSequence x, int a, int b, int n) { }
                        @Override public void onTextChanged(CharSequence x, int a, int b, int n) { }
                        @Override public void afterTextChanged(Editable e) { pasted.put(name, e.toString()); }
                    });
                    actions.addView(finish);
                }
            }
            if (act && flows.contains("\"claude-ai\"")) {
                actions.addView(Ui.button(c, "Manage on claude.ai", 1, new OnClickListener() {
                    @Override public void onClick(View v) { Ui.confirmOpen(getContext(), "https://claude.ai/customize/connectors"); }
                }));
            }
            if (st.equals("failed")) actions.addView(mcpButton("Reconnect", false, App.msg("mcp_reconnect", "sessionId", sessionId, "server", name)));
            card.addView(actions);
            Ui.addMargin(card, 0, 0, 0, 8);
            mcpBody.addView(card);
        }
        mcpBody.addView(Ui.text(c, "Adding or removing servers and clearing credentials stay on the PC.", 12, Ui.TEXT3, false));
    }

    private Button mcpButton(String label, boolean busy, final JSONObject m) {
        Button b = Ui.button(getContext(), label, 1, new OnClickListener() {
            @Override
            public void onClick(View v) {
                // a fresh commandId per tap: a repeated tap is a new request
                try { m.put("commandId", Transport.nextId("cmd")); } catch (Exception ignored) { }
                app.command(m, null, null);
            }
        });
        b.setEnabled(!busy);
        b.setAlpha(busy ? 0.4f : 1f);
        Ui.addMargin(b, 0, 6, 0, 0);
        return b;
    }

    @Override
    public void changed() {
        if (disposed) return;
        JSONObject known = session();
        if (!live && known != null && known.optBoolean("transcript") && app.model.has("transcript.v1")) {
            // opened before the companion listed it (a fork's ack can beat its session_created): reopen live
            ((MainActivity) getContext()).openSession(sessionId);
            return;
        }
        buildHeader();
        // the sheet follows the runtime's state; rebuilt only when it changed (taps stay on their buttons)
        if (mcpDialog != null && !view.mcp.toString().equals(mcpShown)) fillMcp();
        rows.clear();
        long now = System.currentTimeMillis();
        List<JSONObject> pending = app.model.pendingFor(sessionId, now);
        JSONObject s = session();
        notes.removeAllViews();
        Context c = getContext();
        if (s != null && !s.optString("forkedFrom").isEmpty())
            notes.addView(note(c, "Forked from “" + s.optString("forkedFrom") + "”; the PC session is unchanged.", Ui.TEXT2));
        if (live && !app.connected() && !view.epoch.isEmpty())
            notes.addView(note(c, "Not connected: showing what arrived so far. The conversation catches up on reconnect.", Ui.WARNING));
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
        boolean pc = livePc();
        // a PC session is mirrored: a prompt forks it, nothing waits on it here (ADR-018)
        boolean busy = !pc && running();
        String mode = liveCaps == null ? "queue" : liveCaps.optString("promptWhileBusy", "none");
        boolean ended = live ? "ended".equals(view.state.optString("status")) : s != null && "completed".equals(s.optString("status"));
        stop.setVisibility(busy && !pc && (liveCaps == null || liveCaps.optBoolean("interrupt")) ? VISIBLE : GONE);
        photo.setVisibility(offers("images") && !ended ? VISIBLE : GONE);
        boolean full = app.draft(sessionId).size() >= Composer.IMAGES_MAX;
        photo.setEnabled(!full);
        photo.setAlpha(full ? 0.35f : 1f);
        boolean canSend = app.connected() && s != null && !ended && !(busy && mode.equals("none"));
        send.setEnabled(canSend);
        send.setAlpha(canSend ? 1f : 0.35f);
        String name = a == null ? "the agent" : a.optString("name", "the agent");
        input.setHint(pc ? "Continue on phone (fork)…" : "Message " + name + "…");
        String h = tooLarge != null ? tooLarge : !app.connected() ? "Offline: messages cannot be sent" : s == null ? "This session is not in the companion's list"
                : ended ? "This session has ended"
                : pc ? "Live on PC: sending forks it into a new session here; the PC session is untouched"
                : busy ? (mode.equals("merge") ? "Joins the current turn" : mode.equals("queue") ? "Queued: runs after this turn" : "Wait for this turn to finish")
                : live && view.error != null ? "Live view refused: " + view.error
                : live && view.epoch.isEmpty() ? "Loading the conversation…"
                : live && "starting".equals(view.state.optString("status")) ? "Starting " + name + "…" : "";
        hint.setText(h);
        hint.setTextColor(tooLarge != null ? Ui.ERROR : Ui.TEXT3);
    }

    private static TextView note(Context c, String text, int color) {
        TextView t = Ui.text(c, text, 13, color, false);
        t.setPadding(0, Ui.dp(c, 8), 0, 0);
        return t;
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

    /** The main app's working row: mark, rotating verb / "Running Bash…" / "Waiting for you", elapsed, usage. */
    private void updateWorking() {
        long now = System.currentTimeMillis();
        List<JSONObject> pending = app.model.pendingFor(sessionId, now);
        if (!live) {
            boolean busy = running();
            working.setVisibility(busy ? VISIBLE : GONE);
            if (!busy) return;
            JSONObject a = agent();
            String name = a == null ? "Agent" : a.optString("name", "Agent");
            workingMark.setState(pending.isEmpty() ? "working" : "waiting");
            workingText.setText(pending.isEmpty() ? name + " is working" : name + " is waiting for you");
            workingMeta.setText("");
            return;
        }
        Object[] w = Composer.workingStatus(view, pending);
        working.setVisibility(w == null ? GONE : VISIBLE);
        if (w == null) return;
        String state = (String) w[0];
        JSONObject turn = (JSONObject) w[2];
        long elapsed = now - turn.optLong("startedAt", now);
        workingMark.setState(state);
        workingText.setText(Composer.workingLabel(state, elapsed, turn.optString("id"), (String) w[1]));
        workingText.setTextColor("waiting".equals(state) ? Ui.TEXT : Ui.TEXT2);
        StringBuilder meta = new StringBuilder(Composer.formatElapsed(elapsed));
        JSONObject u = turn.optJSONObject("usage");
        if (u != null && u.optLong("outputTokens") > 0) meta.append(" · ").append(tokens(u.optLong("outputTokens"))).append(" tokens");
        if (turn.has("costUsd")) meta.append(String.format(java.util.Locale.US, " · $%.4f", turn.optDouble("costUsd")));
        workingMeta.setText(meta.toString());
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
