package com.chernobyl.kitkat;

import static org.junit.Assert.assertArrayEquals;
import static org.junit.Assert.assertEquals;
import static org.junit.Assert.assertFalse;
import static org.junit.Assert.assertNull;
import static org.junit.Assert.assertTrue;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;
import org.json.JSONArray;
import org.json.JSONObject;
import org.junit.Test;

/** ADR-017/018/020 helpers against apps/mobile's rules and the repo's composer, terminal and workspace fixtures. */
public class ComposerTest {
    @Test
    public void tlsSuitesKeepOnlyEcdheGcmAndChacha() {
        String[] supported = {"TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256", "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256",
                "TLS_RSA_WITH_AES_128_GCM_SHA256", "TLS_ECDHE_RSA_WITH_AES_128_CBC_SHA", "TLS_ECDHE_RSA_WITH_RC4_128_SHA",
                "TLS_DHE_RSA_WITH_AES_256_GCM_SHA384", "TLS_RSA_WITH_NULL_SHA", "TLS_ECDH_anon_WITH_AES_128_CBC_SHA",
                "TLS_AES_128_GCM_SHA256", "TLS_CHACHA20_POLY1305_SHA256", "TLS_EMPTY_RENEGOTIATION_INFO_SCSV"};
        assertArrayEquals(new String[] {"TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256", "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256",
                "TLS_AES_128_GCM_SHA256", "TLS_CHACHA20_POLY1305_SHA256"}, Composer.tlsSuites(supported));
        // nothing modern on the device: empty, so the caller keeps the platform's list
        assertEquals(0, Composer.tlsSuites(new String[] {"TLS_RSA_WITH_AES_128_CBC_SHA", "SSL_RSA_WITH_RC4_128_SHA"}).length);
        assertEquals(0, Composer.tlsSuites(new String[0]).length);
    }

    @Test
    public void slashMenuFiltersTheStatesCommands() throws Exception {
        assertEquals("com", Composer.slashQuery("/com"));
        assertEquals("", Composer.slashQuery("/"));
        assertNull(Composer.slashQuery("/compact now"));
        assertNull(Composer.slashQuery("hi /x"));
        JSONArray names = composerState().getJSONArray("slashCommands");
        List<String> cmds = new ArrayList<String>();
        for (int i = 0; i < names.length(); i++) cmds.add(names.getString(i));
        assertEquals(Arrays.asList("compact", "context", "code-review"), Composer.filterSlash(cmds, "", 8));
        assertEquals(Arrays.asList("compact", "context"), Composer.filterSlash(cmds, "CO", 2));
        // starts-with first, then contains
        assertEquals(Arrays.asList("review", "code-review"), Composer.filterSlash(Arrays.asList("code-review", "review"), "rev", 8));
        assertEquals(Collections.<String>emptyList(), Composer.filterSlash(cmds, "zz", 8));
    }

    @Test
    public void imageBudgetAndFrame() {
        assertEquals(180_000, Composer.budgetLeft(Collections.<String>emptyList(), Composer.IMAGES_DATA_MAX));
        char[] big = new char[100_000];
        Arrays.fill(big, 'A');
        List<String> two = Arrays.asList(new String(big), new String(big));
        assertEquals(0, Composer.budgetLeft(two, Composer.IMAGES_DATA_MAX));
        assertEquals(80_000, Composer.budgetLeft(two.subList(0, 1), Composer.IMAGES_DATA_MAX));
        // the first quality whose JPEG fits; null when none does
        final int[] sizes = {150_000, 90_000, 40_000, 20_000, 9_000};
        final List<Integer> tried = new ArrayList<Integer>();
        Composer.Encoder enc = new Composer.Encoder() {
            @Override
            public String jpegBase64(int quality) {
                tried.add(quality);
                char[] d = new char[sizes[tried.size() - 1]];
                Arrays.fill(d, 'A');
                return "/9j/" + new String(d, 4, d.length - 4);
            }
        };
        String fit = Composer.fitImage(enc, 50_000);
        assertEquals(40_000, fit.length());
        assertEquals(Arrays.asList(70, 50, 35), tried);
        tried.clear();
        assertNull(Composer.fitImage(enc, 5_000));
        assertEquals(5, tried.size());
        // a full budget of image data still fits one frame; twice that does not
        char[] full = new char[Composer.IMAGES_DATA_MAX];
        Arrays.fill(full, '/'); // worst case for org.json, which escapes '/' as "\/"
        String probe = "{\"type\":\"submit_prompt\",\"text\":\"hi\",\"images\":[{\"mediaType\":\"image/jpeg\",\"data\":\"" + new String(full).replace("/", "\\/") + "\"}]}";
        assertFalse(Composer.sealedFrameBytes(probe) <= Composer.FRAME_MAX_BYTES);
        String plain = probe.replace("\\/", "A");
        assertTrue(Composer.sealedFrameBytes(plain) <= Composer.FRAME_MAX_BYTES);
        assertEquals((3 + 16 + 2) / 3 * 4 + 64, Composer.sealedFrameBytes("€"));
    }

