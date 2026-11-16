package com.chernobyl.kitkat;

import android.app.AlertDialog;
import android.content.Context;
import android.content.DialogInterface;
import android.content.Intent;
import android.graphics.Canvas;
import android.graphics.Paint;
import android.graphics.Path;
import android.graphics.Typeface;
import android.graphics.drawable.GradientDrawable;
import android.net.Uri;
import android.os.SystemClock;
import android.provider.Settings;
import android.text.SpannableStringBuilder;
import android.text.Spanned;
import android.text.method.LinkMovementMethod;
import android.text.style.BackgroundColorSpan;
import android.text.style.ClickableSpan;
import android.text.style.ForegroundColorSpan;
import android.text.style.LeadingMarginSpan;
import android.text.style.RelativeSizeSpan;
import android.text.style.StyleSpan;
import android.text.style.TypefaceSpan;
import android.view.Gravity;
import android.view.View;
import android.view.ViewGroup;
import android.widget.Button;
import android.widget.EditText;
import android.widget.HorizontalScrollView;
import android.widget.LinearLayout;
import android.widget.TextView;
import java.util.ArrayList;
import java.util.List;
import org.json.JSONArray;
import org.json.JSONObject;

/** Plain-View building blocks in the main app's look (apps/mobile/DESIGN.md tokens). */
final class Ui {
    private Ui() {}

    static final int BLACK = 0xFF050505, WHITE = 0xFFFFFFFF, SURFACE2 = 0xFFF3F3F3, TEXT = 0xFF111111, TEXT2 = 0xFF666666,
            TEXT3 = 0xFF8B8B8B, BORDER = 0xFFD2D2D2, SUCCESS = 0xFF2F7D4F, SUCCESS_BG = 0xFFEAF4EE, WARNING = 0xFF9A6B00,
            WARNING_BG = 0xFFFBF1DC, ERROR = 0xFFA32B2B, ERROR_BG = 0xFFFBECEB;

    static int dp(Context c, float v) {
        return Math.round(v * c.getResources().getDisplayMetrics().density);
    }

    static TextView text(Context c, CharSequence s, float sp, int color, boolean bold) {
        TextView t = new TextView(c);
        t.setText(s);
        t.setTextSize(sp);
        t.setTextColor(color);
        if (bold) t.setTypeface(Typeface.DEFAULT_BOLD);
        return t;
    }

    static TextView mono(Context c, CharSequence s, int fg, int bg) {
        TextView t = text(c, s, 13, fg, false);
        t.setTypeface(Typeface.MONOSPACE);
        t.setTextIsSelectable(true);
        if (bg != 0) {
            t.setBackgroundDrawable(box(bg, 0, 8, c));
            int p = dp(c, 10);
            t.setPadding(p, p, p, p);
        }
        return t;
    }

    static TextView eyebrow(Context c, String s) {
        TextView t = text(c, s.toUpperCase(), 11, TEXT3, true);
        t.setPadding(0, dp(c, 16), 0, dp(c, 6));
        return t;
    }

    static GradientDrawable box(int fill, int stroke, float radiusDp, Context c) {
        GradientDrawable g = new GradientDrawable();
        g.setColor(fill);
        if (stroke != 0) g.setStroke(Math.max(1, dp(c, 1)), stroke);
        g.setCornerRadius(dp(c, radiusDp));
        return g;
    }

    /** primary = black fill; secondary = outlined; ghost = text only. */
    static Button button(Context c, String label, int kind, View.OnClickListener l) {
        Button b = new Button(c);
        b.setText(label);
        b.setAllCaps(false);
        b.setTextSize(14);
        b.setMinHeight(dp(c, 44));
        b.setMinimumHeight(dp(c, 44));
        int p = dp(c, 16);
        b.setPadding(p, 0, p, 0);
        if (kind == 0) { b.setTextColor(WHITE); b.setBackgroundDrawable(box(BLACK, 0, 22, c)); }
        else if (kind == 1) { b.setTextColor(TEXT); b.setBackgroundDrawable(box(WHITE, BLACK, 22, c)); }
        else { b.setTextColor(TEXT); b.setBackgroundDrawable(box(0x00000000, 0, 22, c)); }
        b.setOnClickListener(l);
        return b;
    }

