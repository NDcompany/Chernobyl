package com.chernobyl.kitkat;

import java.util.ArrayList;
import java.util.List;
import java.util.regex.Matcher;
import java.util.regex.Pattern;

/**
 * The markdown subset the conversation renders (paragraphs, headings, fenced
 * code, lists, rules, pipe tables as aligned monospace text, bold, italic,
 * inline code, links) and unified-diff line
 * classes. Pure Java, unit-tested; Ui turns it into spans. No HTML, no images,
 * no remote loads. Streaming-safe: an unclosed fence runs to the end, an
 * unclosed marker shows as typed.
 */
public final class Md {
    private Md() {}

    public static final int PARA = 0, CODE = 1, HEADING = 2, BULLET = 3, NUMBERED = 4, RULE = 5, QUOTE = 6, TABLE = 7;

    public static final class Run {
        public final String text;
        public final boolean bold, italic, code;
        /** Non-null for a link; opened only after a confirm. */
        public final String url;

        Run(String text, boolean bold, boolean italic, boolean code, String url) {
            this.text = text;
            this.bold = bold;
            this.italic = italic;
            this.code = code;
            // only a safe URL becomes a link; anything else stays the plain text it was
            this.url = safeUrl(url) ? url : null;
        }

        @Override
        public String toString() {
            return (bold ? "B" : "") + (italic ? "I" : "") + (code ? "C" : "") + (url != null ? "L<" + url + ">" : "") + "[" + text + "]";
        }
    }

    public static final class Block {
        public final int type;
        /** Heading level, list number. */
        public final int level;
        /** Code blocks: the raw text; others: inline runs. */
        public final String code;
        public final List<Run> runs;

        Block(int type, int level, String code, List<Run> runs) {
            this.type = type;
            this.level = level;
            this.code = code;
            this.runs = runs;
        }
    }

    private static final Pattern HEADING_RE = Pattern.compile("^(#{1,6})\\s+(.*)$");
    private static final Pattern BULLET_RE = Pattern.compile("^\\s*[-*+]\\s+(.*)$");
    private static final Pattern NUM_RE = Pattern.compile("^\\s*(\\d{1,9})[.)]\\s+(.*)$");
    private static final Pattern RULE_RE = Pattern.compile("^\\s*([-*_])(\\s*\\1){2,}\\s*$");
    private static final Pattern TABLE_SEP = Pattern.compile("^\\|?\\s*:?-+:?\\s*(\\|\\s*:?-+:?\\s*)*\\|?$");

    public static List<Block> parse(String src) {
        List<Block> out = new ArrayList<Block>();
        String[] lines = src.replace("\r\n", "\n").split("\n", -1);
        StringBuilder para = null;
        for (int i = 0; i < lines.length; i++) {
            String line = lines[i];
            String t = line.trim();
            if (t.startsWith("```")) {
                if (para != null) { out.add(new Block(PARA, 0, null, inline(para.toString()))); para = null; }
                StringBuilder code = new StringBuilder();
                int j = i + 1;
                for (; j < lines.length && !lines[j].trim().startsWith("```"); j++) {
                    if (code.length() > 0) code.append('\n');
                    code.append(lines[j]);
                }
                out.add(new Block(CODE, 0, code.toString(), null));
                i = j; // past the closing fence (or the end while streaming)
                continue;
            }
            if (t.startsWith("|") && i + 1 < lines.length && TABLE_SEP.matcher(lines[i + 1].trim()).matches()) {
                if (para != null) { out.add(new Block(PARA, 0, null, inline(para.toString()))); para = null; }
                List<String> rows = new ArrayList<String>();
                rows.add(t);
                int j = i + 2;
                for (; j < lines.length && lines[j].trim().startsWith("|"); j++) rows.add(lines[j].trim());
                out.add(new Block(TABLE, 0, table(rows), null));
                i = j - 1;
                continue;
            }
            Matcher m;
            int type = -1, level = 0;
            String body = null;
            if (t.isEmpty()) {
                type = -2;
            } else if (RULE_RE.matcher(line).matches()) {
                type = RULE;
            } else if ((m = HEADING_RE.matcher(t)).matches()) {
                type = HEADING; level = m.group(1).length(); body = m.group(2);
            } else if ((m = BULLET_RE.matcher(line)).matches()) {
                type = BULLET; body = m.group(1);
            } else if ((m = NUM_RE.matcher(line)).matches()) {
                type = NUMBERED; level = Integer.parseInt(m.group(1)); body = m.group(2);
            } else if (t.startsWith(">")) {
                type = QUOTE; body = t.substring(1).trim();
            }
            if (type == -1) {
                if (para == null) para = new StringBuilder(t);
                else para.append(' ').append(t);
                continue;
            }
            if (para != null) { out.add(new Block(PARA, 0, null, inline(para.toString()))); para = null; }
            if (type == -2) continue;
            out.add(new Block(type, level, null, body == null ? new ArrayList<Run>() : inline(body)));
        }
        if (para != null) out.add(new Block(PARA, 0, null, inline(para.toString())));
        return out;
    }

