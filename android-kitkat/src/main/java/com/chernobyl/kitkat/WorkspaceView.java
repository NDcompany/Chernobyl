package com.chernobyl.kitkat;

import android.app.AlertDialog;
import android.content.Context;
import android.view.View;
import android.view.ViewGroup;
import android.widget.AdapterView;
import android.widget.ArrayAdapter;
import android.widget.Button;
import android.widget.EditText;
import android.widget.HorizontalScrollView;
import android.widget.LinearLayout;
import android.widget.ScrollView;
import android.widget.Spinner;
import android.widget.TextView;
import java.util.List;
import org.json.JSONArray;
import org.json.JSONObject;

/**
 * ADR-016 Files / Changes for one workspace: browse and read files, git status,
 * diffs, stage / unstage, commit, pull (fast-forward, confirmed upstream →
 * branch), push (confirmed branch → upstream). Reads show their errors inline;
 * changes resolve with their command_result (git's own summary on failure).
 * ADR-020 Terminal tab: one command line at a time, streamed output, Stop; only
 * when the PC turned it on (terminal.v1).
 */
final class WorkspaceView extends LinearLayout implements App.Change {
    private final App app = App.app;
    private final LinearLayout body;
    private final TextView status;
    private List<JSONObject> workspaces;
    private String wsId;
    private String path = "";
    private LinearLayout termOut;
    private String termShown;
    private boolean termRunning;
    private final Button[] tabButtons = new Button[3];