    @Test
    public void cameraSizesAndScaling() {
        List<int[]> sizes = Arrays.asList(new int[] {2592, 1944}, new int[] {1280, 960}, new int[] {640, 480}, new int[] {1600, 1200});
        assertArrayEquals(new int[] {1280, 960}, Composer.pickPictureSize(sizes, 1280));
        assertArrayEquals(new int[] {640, 480}, Composer.pickPictureSize(Arrays.asList(new int[] {2592, 1944}, new int[] {640, 480}), 600));
        assertEquals(2, Composer.sampleSize(2592, 1944, 1280));
        assertEquals(1, Composer.sampleSize(1280, 960, 1280));
        assertEquals(4, Composer.sampleSize(5184, 3888, 1280));
        assertArrayEquals(new int[] {1280, 960}, Composer.scaledSize(2592, 1944, 1280));
        assertArrayEquals(new int[] {720, 1280}, Composer.scaledSize(1080, 1920, 1280));
        assertArrayEquals(new int[] {640, 480}, Composer.scaledSize(640, 480, 1280));
    }

    @Test
    public void chipsFromTheComposerState() throws Exception {
        JSONObject st = composerState();
        assertEquals(2, Composer.contextPercent(st)); // 22,900 of 1,000,000
        assertEquals(-1, Composer.contextPercent(new JSONObject()));
        List<String[]> choices = Composer.modelChoices(st);
        assertEquals(2, choices.size());
        assertEquals("Haiku 5.5", Composer.modelLabel("haiku", choices));
        assertEquals("Haiku 5.5", Composer.modelLabel("claude-haiku-5-5", choices));
        assertEquals("Sonnet 4.5", Composer.modelLabel("claude-sonnet-4-5-20250929", choices));
        assertEquals("Opus 5", Composer.modelLabel("claude-opus-5", choices));
        assertEquals("Model", Composer.modelLabel("", choices));
        assertEquals(4, Composer.modelChoices(new JSONObject()).size()); // the aliases when the CLI listed none
        assertEquals("Ask before edits", Composer.modeLabel("default"));
        assertEquals("Plan", Composer.modeLabel("plan"));
        assertEquals("bypassPermissions", Composer.modeLabel("bypassPermissions"));
        assertFalse(Composer.isModeChoice("bypassPermissions"));
        assertTrue(Composer.isModeChoice("acceptEdits"));
        // the composer fixture's titles pass, blanks and control characters do not
        JSONArray valid = new JSONArray(ModelTest.read("composer-valid.json"));
        for (int i = 0; i < valid.length(); i++)
            if ("rename_session".equals(valid.getJSONObject(i).getString("type"))) assertTrue(Composer.isSessionTitle(valid.getJSONObject(i).getString("title")));
        assertFalse(Composer.isSessionTitle("   "));
        assertFalse(Composer.isSessionTitle("a\nb"));
        assertFalse(Composer.isSessionTitle(new String(new char[81]).replace('\0', 'x')));
    }

    @Test
    public void mcpOpsFoldAndSummarize() throws Exception {
        Model.View v = new Model.View();
        Model.applyTranscript(v, ModelTest.fixture("transcript-snapshot.json"));
        assertEquals(0, v.mcp.length());
        Model.applyTranscript(v, ModelTest.fixture("transcript.json"));
        assertEquals(3, v.mcp.length());
        assertArrayEquals(new int[] {1, 3, 1}, Composer.mcpSummary(v.mcp));
        JSONObject linear = v.mcp.getJSONObject(1).getJSONObject("auth");
        assertTrue(Composer.isCallbackUrl("http://localhost:44818/callback?code=x", linear.getString("callbackPrefix")));
        assertFalse(Composer.isCallbackUrl("http://evil/callback?code=x", linear.getString("callbackPrefix")));
        assertFalse(Composer.isCallbackUrl("https://localhost:44818/callback", null));
    }