    /** Pipe-table rows (header first, separator left out) as padded columns joined by " │ ". */
    static String table(List<String> rows) {
        List<String[]> cells = new ArrayList<String[]>();
        int cols = 0;
        for (String r : rows) {
            String body = r.substring(1);
            if (body.endsWith("|")) body = body.substring(0, body.length() - 1);
            String[] cs = body.split("\\|", -1);
            for (int k = 0; k < cs.length; k++) cs[k] = cs[k].trim().replace("**", "").replace("`", "");
            cells.add(cs);
            cols = Math.max(cols, cs.length);
        }
        int[] width = new int[cols];
        for (String[] cs : cells) for (int k = 0; k < cs.length; k++) width[k] = Math.max(width[k], cs[k].length());
        StringBuilder sb = new StringBuilder();
        for (int r = 0; r < cells.size(); r++) {
            String[] cs = cells.get(r);
            for (int k = 0; k < cols; k++) {
                String c = k < cs.length ? cs[k] : "";
                sb.append(k == 0 ? "" : " │ ").append(c);
                if (k < cols - 1) for (int p = c.length(); p < width[k]; p++) sb.append(' ');
            }
            if (r == 0) {
                sb.append('\n');
                for (int k = 0; k < cols; k++) {
                    if (k > 0) sb.append("─┼─");
                    for (int p = 0; p < width[k]; p++) sb.append('─');
                }
            }
            if (r < cells.size() - 1) sb.append('\n');
        }
        return sb.toString();
    }

    /**
     * True only for an http(s) URL without bidi / zero-width / format characters (a link
     * whose text reads differently from where it goes). Other schemes (javascript:, intent:,
     * file:, content:, sms:, ...) and null are not links.
     */
    public static boolean safeUrl(String url) {
        if (url == null || !url.regionMatches(true, 0, "http://", 0, 7) && !url.regionMatches(true, 0, "https://", 0, 8)) {
            return false;
        }
        for (int i = 0; i < url.length(); i++) {
            char ch = url.charAt(i);
            if (ch >= 0x200B && ch <= 0x200F || ch >= 0x202A && ch <= 0x202E
                    || ch >= 0x2060 && ch <= 0x2069 || ch == 0xFEFF || ch == 0x061C) {
                return false;
            }
        }
        return true;
    }

    private static final Pattern BARE_URL = Pattern.compile("https?://[^\\s<>()\\[\\]]+[^\\s<>()\\[\\].,;:!?'\"]");

    /** Inline runs: `code`, **bold**, *italic* / _italic_, [text](url), bare http(s) URLs. */
    public static List<Run> inline(String s) {
        List<Run> out = new ArrayList<Run>();
        inline(s, false, false, out);
        return out;
    }

