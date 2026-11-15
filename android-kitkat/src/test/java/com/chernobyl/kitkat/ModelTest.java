package com.chernobyl.kitkat;

import static org.junit.Assert.assertEquals;
import static org.junit.Assert.assertFalse;
import static org.junit.Assert.assertNotNull;
import static org.junit.Assert.assertNull;
import static org.junit.Assert.assertTrue;

import java.io.File;
import java.nio.file.Files;
import java.util.List;
import org.json.JSONArray;
import org.json.JSONObject;
import org.junit.Test;

/** The repo's protocol fixtures (packages/protocol/fixtures/live) through the reducers and helpers. */
public class ModelTest {
    static final File FIX = new File("../packages/protocol/fixtures/live");

    static String read(String name) throws Exception {
        return new String(Files.readAllBytes(new File(FIX, name).toPath()), "UTF-8");
    }

    static JSONObject fixture(String name) throws Exception {
        return new JSONObject(read(name));
    }

    @Test
    public void helloAckSyncsAndReadsFeatures() throws Exception {
        Model m = new Model();
        assertTrue(m.apply(fixture("hello-ack.json")));
        assertTrue(m.synced);
        assertTrue(m.has("transcript.v1"));
        assertTrue(m.has("workspace.v1"));
    }

    @Test
    public void permissionRequestedIsPendingWithOptionsAndDetail() throws Exception {
        Model m = new Model();
        m.apply(fixture("permission-requested.json"));
        long requested = 1761436800300L;
        List<JSONObject> pending = m.pending(requested + 1);
        assertEquals(1, pending.size());
        JSONObject p = pending.get(0);
        assertEquals("toolu_01", p.optString("toolItemId"));
        assertEquals(4, p.optJSONArray("options").length());
        assertEquals("edit", p.optJSONObject("detail").optString("kind"));
        assertEquals(0, m.pending(p.optLong("expiresAt")).size()); // stale at its deadline
        JSONObject resolved = new JSONObject(p.toString()).put("status", "approved");
        m.apply(new JSONObject().put("type", "permission_resolved").put("permission", resolved));
        assertEquals(0, m.pending(requested + 1).size());
        assertEquals(1, m.permissions.size());
    }

    @Test
    public void snapshotThenOpsFoldLikeTranscriptTs() throws Exception {
        Model.View v = new Model.View();
        JSONObject snap = fixture("transcript-snapshot.json");
        assertFalse(Model.applyTranscript(v, snap));
        assertEquals(3, v.seq);
        assertEquals(4, v.items.size());
        JSONObject ops = fixture("transcript.json");
        assertFalse(Model.applyTranscript(v, ops));
        assertEquals(10, v.seq);
        JSONObject text = v.items.get(v.index.get("msg_01#0"));
        assertEquals("Creating a.txt now.", text.optString("text"));
        JSONObject tool = v.items.get(v.index.get("toolu_01")).optJSONObject("tool");
        assertEquals("Write", tool.optString("name"));
        assertEquals("File created successfully", tool.optString("output"));
        assertEquals("completed", v.turns.get("turn_1").optString("status"));
        // the same batch again is a no-op (duplicates skipped)
        int ver = v.version;
        assertFalse(Model.applyTranscript(v, ops));
        assertEquals(ver, v.version);
    }

    @Test
    public void gapAndEpochChangeAskToResubscribe() throws Exception {
        Model.View v = new Model.View();
        Model.applyTranscript(v, fixture("transcript-snapshot.json"));
        JSONObject ops = fixture("transcript.json");
        JSONObject gapped = new JSONObject(ops.toString());
        JSONArray arr = gapped.getJSONArray("ops");
        arr.remove(0); // seq 5 first: 4 is missing
        assertTrue(Model.applyTranscript(v, gapped));
        assertEquals(3, v.seq);
        JSONObject other = new JSONObject(ops.toString()).put("epoch", "other");
        assertTrue(Model.applyTranscript(v, other));
    }

    @Test
    public void unknownKindsAreKeptButFlagged() throws Exception {
        Model.View v = new Model.View();
        JSONObject snap = fixture("transcript-snapshot.json");
        snap.getJSONArray("items").put(new JSONObject().put("id", "x1").put("ts", 1).put("kind", "hologram"));
        Model.applyTranscript(v, snap);
        JSONObject x = v.items.get(v.index.get("x1"));
        assertEquals("unknown", x.optString("kind"));
        assertEquals("hologram", x.optString("originalKind"));
    }

    @Test
    public void composerTranscriptCarriesImageCountAndModels() throws Exception {
        Model.View v = new Model.View();
        JSONObject c = fixture("composer-transcript.json");
        Model.applyTranscript(v, new JSONObject().put("type", "transcript_snapshot").put("epoch", c.optString("epoch"))
                .put("seq", 0).put("items", new JSONArray()).put("turns", new JSONArray()).put("state", new JSONObject()));
        Model.applyTranscript(v, c);
        assertEquals(2, v.items.get(v.index.get("u_img")).optInt("images"));
        assertNotNull(v.state.optJSONArray("models"));
    }