    static TextView pill(Context c, String label, int fg, int bg) {
        TextView t = text(c, label, 12, fg, true);
        t.setBackgroundDrawable(box(bg, bg == WHITE ? BORDER : 0, 999, c));
        t.setPadding(dp(c, 10), dp(c, 3), dp(c, 10), dp(c, 3));
        t.setGravity(Gravity.CENTER);
        return t;
    }

    static TextView statusPill(Context c, String status) {
        switch (status) {
            case "running": return pill(c, "Running", WHITE, BLACK);
            case "waiting": return pill(c, "Waiting", WARNING, WARNING_BG);
            case "completed": return pill(c, "Completed", SUCCESS, SUCCESS_BG);
            case "interrupted": return pill(c, "Interrupted", ERROR, ERROR_BG);
            case "starting": return pill(c, "Starting", TEXT2, SURFACE2);
            case "ended": return pill(c, "Ended", TEXT2, SURFACE2);
            default: return pill(c, status.isEmpty() ? "Idle" : capital(status), TEXT2, SURFACE2);
        }
    }

    /** ADR-009: demo data is always labelled. */
    static TextView simulated(Context c) {
        return pill(c, "Simulated", TEXT2, SURFACE2);
    }

    /** ADR-018: a session a terminal or VS Code on the PC is writing right now. */
    static TextView livePc(Context c, boolean inverse) {
        SpannableStringBuilder sb = new SpannableStringBuilder("● Live on PC");
        sb.setSpan(new ForegroundColorSpan(0xFF3FA66A), 0, 1, Spanned.SPAN_EXCLUSIVE_EXCLUSIVE);
        TextView t = text(c, sb, 12, inverse ? WHITE : TEXT, true);
        t.setBackgroundDrawable(box(inverse ? BLACK : WHITE, inverse ? TEXT2 : BLACK, 999, c));
        t.setPadding(dp(c, 10), dp(c, 3), dp(c, 10), dp(c, 3));
        return t;
    }

    /** A tappable pill on the black session header (null listener = read-only). */
    static TextView chip(Context c, CharSequence label, View.OnClickListener l) {
        TextView t = text(c, label, 13, WHITE, true);
        t.setBackgroundDrawable(box(BLACK, TEXT2, 999, c));
        t.setPadding(dp(c, 12), dp(c, 7), dp(c, 12), dp(c, 7));
        t.setSingleLine(true);
        if (l != null) t.setOnClickListener(l);
        LinearLayout.LayoutParams lp = new LinearLayout.LayoutParams(ViewGroup.LayoutParams.WRAP_CONTENT, ViewGroup.LayoutParams.WRAP_CONTENT);
        lp.rightMargin = dp(c, 8);
        t.setLayoutParams(lp);
        return t;
    }

    /** A titled sheet (dialog) around `body`, scrollable; the caller adds rows that dismiss it. */
    static AlertDialog sheet(Context c, String title, View body) {
        android.widget.ScrollView sv = new android.widget.ScrollView(c);
        int p = dp(c, 16);
        body.setPadding(p, dp(c, 8), p, p);
        sv.addView(body);
        return new AlertDialog.Builder(c).setTitle(title).setView(sv).setNegativeButton("Close", null).create();
    }

    /** One radio-like row of a sheet: bold title, caption, a check when on. */
    static View choiceRow(Context c, String title, String caption, boolean on, View.OnClickListener l) {
        LinearLayout r = row(c);
        r.setBackgroundDrawable(box(on ? SURFACE2 : WHITE, on ? BLACK : BORDER, 12, c));
        int p = dp(c, 12);
        r.setPadding(p, p, p, p);
        LinearLayout t = col(c);
        t.addView(text(c, title, 15, TEXT, true));
        if (caption != null) t.addView(text(c, caption, 12, TEXT2, false));
        r.addView(t, weight(1));
        if (on) r.addView(text(c, "✓", 18, TEXT, true));
        r.setOnClickListener(l);
        addMargin(r, 0, 0, 0, 8);
        return r;
    }