    @Test
    public void workingRowMatchesTheMainApp() throws Exception {
        // reference values computed with apps/mobile/src/utils/agentIdentity.ts workingVerb
        assertEquals("Assembling", Composer.workingVerb(0, "t1"));
        assertEquals("Whittling", Composer.workingVerb(7000, "turn_1"));
        assertEquals("Assembling", Composer.workingVerb(0, "a-much-longer-turn-id-0123456789"));
        assertEquals("Percolating", Composer.workingVerb(99999, "msg_01ABCxyz"));
        assertEquals(Composer.workingVerb(0, "t1"), Composer.workingVerb(-5, "t1"));
        assertEquals("Running Bash…", Composer.workingLabel("working", 0, "t1", "Bash"));
        assertEquals("Running a tool…", Composer.workingLabel("working", 0, "t1", null));
        assertEquals("Waiting for you", Composer.workingLabel("waiting", 0, "t1", null));
        assertEquals("1m 05s", Composer.formatElapsed(65_000));
        assertEquals("1h 02m", Composer.formatElapsed(3_720_000));

        Model.View v = new Model.View();
        Model.applyTranscript(v, ModelTest.fixture("transcript-snapshot.json"));
        v.turns.clear();
        assertNull(Composer.workingStatus(v, Collections.<JSONObject>emptyList()));
        v.turns.put("t9", new JSONObject().put("id", "t9").put("status", "running").put("startedAt", 5));
        v.items.add(new JSONObject().put("id", "x").put("turnId", "t9").put("kind", "tool")
                .put("tool", new JSONObject().put("name", "Edit").put("status", "running")));
        Object[] w = Composer.workingStatus(v, Collections.<JSONObject>emptyList());
        assertEquals("working", w[0]);
        assertEquals("Edit", w[1]);
        assertEquals("waiting", Composer.workingStatus(v, Collections.singletonList(new JSONObject()))[0]);
        v.items.get(v.items.size() - 1).getJSONObject("tool").put("status", "completed");
        assertEquals("thinking", Composer.workingStatus(v, Collections.<JSONObject>emptyList())[0]);
    }

    @Test
    public void terminalRunAssemblesTheFixture() throws Exception {
        JSONArray msgs = new JSONArray(ModelTest.read("terminal.json"));
        Composer.TerminalRun run = new Composer.TerminalRun("c-term-1", "npm test");
        assertFalse(run.apply(new JSONObject().put("type", "terminal_output").put("commandId", "other").put("data", "x")));
        assertTrue(run.apply(msgs.getJSONObject(0)));
        assertTrue(run.apply(msgs.getJSONObject(0)));
        assertEquals("ok 12 passing\nok 12 passing\n", run.output.toString());
        assertTrue(run.running);
        assertTrue(run.apply(msgs.getJSONObject(1)));
        assertFalse(run.running);
        assertEquals("Exit code 0", Composer.exitLabel(run.exit));
        assertTrue(Composer.exitOk(run.exit));
        // the result after an exit changes nothing
        assertFalse(run.apply(new JSONObject().put("type", "command_result").put("commandId", "c-term-1").put("status", "completed")));
        assertEquals("Timed out after 120 s, stopped · output cut at 1 MiB", Composer.exitLabel(msgs.getJSONObject(2)));
        assertFalse(Composer.exitOk(msgs.getJSONObject(2)));
        assertEquals("Stopped", Composer.exitLabel(msgs.getJSONObject(3)));
        // a reconnect lost the exit: the command_result still ends it
        Composer.TerminalRun lost = new Composer.TerminalRun("c-term-1", "x");
        assertTrue(lost.apply(new JSONObject().put("type", "command_result").put("commandId", "c-term-1").put("status", "failed").put("error", "exited with code 2")));
        assertEquals("The connection dropped during the run; it ended: exited with code 2", lost.failed);
        Composer.TerminalRun refused = new Composer.TerminalRun("c2", "x");
        refused.refused("the terminal is off");
        assertFalse(refused.running);
        // history: newest first, no duplicates, 20 at most, blanks ignored
        List<String> h = new ArrayList<String>();
        for (int i = 0; i < 25; i++) h = Composer.remember(h, "cmd " + i);
        assertEquals(20, h.size());
        assertEquals("cmd 24", h.get(0));
        h = Composer.remember(h, "  cmd 10 ");
        assertEquals("cmd 10", h.get(0));
        assertEquals(20, h.size());
        assertEquals(h, Composer.remember(h, "   "));
    }

    @Test
    public void pullAndPushLabelsFromGitStatus() throws Exception {
        JSONObject st = new JSONArray(ModelTest.read("workspace-results.json")).getJSONObject(2).getJSONObject("result");
        assertArrayEquals(new String[] {"origin/main → main", "1"}, Composer.pullSource(st));
        assertArrayEquals(new String[] {"main → origin/main", "1"}, Composer.pushTarget(st));
        st.remove("upstream");
        assertNull(Composer.pullSource(st)[1]);
        assertEquals("main has no upstream: set one on the PC", Composer.pullSource(st)[0]);
        st.put("branch", JSONObject.NULL);
        assertNull(Composer.pushTarget(st)[1]);
    }

    @Test
    public void markdownTablesBecomeAlignedText() {
        List<Md.Block> b = Md.parse("Results:\n\n| Name | Count |\n|:---|---:|\n| **apples** | 3 |\n| kiwi | `12` |\nafter");
        assertEquals(Md.PARA, b.get(0).type);
        assertEquals(Md.TABLE, b.get(1).type);
        assertEquals("Name   │ Count\n───────┼──────\napples │ 3\nkiwi   │ 12", b.get(1).code);
        assertEquals(Md.PARA, b.get(2).type);
        // a lone pipe line without a separator stays text (streaming-safe)
        assertEquals(Md.PARA, Md.parse("| a | b |").get(0).type);
    }
}