    WorkspaceView(Context c) {
        super(c);
        setOrientation(VERTICAL);
        int p = Ui.dp(c, 16);
        setPadding(p, p, p, 0);
        addView(Ui.text(c, "Files & changes", 24, Ui.TEXT, true));
        status = Ui.text(c, "", 13, Ui.TEXT2, false);
        addView(status);
        ScrollView sv = new ScrollView(c);
        body = Ui.col(c);
        body.setPadding(0, 0, 0, Ui.dp(c, 24));
        sv.addView(body);

        if (!app.model.has("workspace.v1")) {
            status.setText(app.model.synced ? "This companion does not offer files and git (workspace.v1)." : "Connect to your companion first.");
            return;
        }
        workspaces = app.model.workspaces();
        if (workspaces.isEmpty()) {
            status.setText("No workspace yet: give a claude or codex adapter a workspace folder in the companion's config.");
            return;
        }
        LinearLayout bar = Ui.row(c);
        bar.setPadding(0, Ui.dp(c, 8), 0, Ui.dp(c, 8));
        final Spinner pick = new Spinner(c);
        String[] labels = new String[workspaces.size()];
        for (int i = 0; i < labels.length; i++) labels[i] = workspaces.get(i).optString("label") + "  —  " + workspaces.get(i).optString("path");
        ArrayAdapter<String> ad = new ArrayAdapter<String>(c, android.R.layout.simple_spinner_item, labels);
        ad.setDropDownViewResource(android.R.layout.simple_spinner_dropdown_item);
        pick.setAdapter(ad);
        pick.setOnItemSelectedListener(new AdapterView.OnItemSelectedListener() {
            @Override
            public void onItemSelected(AdapterView<?> parent, View v, int pos, long id) {
                wsId = workspaces.get(pos).optString("id");
                app.filesWs = wsId;
                path = "";
                load();
            }

            @Override
            public void onNothingSelected(AdapterView<?> parent) { }
        });
        for (int i = 0; i < workspaces.size(); i++) if (workspaces.get(i).optString("id").equals(app.filesWs)) pick.setSelection(i);
        bar.addView(pick, Ui.weight(1));
        String[] tabs = {"Files", "Changes", "Terminal"};
        for (int i = 0; i < tabs.length; i++) {
            final int tab = i;
            bar.addView(Ui.gap(c, 8));
            bar.addView(tabButtons[i] = Ui.button(c, tabs[i], 1, new OnClickListener() {
                @Override public void onClick(View v) { app.filesTab = tab; load(); }
            }));
        }
        addView(bar);
        addView(sv, new LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, 0, 1));
    }

    private void loading(String what) {
        body.removeAllViews();
        body.addView(Ui.text(getContext(), what, 14, Ui.TEXT3, false));
    }

    private void error(String e) {
        body.removeAllViews();
        body.addView(Ui.text(getContext(), e, 14, Ui.ERROR, false));
        body.addView(Ui.button(getContext(), "Retry", 1, new OnClickListener() {
            @Override public void onClick(View v) { load(); }
        }));
    }

    void load() {
        termOut = null;
        if (wsId == null) return;
        for (int i = 0; i < tabButtons.length; i++) {
            boolean on = i == app.filesTab;
            tabButtons[i].setTextColor(on ? Ui.WHITE : Ui.TEXT);
            tabButtons[i].setBackgroundDrawable(Ui.box(on ? Ui.BLACK : Ui.WHITE, Ui.BLACK, 22, getContext()));
        }
        if (app.filesTab == 1) loadStatus();
        else if (app.filesTab == 2) terminal();
        else list(path);
    }

    @Override
    protected void onAttachedToWindow() {
        super.onAttachedToWindow();
        app.watch(this);
    }

    @Override
    protected void onDetachedFromWindow() {
        app.unwatch(this);
        super.onDetachedFromWindow();
    }

    /** Streamed terminal output; nothing else here follows the model. */
    @Override
    public void changed() {
        if (termOut != null) showRun();
    }

    private JSONObject workspace() {
        for (JSONObject w : workspaces) if (w.optString("id").equals(wsId)) return w;
        return null;
    }

    /* ---------------- terminal (ADR-020) ---------------- */

    private void terminal() {
        final Context c = getContext();
        body.removeAllViews();
        if (!app.model.has("terminal.v1")) {
            body.addView(Ui.text(c, "The terminal is off on this PC", 17, Ui.TEXT, true));
            body.addView(Ui.text(c, "It is opt-in: run setup with -EnableTerminal, or set \"terminal\": {\"enabled\": true} in companion.json and restart the companion.", 14, Ui.TEXT2, false));
            return;
        }
        LinearLayout banner = Ui.col(c);
        banner.setBackgroundDrawable(Ui.box(Ui.WARNING_BG, 0, 12, c));
        int q = Ui.dp(c, 12);
        banner.setPadding(q, q, q, q);
        banner.addView(Ui.text(c, "Runs on your PC without approval prompts", 15, Ui.WARNING, true));
        JSONObject ws = workspace();
        banner.addView(Ui.text(c, "Any command, in " + (ws == null ? "the workspace" : ws.optString("path")) + ". Two minutes at most; every run is logged on the PC.", 13, Ui.TEXT2, false));
        Ui.addMargin(banner, 0, 4, 0, 8);
        body.addView(banner);
        if (!app.connected()) body.addView(Ui.text(c, "Not connected: commands run when the connection to your PC is back.", 13, Ui.WARNING, false));
        final EditText cmd = new EditText(c);
        cmd.setHint("Command, e.g. npm test");
        cmd.setSingleLine(true);
        cmd.setTypeface(android.graphics.Typeface.MONOSPACE);
        cmd.setInputType(android.text.InputType.TYPE_CLASS_TEXT | android.text.InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS);
        cmd.setFilters(new android.text.InputFilter[] {new android.text.InputFilter.LengthFilter(Composer.TERMINAL_COMMAND_MAX)});
        final Composer.TerminalRun run = app.terminals.get(wsId);
        LinearLayout row = Ui.row(c);
        row.addView(cmd, Ui.weight(1));
        row.addView(Ui.gap(c, 8));
        if (run != null && run.running) {
            final Button stop = Ui.button(c, "Stop", 1, null);
            stop.setOnClickListener(new OnClickListener() {
                @Override
                public void onClick(View v) {
                    stop.setText("Stopping…");
                    stop.setEnabled(false);
                    app.stopTerminal(wsId);
                }
            });
            row.addView(stop);
        } else {
            Button go = Ui.button(c, "Run", 0, new OnClickListener() {
                @Override
                public void onClick(View v) {
                    String line = cmd.getText().toString().trim();
                    if (line.isEmpty()) { cmd.setError("Type a command"); return; }
                    if (line.contains("\n") || line.contains("\r")) { cmd.setError("One line only"); return; }
                    app.runTerminal(wsId, line);
                    terminal();
                }
            });
            go.setEnabled(app.connected());
            go.setAlpha(app.connected() ? 1f : 0.35f);
            row.addView(go);
        }
        body.addView(row);
        List<String> history = app.history(wsId);
        if (!history.isEmpty()) {
            body.addView(Ui.eyebrow(c, "Recent"));
            LinearLayout chips = Ui.row(c);
            for (final String h : history) {
                TextView chip = Ui.pill(c, Ui.ellipsize(h, 40), Ui.TEXT, Ui.WHITE);
                chip.setTypeface(android.graphics.Typeface.MONOSPACE);
                chip.setPadding(Ui.dp(c, 12), Ui.dp(c, 8), Ui.dp(c, 12), Ui.dp(c, 8));
                chip.setOnClickListener(new OnClickListener() {
                    @Override
                    public void onClick(View v) {
                        cmd.setText(h);
                        cmd.setSelection(h.length());
                    }
                });
                chips.addView(chip);
                chips.addView(Ui.gap(c, 8));
            }
            HorizontalScrollView hs = new HorizontalScrollView(c);
            hs.setHorizontalScrollBarEnabled(false);
            hs.addView(chips);
            body.addView(hs);
        }
        termOut = Ui.col(c);
        termOut.setPadding(0, Ui.dp(c, 12), 0, 0);
        body.addView(termOut);
        termShown = null;
        showRun();
    }

    /** The run's command, output (its last 60,000 characters), and how it ended. */
    private void showRun() {
        Context c = getContext();
        Composer.TerminalRun run = app.terminals.get(wsId);
        String sig = run == null ? "" : run.commandId + run.output.length() + run.running + run.failed + (run.exit != null);
        if (sig.equals(termShown)) return;
        termShown = sig;
        termOut.removeAllViews();
        if (run == null) return;
        termOut.addView(Ui.text(c, "> " + run.command, 13, Ui.TEXT2, false));
        if (run.output.length() > 0) {
            int cut = Math.max(0, run.output.length() - 60000);
            String out = (cut > 0 ? "… (" + cut + " earlier characters)\n" : "") + run.output.substring(cut);
            HorizontalScrollView hs = new HorizontalScrollView(c);
            hs.addView(Ui.mono(c, out, Ui.WHITE, Ui.BLACK));
            Ui.addMargin(hs, 0, 6, 0, 6);
            termOut.addView(hs);
        }
        if (run.running) termOut.addView(Ui.text(c, "Running…", 14, Ui.TEXT2, false));
        if (run.exit != null) termOut.addView(Ui.text(c, Composer.exitLabel(run.exit), 15, Composer.exitOk(run.exit) ? Ui.SUCCESS : Ui.ERROR, true));
        if (run.failed != null) termOut.addView(Ui.text(c, run.failed, 14, Ui.ERROR, false));
        if (termRunning && !run.running) {
            termRunning = false;
            terminal(); // Stop → Run
            return;
        }
        termRunning = run.running;
    }

    private void list(final String dir) {
        loading("Loading " + (dir.isEmpty() ? "/" : dir) + "…");
        app.query(App.msg("fs_list", "workspaceId", wsId, "path", dir), new App.Result() {
            @Override
            public void done(JSONObject r, String err) {
                if (err != null) { error(err); return; }
                path = dir;
                Context c = getContext();
                body.removeAllViews();
                body.addView(Ui.eyebrow(c, "/" + dir));
                if (!dir.isEmpty()) body.addView(entry("‹ ..", new OnClickListener() {
                    @Override public void onClick(View v) { list(dir.contains("/") ? dir.substring(0, dir.lastIndexOf('/')) : ""); }
                }));
                JSONArray es = r.optJSONArray("entries");
                for (int i = 0; es != null && i < es.length(); i++) {
                    final JSONObject e = es.optJSONObject(i);
                    if (e == null) continue;
                    final String child = dir.isEmpty() ? e.optString("name") : dir + "/" + e.optString("name");
                    final boolean isDir = "dir".equals(e.optString("kind"));
                    body.addView(entry((isDir ? "▸ " : "   ") + e.optString("name") + (isDir ? "/" : "   " + size(e.optLong("size"))), new OnClickListener() {
                        @Override public void onClick(View v) { if (isDir) list(child); else read(child); }
                    }));
                }
                if (r.optBoolean("truncated")) body.addView(Ui.text(c, "Folder list shortened", 12, Ui.TEXT3, false));
            }
        });
    }

    static String size(long b) {
        return b < 1024 ? b + " B" : b < 1048576 ? (b / 1024) + " KB" : (b / 1048576) + " MB";
    }

    private TextView entry(String label, OnClickListener l) {
        TextView t = Ui.text(getContext(), label, 15, Ui.TEXT, false);
        int p = Ui.dp(getContext(), 10);
        t.setPadding(0, p, 0, p);
        t.setOnClickListener(l);
        t.setBackgroundDrawable(Ui.box(Ui.WHITE, 0, 0, getContext()));
        return t;
    }

    private void read(final String file) {
        loading("Opening " + file + "…");
        app.query(App.msg("fs_read", "workspaceId", wsId, "path", file), new App.Result() {
            @Override
            public void done(JSONObject r, String err) {
                if (err != null) { error(err); return; }
                Context c = getContext();
                body.removeAllViews();
                final String dir = file.contains("/") ? file.substring(0, file.lastIndexOf('/')) : "";
                body.addView(Ui.button(c, "‹ Back to /" + dir, 2, new OnClickListener() {
                    @Override public void onClick(View v) { list(dir); }
                }));
                body.addView(Ui.text(c, file + "  ·  " + size(r.optLong("size")), 15, Ui.TEXT, true));
                if (r.optBoolean("sensitive")) body.addView(Ui.text(c, "Sensitive file: every value is masked", 12, Ui.WARNING, false));
                else if (r.optBoolean("redacted")) body.addView(Ui.text(c, "Secret-looking values were masked", 12, Ui.WARNING, false));
                if (r.optBoolean("truncated")) body.addView(Ui.text(c, "Shortened: the first 256 KB", 12, Ui.TEXT3, false));
                HorizontalScrollView hs = new HorizontalScrollView(c);
                hs.addView(Ui.mono(c, Ui.ellipsizeKeep(r.optString("content"), 60000), Ui.WHITE, Ui.BLACK));
                Ui.addMargin(hs, 0, 8, 0, 0);
                body.addView(hs);
            }
        });
    }

    private void loadStatus() {
        loading("Reading git status…");
        app.query(App.msg("git_status", "workspaceId", wsId), new App.Result() {
            @Override
            public void done(JSONObject r, String err) {
                if (err != null) { error(err); return; }
                showStatus(r);
            }
        });
    }

    private void showStatus(JSONObject r) {
        final Context c = getContext();
        body.removeAllViews();
        String branch = r.isNull("branch") ? "detached HEAD" : r.optString("branch");
        String up = r.has("upstream") ? "  ·  " + r.optString("upstream") + "  ↑" + r.optInt("ahead") + " ↓" + r.optInt("behind") : "  ·  no upstream";
        body.addView(Ui.text(c, branch + up, 15, Ui.TEXT, true));
        section("Staged", r.optJSONArray("staged"), true);
        section("Changed", r.optJSONArray("unstaged"), false);
        JSONArray untracked = r.optJSONArray("untracked");
        if (untracked != null && untracked.length() > 0) {
            body.addView(Ui.eyebrow(c, "Untracked"));
            for (int i = 0; i < untracked.length(); i++) body.addView(fileRow("?", untracked.optString(i), false, false));
        }
        JSONArray conflicted = r.optJSONArray("conflicted");
        if (conflicted != null && conflicted.length() > 0) {
            body.addView(Ui.eyebrow(c, "Conflicted (resolve on the PC)"));
            for (int i = 0; i < conflicted.length(); i++) body.addView(Ui.text(c, "U  " + conflicted.optString(i), 14, Ui.ERROR, false));
        }
        if (r.optBoolean("truncated")) body.addView(Ui.text(c, "List shortened", 12, Ui.TEXT3, false));
        body.addView(Ui.eyebrow(c, "Commit"));
        final String[] source = Composer.pullSource(r), target = Composer.pushTarget(r);
        final String br = r.isNull("branch") ? "" : r.optString("branch");
        final EditText msg = new EditText(c);
        msg.setHint("Commit message");
        body.addView(msg);
        LinearLayout actions = Ui.row(c);
        actions.addView(Ui.button(c, "Commit staged", 0, new OnClickListener() {
            @Override
            public void onClick(View v) {
                String m = msg.getText().toString().trim();
                if (m.isEmpty()) { app.toast("Write a commit message first"); return; }
                change(App.msg("git_commit", "workspaceId", wsId, "message", m), "Committed");
            }
        }));
        actions.addView(Ui.gap(c, 8));
        Button pull = Ui.button(c, r.optInt("behind") > 0 ? "Pull " + r.optInt("behind") + " commit(s)" : "Pull", 1, new OnClickListener() {
            @Override
            public void onClick(View v) {
                confirm("Pull into " + br + "?", source[0],
                        "Fast-forward only: no merge commit, no rebase. If the branches have diverged or a local change would be overwritten, git refuses and nothing changes.",
                        "Pull", App.msg("git_pull", "workspaceId", wsId), "Pulled " + source[0]);
            }
        });
        pull.setEnabled(source[1] != null);
        pull.setAlpha(source[1] != null ? 1f : 0.35f);
        actions.addView(pull);
        actions.addView(Ui.gap(c, 8));
        Button push = Ui.button(c, r.optInt("ahead") > 0 ? "Push " + r.optInt("ahead") + " commit(s)" : "Push", 1, new OnClickListener() {
            @Override
            public void onClick(View v) {
                confirm("Push " + br + "?", target[0],
                        "Never forced. If the remote has commits you do not have, the push is rejected and nothing changes.",
                        "Push", App.msg("git_push", "workspaceId", wsId), "Pushed " + target[0]);
            }
        });
        push.setEnabled(target[1] != null);
        push.setAlpha(target[1] != null ? 1f : 0.35f);
        actions.addView(push);
        actions.addView(Ui.gap(c, 8));
        actions.addView(Ui.button(c, "Refresh", 2, new OnClickListener() {
            @Override public void onClick(View v) { loadStatus(); }
        }));
        body.addView(actions);
        body.addView(Ui.text(c, target[0], 12, Ui.TEXT3, false));
    }

    private void confirm(String title, String what, String note, String label, final JSONObject m, final String okText) {
        new AlertDialog.Builder(getContext()).setTitle(title).setMessage(what + "\n\n" + note)
                .setPositiveButton(label, new android.content.DialogInterface.OnClickListener() {
                    @Override public void onClick(android.content.DialogInterface d, int w) { change(m, okText); }
                }).setNegativeButton("Cancel", null).show();
    }

    private void section(String title, JSONArray files, boolean staged) {
        if (files == null || files.length() == 0) return;
        body.addView(Ui.eyebrow(getContext(), title));
        for (int i = 0; i < files.length(); i++) {
            JSONObject f = files.optJSONObject(i);
            if (f == null) continue;
            String label = f.has("origPath") ? f.optString("origPath") + " → " + f.optString("path") : f.optString("path");
            body.addView(fileRow(f.optString("status"), f.optString("path"), staged, true, label));
        }
    }

    private View fileRow(String letter, String path, boolean staged, boolean tracked) {
        return fileRow(letter, path, staged, tracked, path);
    }

    private View fileRow(String letter, final String file, final boolean staged, final boolean tracked, String label) {
        Context c = getContext();
        LinearLayout r = Ui.row(c);
        r.setPadding(0, Ui.dp(c, 4), 0, Ui.dp(c, 4));
        r.addView(Ui.text(c, letter, 14, Ui.TEXT2, true), Ui.lp(Ui.dp(c, 24), ViewGroup.LayoutParams.WRAP_CONTENT));
        TextView name = Ui.text(c, label, 14, Ui.TEXT, false);
        name.setSingleLine(true);
        name.setEllipsize(android.text.TextUtils.TruncateAt.MIDDLE);
        name.setOnClickListener(new OnClickListener() {
            @Override public void onClick(View v) { if (tracked) diff(file, staged); else read(file); }
        });
        r.addView(name, Ui.weight(1));
        r.addView(Ui.button(c, staged ? "Unstage" : "Stage", 1, new OnClickListener() {
            @Override
            public void onClick(View v) {
                JSONArray paths = new JSONArray().put(file);
                change(App.msg(staged ? "git_unstage" : "git_stage", "workspaceId", wsId, "paths", paths), null);
            }
        }));
        return r;
    }

    private void diff(final String file, boolean staged) {
        app.query(App.msg("git_diff", "workspaceId", wsId, "path", file, "staged", staged), new App.Result() {
            @Override
            public void done(JSONObject r, String err) {
                Context c = getContext();
                if (err != null) { app.toast(err); return; }
                ScrollView sv = new ScrollView(c);
                String patch = r.optString("patch");
                sv.addView(patch.isEmpty() ? Ui.text(c, "No textual changes", 14, Ui.TEXT2, false) : Ui.diff(c, null, patch));
                new AlertDialog.Builder(c).setTitle(file + (r.optBoolean("staged") ? " (staged)" : "")).setView(sv)
                        .setPositiveButton("Close", null).show();
            }
        });
    }

    private void change(JSONObject m, final String okText) {
        status.setText("Working…");
        app.change(m, new App.Result() {
            @Override
            public void done(JSONObject result, String err) {
                status.setText(result != null && err == null ? (okText == null ? "" : okText) : err == null ? "" : err);
                if (result != null && err != null) new AlertDialog.Builder(getContext()).setTitle("Git said").setMessage(err).setPositiveButton("OK", null).show();
                String warn = result != null && err == null ? Composer.agentConfigWarning(result.optJSONArray("agentConfigChanged")) : null;
                if (warn != null) new AlertDialog.Builder(getContext()).setTitle("Agent settings changed").setMessage(warn).setPositiveButton("OK", null).show();
                loadStatus();
            }
        });
    }
}