    @Test
    public void workspaceResultsParse() throws Exception {
        JSONArray all = new JSONArray(read("workspace-results.json"));
        assertEquals(4, all.length());
        JSONObject status = all.getJSONObject(2).getJSONObject("result");
        assertEquals("git_status", status.getString("kind"));
        assertEquals("src/old.ts", status.getJSONArray("staged").getJSONObject(1).getString("origPath"));
        List<Md.DiffLine> d = Md.diff(all.getJSONObject(3).getJSONObject("result").getString("patch"), 200);
        assertEquals(7, d.size());
        assertEquals(Md.META, d.get(0).kind);
        assertEquals(Md.HUNK, d.get(4).kind);
        assertEquals(Md.CTX, d.get(5).kind);
        assertEquals(Md.ADD, d.get(6).kind);
    }

    @Test
    public void validClientMessagesOurBuildersProduceAreInTheFixture() throws Exception {
        // every client message type this app sends appears in the shared valid fixture
        String valid = read("client-valid.json") + read("composer-valid.json");
        String[] sent = {"session_subscribe", "session_unsubscribe", "submit_prompt", "permission_decision",
            "create_session", "fs_list", "fs_read", "git_status", "git_diff", "git_stage", "git_unstage", "git_commit", "git_push",
            "git_pull", "terminal_run", "terminal_stop", "set_model", "rename_session", "set_permission_mode", "session_end",
            "mcp_auth", "mcp_auth_callback", "mcp_reconnect"};
        for (String t : sent) assertTrue(t, valid.contains("\"" + t + "\""));
    }

    @Test
    public void snapshotReducerUpsertsAndOutputCaps() throws Exception {
        Model m = new Model();
        JSONObject s = new JSONObject().put("id", "s1").put("status", "running").put("output", new JSONArray());
        m.apply(new JSONObject().put("type", "session_created").put("session", s));
        JSONArray lines = new JSONArray();
        for (int i = 0; i < 350; i++) lines.put(new JSONObject().put("id", "l" + i).put("text", "x"));
        m.apply(new JSONObject().put("type", "session_output").put("sessionId", "s1").put("lines", lines).put("progress", 40).put("status", "waiting"));
        assertEquals(300, m.session("s1").optJSONArray("output").length());
        assertEquals("waiting", m.session("s1").optString("status"));
        m.apply(new JSONObject().put("type", "session_updated").put("session", new JSONObject().put("id", "s1").put("status", "completed")));
        assertEquals(1, m.sessions.size());
        assertEquals("completed", m.session("s1").optString("status"));
    }

    @Test
    public void markdownSubset() {
        List<Md.Block> b = Md.parse("# Title\n\nSome **bold** and `code` here.\n\n```js\nlet a = 1;\n```\n- one\n- two\n1. first\n---\nSee [docs](https://example.com) or https://x.dev/a.");
        assertEquals(Md.HEADING, b.get(0).type);
        assertEquals(1, b.get(0).level);
        assertEquals(Md.PARA, b.get(1).type);
        assertEquals("[[Some ], B[bold], [ and ], C[code], [ here.]]", b.get(1).runs.toString());
        assertEquals(Md.CODE, b.get(2).type);
        assertEquals("let a = 1;", b.get(2).code);
        assertEquals(Md.BULLET, b.get(3).type);
        assertEquals(Md.BULLET, b.get(4).type);
        assertEquals(Md.NUMBERED, b.get(5).type);
        assertEquals(Md.RULE, b.get(6).type);
        assertEquals("[[See ], L<https://example.com>[docs (https://example.com)], [ or ], L<https://x.dev/a>[https://x.dev/a], [.]]",
                b.get(7).runs.toString());
    }

    @Test
    public void onlyPlainWebLinksBecomeLinks() {
        String[] bad = {"javascript:alert(1)", "intent://x#Intent;scheme=http;end", "file:///sdcard/a", "content://x/y",
                "sms:123?body=hi", "https://example.com/\u202Egpj.exe", "https://exa\u200Bmple.com", "http://a.b/\u2066x",
                "https://a.b/\uFEFF", "https://a.b/\u061C", "ftp://a.b", ""};
        for (String u : bad) {
            assertFalse(u, Md.safeUrl(u));
            for (Md.Run run : Md.inline("[tap](" + u + ")")) assertNull(u, run.url);
        }
        assertFalse(Md.safeUrl(null));
        assertTrue(Md.safeUrl("https://example.com/a?b=1"));
        assertTrue(Md.safeUrl("HTTP://example.com"));
        // a bare URL with a right-to-left override is text, not a link
        for (Md.Run run : Md.inline("see https://example.com/\u202Egpj.exe now")) assertNull(run.url);
        assertEquals("[[see ], L<https://x.dev>[https://x.dev], [ now]]", Md.inline("see https://x.dev now").toString());
    }

    @Test
    public void markdownIsStreamingSafe() {
        // an unclosed fence runs to the end; an unclosed marker shows as typed
        List<Md.Block> b = Md.parse("text\n```\npartial code");
        assertEquals(Md.CODE, b.get(1).type);
        assertEquals("partial code", b.get(1).code);
        assertEquals("[[a **b]]", Md.inline("a **b").toString());
        assertEquals("[[snake_case_name]]", Md.inline("snake_case_name").toString());
        assertEquals("[[a ], I[it], [ b]]", Md.inline("a *it* b").toString());
    }
}
