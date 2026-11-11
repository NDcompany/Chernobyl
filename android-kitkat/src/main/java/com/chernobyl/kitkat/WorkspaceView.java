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
 * diffs, stage / unstage, commit, push. Reads show their errors inline; changes
 * resolve with their command_result (git's own summary on failure). The
 * protocol has no pull and no terminal yet, so neither is offered.
 */
final class WorkspaceView extends LinearLayout {
    private final App app = App.app;
    private final LinearLayout body;
    private final TextView status;
    private List<JSONObject> workspaces;
    private String wsId;
    private boolean changesTab;
    private String path = "";

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
        Spinner pick = new Spinner(c);
        String[] labels = new String[workspaces.size()];
        for (int i = 0; i < labels.length; i++) labels[i] = workspaces.get(i).optString("label") + "  —  " + workspaces.get(i).optString("path");
        ArrayAdapter<String> ad = new ArrayAdapter<String>(c, android.R.layout.simple_spinner_item, labels);
        ad.setDropDownViewResource(android.R.layout.simple_spinner_dropdown_item);
        pick.setAdapter(ad);
        pick.setOnItemSelectedListener(new AdapterView.OnItemSelectedListener() {
            @Override
            public void onItemSelected(AdapterView<?> parent, View v, int pos, long id) {
                wsId = workspaces.get(pos).optString("id");
                path = "";
                load();
            }

            @Override
            public void onNothingSelected(AdapterView<?> parent) { }
        });
        bar.addView(pick, Ui.weight(1));
        bar.addView(Ui.button(c, "Files", 1, new OnClickListener() {
            @Override public void onClick(View v) { changesTab = false; load(); }
        }));
        bar.addView(Ui.gap(c, 8));
        bar.addView(Ui.button(c, "Changes", 1, new OnClickListener() {
            @Override public void onClick(View v) { changesTab = true; load(); }
        }));
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
        if (wsId == null) return;
        if (changesTab) loadStatus();
        else list(path);
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
        actions.addView(Ui.button(c, "Push", 1, new OnClickListener() {
            @Override public void onClick(View v) { change(App.msg("git_push", "workspaceId", wsId), "Pushed"); }
        }));
        actions.addView(Ui.gap(c, 8));
        actions.addView(Ui.button(c, "Refresh", 2, new OnClickListener() {
            @Override public void onClick(View v) { loadStatus(); }
        }));
        body.addView(actions);
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
                loadStatus();
            }
        });
    }
}