    static String capital(String s) {
        return s.isEmpty() ? s : Character.toUpperCase(s.charAt(0)) + s.substring(1);
    }

    static LinearLayout row(Context c) {
        LinearLayout l = new LinearLayout(c);
        l.setOrientation(LinearLayout.HORIZONTAL);
        l.setGravity(Gravity.CENTER_VERTICAL);
        return l;
    }

    static LinearLayout col(Context c) {
        LinearLayout l = new LinearLayout(c);
        l.setOrientation(LinearLayout.VERTICAL);
        return l;
    }

    static LinearLayout card(Context c, boolean strong) {
        LinearLayout l = col(c);
        l.setBackgroundDrawable(box(WHITE, strong ? BLACK : BORDER, 16, c));
        int p = dp(c, 14);
        l.setPadding(p, p, p, p);
        return l;
    }

    static LinearLayout.LayoutParams lp(int w, int h) {
        return new LinearLayout.LayoutParams(w, h);
    }

    static LinearLayout.LayoutParams weight(float w) {
        return new LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, w);
    }

    static View gap(Context c, int dpSize) {
        View v = new View(c);
        v.setLayoutParams(new LinearLayout.LayoutParams(dp(c, dpSize), dp(c, dpSize)));
        return v;
    }

    static void addMargin(View v, int l, int t, int r, int b) {
        Context c = v.getContext();
        LinearLayout.LayoutParams p = v.getLayoutParams() instanceof LinearLayout.LayoutParams
                ? (LinearLayout.LayoutParams) v.getLayoutParams()
                : new LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT);
        p.setMargins(dp(c, l), dp(c, t), dp(c, r), dp(c, b));
        v.setLayoutParams(p);
    }

    static String ago(long ts) {
        long s = Math.max(0, (System.currentTimeMillis() - ts) / 1000);
        if (s < 60) return s + "s ago";
        if (s < 3600) return (s / 60) + "m ago";
        if (s < 86400) return (s / 3600) + "h ago";
        return (s / 86400) + "d ago";
    }

    /* ---------------- agent marks (agentMark.tsx sprites, original 8x8 characters) ---------------- */

    static final String[] CLAUDE = {"...##...", "..####..", ".######.", "##.##.##", "########", ".######.", "..####..", "...##..."};
    static final String[] CODEX = {"...##...", ".######.", ".#.##.#.", ".######.", ".##..##.", ".######.", "..#..#..", ".##..##."};
    static final String[] OPENCODE = {".##..##.", ".#....#.", ".#....#.", "#..##..#", "#..##..#", ".#....#.", ".#....#.", ".##..##."};
    static final String[] ANTIGRAVITY = {"..####..", ".#.##.#.", "########", "#.#..#.#", "########", "..#..#..", ".#....#.", "#......#"};
    static final String[] GENERIC = {"..####..", ".######.", "##.##.##", "########", "########", "########", ".######.", ".#.##.#."};

    static String[] sprite(String kind) {
        if ("claude-code".equals(kind)) return CLAUDE;
        if ("codex".equals(kind)) return CODEX;
        if ("opencode".equals(kind)) return OPENCODE;
        if ("antigravity".equals(kind)) return ANTIGRAVITY;
        return GENERIC;
    }

    static int agentColor(String kind) {
        if ("claude-code".equals(kind)) return 0xFFD97757; // terracotta
        if ("codex".equals(kind)) return 0xFF0F8A7E; // teal
        if ("opencode".equals(kind)) return 0xFF5B5FC7; // indigo
        if ("antigravity".equals(kind)) return 0xFF8B5CF6; // violet
        return 0xFF666666;
    }

    /** The agent's mark; `state` idle | working (bob + sparkle) | waiting (fade pulse) | error (dim). */
    static final class Mark extends View {
        private final String[] rows;
        private final Paint paint = new Paint(Paint.ANTI_ALIAS_FLAG);
        private String state = "idle";
        private final Path spark = new Path();

        Mark(Context c, String kind, int sizeDp) {
            super(c);
            rows = sprite(kind);
            paint.setColor(agentColor(kind));
            int s = dp(c, sizeDp);
            setLayoutParams(new LinearLayout.LayoutParams(s + s / 3, s + s / 4));
            setContentDescription(null);
            setImportantForAccessibility(IMPORTANT_FOR_ACCESSIBILITY_NO);
        }

        void setState(String s) {
            if (s.equals(state)) return;
            state = s;
            invalidate();
        }

        @Override
        protected void onDraw(Canvas cv) {
            float h = getHeight() * 0.8f, cell = h / 8f, top = getHeight() - h;
            long t = SystemClock.uptimeMillis();
            float phase = 0;
            int period = "working".equals(state) ? 800 : "thinking".equals(state) ? 1600 : "waiting".equals(state) ? 2400 : 0;
            // "Remove animations" (animator scale 0): a still mark, no 20 fps redraw while an approval waits for hours
            if (period > 0 && Settings.Global.getFloat(getContext().getContentResolver(), Settings.Global.ANIMATOR_DURATION_SCALE, 1f) == 0f) period = 0;
            if (period > 0) phase = (float) (0.5 - 0.5 * Math.cos(2 * Math.PI * (t % period) / period));
            int alpha = "error".equals(state) ? 115 : "waiting".equals(state) ? (int) (255 * (1 - 0.6f * phase)) : 255;
            paint.setAlpha(alpha);
            float dy = "working".equals(state) || "thinking".equals(state) ? -phase * cell : 0;
            for (int y = 0; y < 8; y++)
                for (int x = 0; x < 8; x++)
                    if (rows[y].charAt(x) == '#') cv.drawRect(x * cell, top + y * cell + dy, (x + 1) * cell, top + (y + 1) * cell + dy, paint);
            if ("working".equals(state) || "thinking".equals(state)) {
                float s = h * 0.375f, ox = 8 * cell - s / 3, oy = 0;
                float u = s / 8f;
                spark.reset();
                spark.moveTo(ox + 4 * u, oy);
                spark.lineTo(ox + 5 * u, oy + 3 * u);
                spark.lineTo(ox + 8 * u, oy + 4 * u);
                spark.lineTo(ox + 5 * u, oy + 5 * u);
                spark.lineTo(ox + 4 * u, oy + 8 * u);
                spark.lineTo(ox + 3 * u, oy + 5 * u);
                spark.lineTo(ox, oy + 4 * u);
                spark.lineTo(ox + 3 * u, oy + 3 * u);
                spark.close();
                paint.setAlpha((int) (255 * phase));
                cv.drawPath(spark, paint);
            }
            if (period > 0) postInvalidateDelayed(50);
        }
    }

    /* ---------------- markdown → spans ---------------- */

    static void confirmOpen(final Context c, final String url) {
        if (!Md.safeUrl(url)) return;
        new AlertDialog.Builder(c).setTitle("Open link?").setMessage(url)
                .setPositiveButton("Open", new DialogInterface.OnClickListener() {
                    @Override
                    public void onClick(DialogInterface d, int w) {
                        try {
                            c.startActivity(new Intent(Intent.ACTION_VIEW, Uri.parse(url)));
                        } catch (Exception e) {
                            App.app.toast("No app can open this link");
                        }
                    }
                }).setNegativeButton("Cancel", null).show();
    }

    static void runs(final Context c, SpannableStringBuilder sb, List<Md.Run> runs) {
        for (final Md.Run r : runs) {
            int start = sb.length();
            sb.append(r.text);
            int end = sb.length();
            if (r.bold && r.italic) sb.setSpan(new StyleSpan(Typeface.BOLD_ITALIC), start, end, Spanned.SPAN_EXCLUSIVE_EXCLUSIVE);
            else if (r.bold) sb.setSpan(new StyleSpan(Typeface.BOLD), start, end, Spanned.SPAN_EXCLUSIVE_EXCLUSIVE);
            else if (r.italic) sb.setSpan(new StyleSpan(Typeface.ITALIC), start, end, Spanned.SPAN_EXCLUSIVE_EXCLUSIVE);
            if (r.code) {
                sb.setSpan(new TypefaceSpan("monospace"), start, end, Spanned.SPAN_EXCLUSIVE_EXCLUSIVE);
                sb.setSpan(new BackgroundColorSpan(SURFACE2), start, end, Spanned.SPAN_EXCLUSIVE_EXCLUSIVE);
            }
            if (r.url != null) {
                sb.setSpan(new ClickableSpan() {
                    @Override
                    public void onClick(View w) {
                        confirmOpen(c, r.url);
                    }
                }, start, end, Spanned.SPAN_EXCLUSIVE_EXCLUSIVE);
            }
        }
    }

    /** Markdown as Views: text blocks become one TextView each, fenced code a black mono block. */
    static View markdown(Context c, String src) {
        LinearLayout box = col(c);
        SpannableStringBuilder sb = new SpannableStringBuilder();
        for (Md.Block b : Md.parse(src)) {
            if (b.type == Md.CODE || b.type == Md.TABLE) {
                flushText(c, box, sb);
                HorizontalScrollView hs = new HorizontalScrollView(c);
                TextView code = b.type == Md.CODE ? mono(c, b.code + " ", WHITE, BLACK) : mono(c, b.code + " ", TEXT, SURFACE2);
                hs.addView(code);
                addMargin(hs, 0, 4, 0, 6);
                box.addView(hs);
                continue;
            }
            if (sb.length() > 0) sb.append("\n");
            int start = sb.length();
            if (b.type == Md.RULE) {
                sb.append("────────");
                sb.setSpan(new ForegroundColorSpan(BORDER), start, sb.length(), Spanned.SPAN_EXCLUSIVE_EXCLUSIVE);
                continue;
            }
            if (b.type == Md.BULLET) sb.append("•  ");
            else if (b.type == Md.NUMBERED) sb.append(b.level + ".  ");
            else if (b.type == Md.QUOTE) sb.append("│ ");
            runs(c, sb, b.runs);
            if (b.type == Md.HEADING) {
                sb.setSpan(new StyleSpan(Typeface.BOLD), start, sb.length(), Spanned.SPAN_EXCLUSIVE_EXCLUSIVE);
                sb.setSpan(new RelativeSizeSpan(b.level <= 1 ? 1.3f : b.level == 2 ? 1.15f : 1.05f), start, sb.length(), Spanned.SPAN_EXCLUSIVE_EXCLUSIVE);
            }
            if (b.type == Md.BULLET || b.type == Md.NUMBERED) sb.setSpan(new LeadingMarginSpan.Standard(dp(c, 6), dp(c, 20)), start, sb.length(), Spanned.SPAN_EXCLUSIVE_EXCLUSIVE);
            if (b.type == Md.QUOTE) sb.setSpan(new ForegroundColorSpan(TEXT2), start, sb.length(), Spanned.SPAN_EXCLUSIVE_EXCLUSIVE);
            sb.append("\n");
        }
        flushText(c, box, sb);
        return box;
    }

    private static void flushText(Context c, LinearLayout box, SpannableStringBuilder sb) {
        while (sb.length() > 0 && sb.charAt(sb.length() - 1) == '\n') sb.delete(sb.length() - 1, sb.length());
        if (sb.length() == 0) return;
        TextView t = text(c, new SpannableStringBuilder(sb), 15, TEXT, false);
        t.setLineSpacing(0, 1.15f);
        t.setMovementMethod(LinkMovementMethod.getInstance());
        t.setFocusable(false);
        box.addView(t);
        sb.clear();
        sb.clearSpans();
    }

    /* ---------------- diffs ---------------- */

    static View diff(final Context c, String path, String patch) {
        final LinearLayout box = col(c);
        if (path != null) box.addView(text(c, path, 13, TEXT, true));
        final List<Md.DiffLine> all = Md.diff(patch, Integer.MAX_VALUE);
        final LinearLayout lines = col(c);
        HorizontalScrollView hs = new HorizontalScrollView(c);
        hs.addView(lines);
        box.addView(hs);
        final int[] shown = {0};
        final Button more = button(c, "Show more", 2, null);
        final Runnable fill = new Runnable() {
            @Override
            public void run() {
                SpannableStringBuilder sb = new SpannableStringBuilder();
                int end = Math.min(all.size(), shown[0] + 200);
                for (int i = shown[0]; i < end; i++) {
                    Md.DiffLine d = all.get(i);
                    int s = sb.length();
                    sb.append(d.text).append(" \n");
                    int fg = d.kind == Md.ADD ? SUCCESS : d.kind == Md.DEL ? ERROR : d.kind == Md.CTX ? TEXT : TEXT3;
                    sb.setSpan(new ForegroundColorSpan(fg), s, sb.length(), Spanned.SPAN_EXCLUSIVE_EXCLUSIVE);
                    if (d.kind == Md.ADD || d.kind == Md.DEL)
                        sb.setSpan(new BackgroundColorSpan(d.kind == Md.ADD ? SUCCESS_BG : ERROR_BG), s, sb.length() - 1, Spanned.SPAN_EXCLUSIVE_EXCLUSIVE);
                }
                if (sb.length() > 0) sb.delete(sb.length() - 1, sb.length());
                TextView t = mono(c, sb, TEXT, 0);
                t.setTextSize(12);
                lines.addView(t);
                shown[0] = end;
                more.setVisibility(shown[0] < all.size() ? View.VISIBLE : View.GONE);
                more.setText("Show more (" + (all.size() - shown[0]) + " lines)");
            }
        };
        more.setOnClickListener(new View.OnClickListener() {
            @Override
            public void onClick(View v) {
                fill.run();
            }
        });
        fill.run();
        box.addView(more);
        box.setBackgroundDrawable(box(WHITE, BORDER, 8, c));
        int p = dp(c, 8);
        box.setPadding(p, p, p, p);
        return box;
    }

    /* ---------------- approvals (InlineApproval / StandaloneApproval / inbox) ---------------- */

    static String detailText(JSONObject p) {
        JSONObject d = p.optJSONObject("detail");
        if (d == null) return p.optString("command");
        switch (d.optString("kind")) {
            case "shell": return d.optString("command") + (d.has("cwd") ? "\n(in " + d.optString("cwd") + ")" : "");
            case "edit":
            case "read": return d.optString("path");
            case "fetch": return d.optString("url");
            case "mcp": return d.optString("server") + " / " + d.optString("tool") + (d.has("input") ? "\n" + d.optString("input") : "");
            case "plan": return d.optString("plan");
            case "generic": return d.optString("tool") + (d.has("input") ? "\n" + d.optString("input") : "");
            case "question": {
                StringBuilder s = new StringBuilder();
                JSONArray qs = d.optJSONArray("questions");
                for (int i = 0; qs != null && i < qs.length(); i++) s.append(i > 0 ? "\n" : "").append(qs.optJSONObject(i).optString("text"));
                return s.toString();
            }
            default: return p.optString("command");
        }
    }

    /** The request with its runtime's own choices; `compact` hides the context line. */
    static View approval(final Context c, final JSONObject p, boolean compact) {
        final App app = App.app;
        LinearLayout box = card(c, true);
        JSONObject agent = app.model.agent(p.optString("agentId"));
        LinearLayout head = row(c);
        if (agent != null) {
            head.addView(new Mark(c, agent.optString("kind"), 16));
            head.addView(gap(c, 6));
        }
        TextView title = text(c, p.optString("title", "Approval needed"), 15, TEXT, true);
        head.addView(title, weight(1));
        String risk = p.optString("risk");
        head.addView(pill(c, Ui.capital(risk) + " risk", risk.equals("high") ? ERROR : risk.equals("medium") ? WARNING : TEXT2,
                risk.equals("high") ? ERROR_BG : risk.equals("medium") ? WARNING_BG : SURFACE2));
        box.addView(head);
        // ADR-018: asked by a Claude Code session in a terminal or VS Code on the PC (Allow / Deny only)
        if (p.optBoolean("fromPc")) box.addView(text(c, "Approval needed · from a PC session", 12, TEXT2, true));
        JSONObject d = p.optJSONObject("detail");
        if (d != null && "edit".equals(d.optString("kind")) && d.has("patch")) {
            View df = diff(c, d.optString("path"), d.optString("patch"));
            addMargin(df, 0, 8, 0, 0);
            box.addView(df);
        } else {
            TextView cmd = mono(c, detailText(p), WHITE, BLACK);
            HorizontalScrollView hs = new HorizontalScrollView(c);
            hs.addView(cmd);
            addMargin(hs, 0, 8, 0, 0);
            box.addView(hs);
        }
        if (!compact && !p.optString("context").isEmpty()) {
            TextView ctx = text(c, p.optString("context"), 13, TEXT2, false);
            ctx.setPadding(0, dp(c, 6), 0, 0);
            box.addView(ctx);
        }
        if (agent != null && agent.optBoolean("simulated")) {
            LinearLayout tag = row(c);
            tag.setPadding(0, dp(c, 6), 0, 0);
            tag.addView(simulated(c));
            box.addView(tag);
        }
        LinearLayout buttons = row(c);
        buttons.setPadding(0, dp(c, 10), 0, 0);
        final List<JSONObject> options = new ArrayList<JSONObject>();
        JSONArray opts = p.optJSONArray("options");
        if (opts != null) for (int i = 0; i < opts.length(); i++) if (opts.optJSONObject(i) != null) options.add(opts.optJSONObject(i));
        if (options.isEmpty()) {
            // legacy: the decisions the runtime supports, never assumed (G2)
            JSONArray sd = p.optJSONArray("supportedDecisions");
            for (int i = 0; sd != null && i < sd.length(); i++) {
                final String dec = sd.optString(i);
                String label = dec.equals("approve-once") ? "Allow once" : dec.equals("approve-session") ? "Allow for session" : "Deny";
                Button b = button(c, label, dec.equals("approve-once") ? 0 : 1, new View.OnClickListener() {
                    @Override
                    public void onClick(View v) {
                        if (dec.equals("reject")) askReason(c, p, null);
                        else decide(p, dec, null, null, null);
                    }
                });
                buttons.addView(b);
                buttons.addView(gap(c, 8));
            }
        } else {
            JSONObject allow = null, deny = null;
            for (JSONObject o : options) {
                if (allow == null && o.optString("decision").startsWith("approve") && "once".equals(o.optString("scope"))) allow = o;
                if (deny == null && "reject".equals(o.optString("decision"))) deny = o;
            }
            if (allow != null) {
                final JSONObject a = allow;
                buttons.addView(button(c, a.optString("label", "Allow"), 0, new View.OnClickListener() {
                    @Override public void onClick(View v) { choose(c, p, a); }
                }));
                buttons.addView(gap(c, 8));
            }
            if (deny != null) {
                final JSONObject dn = deny;
                buttons.addView(button(c, dn.optString("label", "Deny"), 1, new View.OnClickListener() {
                    @Override public void onClick(View v) { choose(c, p, dn); }
                }));
                buttons.addView(gap(c, 8));
            }
            buttons.addView(button(c, "More…", 2, new View.OnClickListener() {
                @Override
                public void onClick(View v) {
                    String[] labels = new String[options.size()];
                    for (int i = 0; i < labels.length; i++) {
                        JSONObject o = options.get(i);
                        labels[i] = o.optString("label") + (o.has("rule") ? "\n" + o.optString("rule") : "")
                                + ("persistent".equals(o.optString("scope")) ? "  (saved)" : "session".equals(o.optString("scope")) ? "  (this session)" : "");
                    }
                    new AlertDialog.Builder(c).setTitle(p.optString("title")).setItems(labels, new DialogInterface.OnClickListener() {
                        @Override public void onClick(DialogInterface dlg, int which) { choose(c, p, options.get(which)); }
                    }).setNegativeButton("Cancel", null).show();
                }
            }));
        }
        box.addView(buttons);
        return box;
    }

    /** One runtime option: questions get answered, persistent grants confirmed, reasons asked. */
    static void choose(final Context c, final JSONObject p, final JSONObject o) {
        JSONObject d = p.optJSONObject("detail");
        boolean approve = o.optString("decision").startsWith("approve");
        if (approve && d != null && "question".equals(d.optString("kind"))) {
            answerQuestions(c, p, o, d.optJSONArray("questions"));
            return;
        }
        if ("persistent".equals(o.optString("scope"))) {
            new AlertDialog.Builder(c).setTitle("Save this rule?")
                    .setMessage(o.optString("label") + (o.has("rule") ? "\n\nRule: " + o.optString("rule") : "")
                            + (o.has("destination") ? "\nWritten to: " + o.optString("destination") : ""))
                    .setPositiveButton("Save rule", new DialogInterface.OnClickListener() {
                        @Override public void onClick(DialogInterface dlg, int w) { decide(p, o.optString("decision"), o.optString("id"), null, null); }
                    }).setNegativeButton("Cancel", null).show();
            return;
        }
        if (o.optBoolean("takesMessage")) {
            askReason(c, p, o);
            return;
        }
        decide(p, o.optString("decision"), o.optString("id"), null, null);
    }

    static void askReason(final Context c, final JSONObject p, final JSONObject o) {
        final EditText reason = new EditText(c);
        reason.setHint("Optional: tell the agent why");
        JSONObject agent = App.app.model.agent(p.optString("agentId"));
        new AlertDialog.Builder(c).setTitle("Deny" + (agent != null ? " and tell " + agent.optString("name") + " why" : "")).setView(reason)
                .setPositiveButton("Deny", new DialogInterface.OnClickListener() {
                    @Override
                    public void onClick(DialogInterface dlg, int w) {
                        String r = reason.getText().toString().trim();
                        decide(p, "reject", o == null ? null : o.optString("id"), r.isEmpty() ? null : r, null);
                    }
                }).setNegativeButton("Cancel", null).show();
    }

    static void answerQuestions(final Context c, final JSONObject p, final JSONObject o, final JSONArray qs) {
        final JSONObject answers = new JSONObject();
        final int[] i = {0};
        final Runnable[] next = new Runnable[1];
        next[0] = new Runnable() {
            @Override
            public void run() {
                if (qs == null || i[0] >= qs.length()) {
                    decide(p, o.optString("decision"), o.optString("id"), null, answers);
                    return;
                }
                final JSONObject q = qs.optJSONObject(i[0]++);
                JSONArray qo = q.optJSONArray("options");
                final String[] labels = new String[qo == null ? 0 : qo.length()];
                for (int k = 0; k < labels.length; k++) labels[k] = qo.optJSONObject(k).optString("label");
                final boolean[] picked = new boolean[labels.length];
                final EditText free = new EditText(c);
                free.setHint("Or type an answer");
                AlertDialog.Builder b = new AlertDialog.Builder(c).setTitle(q.optString("header", q.optString("text"))).setMessage(null);
                if (q.optBoolean("allowFreeText")) b.setView(free);
                b.setMultiChoiceItems(labels, picked, new DialogInterface.OnMultiChoiceClickListener() {
                    @Override public void onClick(DialogInterface d, int which, boolean on) { picked[which] = on; }
                });
                b.setPositiveButton("Next", new DialogInterface.OnClickListener() {
                    @Override
                    public void onClick(DialogInterface d, int w) {
                        JSONArray a = new JSONArray();
                        String typed = free.getText().toString().trim();
                        if (!typed.isEmpty()) a.put(typed);
                        else for (int k = 0; k < labels.length; k++) if (picked[k]) {
                            a.put(labels[k]);
                            if (!q.optBoolean("multiSelect")) break;
                        }
                        try { answers.put(q.optString("id"), a); } catch (Exception ignored) { }
                        next[0].run();
                    }
                }).setNegativeButton("Cancel", null).show();
            }
        };
        next[0].run();
    }

    static void decide(JSONObject p, String decision, String optionId, String message, JSONObject answers) {
        JSONObject m = App.msg("permission_decision", "permissionId", p.optString("id"), "decision", decision,
                "revision", p.optInt("revision"), "optionId", optionId, "message", message, "answers", answers);
        App.app.command(m, null, null);
    }

    static String ellipsize(String s, int max) {
        if (s == null) return "";
        s = s.replace('\n', ' ');
        return s.length() <= max ? s : s.substring(0, max - 1) + "…";
    }

    /** Long tool input/output: the head, then how much the card leaves out. */
    static String ellipsizeKeep(String s, int max) {
        return s.length() <= max ? s : s.substring(0, max) + "\n… (" +(s.length() - max) + " more characters)";
    }
}