    private static void plain(String s, boolean bold, boolean italic, List<Run> out) {
        Matcher m = BARE_URL.matcher(s);
        int at = 0;
        while (m.find()) {
            if (m.start() > at) out.add(new Run(s.substring(at, m.start()), bold, italic, false, null));
            out.add(new Run(m.group(), bold, italic, false, m.group()));
            at = m.end();
        }
        if (at < s.length()) out.add(new Run(s.substring(at), bold, italic, false, null));
    }

    private static void inline(String s, boolean bold, boolean italic, List<Run> out) {
        StringBuilder text = new StringBuilder();
        int i = 0;
        while (i < s.length()) {
            char c = s.charAt(i);
            int close;
            if (c == '`' && (close = s.indexOf('`', i + 1)) > i + 1) {
                flush(text, bold, italic, out);
                out.add(new Run(s.substring(i + 1, close), bold, italic, true, null));
                i = close + 1;
            } else if (s.startsWith("**", i) && (close = s.indexOf("**", i + 2)) > i + 2) {
                flush(text, bold, italic, out);
                inline(s.substring(i + 2, close), true, italic, out);
                i = close + 2;
            } else if ((c == '*' || c == '_') && i + 1 < s.length() && s.charAt(i + 1) != ' ' && s.charAt(i + 1) != c
                    && (c == '*' || i == 0 || !Character.isLetterOrDigit(s.charAt(i - 1)))
                    && (close = s.indexOf(c, i + 1)) > i + 1 && s.charAt(close - 1) != ' ') {
                flush(text, bold, italic, out);
                inline(s.substring(i + 1, close), bold, true, out);
                i = close + 1;
            } else if (c == '[' && (close = s.indexOf("](", i)) > i && s.indexOf(')', close) > close + 2) {
                int end = s.indexOf(')', close);
                String label = s.substring(i + 1, close), url = s.substring(close + 2, end).trim();
                flush(text, bold, italic, out);
                // the full URL is always visible: the label alone could disguise where it goes
                out.add(new Run(label.equals(url) ? url : label + " (" + url + ")", bold, italic, false, url));
                i = end + 1;
            } else {
                text.append(c);
                i++;
            }
        }
        flush(text, bold, italic, out);
    }

    private static void flush(StringBuilder text, boolean bold, boolean italic, List<Run> out) {
        if (text.length() == 0) return;
        plain(text.toString(), bold, italic, out);
        text.setLength(0);
    }

    /* ---------------- unified diff ---------------- */

    public static final int CTX = 0, ADD = 1, DEL = 2, HUNK = 3, META = 4;

    public static final class DiffLine {
        public final int kind;
        public final String text;

        DiffLine(int kind, String text) {
            this.kind = kind;
            this.text = text;
        }
    }

    /** Lines of a patch with their class; at most `limit` (the caller shows "N more lines"). */
    public static List<DiffLine> diff(String patch, int limit) {
        List<DiffLine> out = new ArrayList<DiffLine>();
        if (patch == null || patch.isEmpty()) return out;
        String[] lines = patch.replace("\r\n", "\n").split("\n", -1);
        int n = lines.length;
        if (n > 0 && lines[n - 1].isEmpty()) n--;
        for (int i = 0; i < n && out.size() < limit; i++) {
            String l = lines[i];
            int k = l.startsWith("@@") ? HUNK
                    : l.startsWith("+++") || l.startsWith("---") || l.startsWith("diff ") || l.startsWith("index ") ? META
                    : l.startsWith("+") ? ADD : l.startsWith("-") ? DEL : CTX;
            out.add(new DiffLine(k, l));
        }
        return out;
    }

    public static int diffLineCount(String patch) {
        if (patch == null || patch.isEmpty()) return 0;
        String[] lines = patch.split("\n", -1);
        return lines[lines.length - 1].isEmpty() ? lines.length - 1 : lines.length;
    }
}
