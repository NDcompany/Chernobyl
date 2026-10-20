//! Canonical mock snapshot — byte-for-byte port of `packages/protocol/src/seed.ts`.
//! Timestamps are relative to the moment `create_seed_snapshot()` is called.

use crate::protocol::{
    ActivityEvent, Agent, FileChange, Host, OutputLine, PermissionRequest, Session, SessionStep,
    Snapshot,
};

const MIN: i64 = 60_000;

pub fn create_seed_snapshot(now: i64) -> Snapshot {
    let hosts = vec![
        Host {
            id: "h1".into(),
            name: "Workstation-01".into(),
            kind: "local".into(),
            platform: "Windows 11".into(),
            address: "192.168.1.14".into(),
            status: "connected".into(),
            last_seen_at: now,
        },
        Host {
            id: "h2".into(),
            name: "edge-prod-02".into(),
            kind: "remote".into(),
            platform: "Ubuntu 24.04".into(),
            address: "10.0.4.21".into(),
            status: "connected".into(),
            last_seen_at: now - 4_000,
        },
        Host {
            id: "h3".into(),
            name: "lab-nas".into(),
            kind: "remote".into(),
            platform: "Debian 12".into(),
            address: "192.168.1.90".into(),
            status: "offline".into(),
            last_seen_at: now - 42 * MIN,
        },
    ];

    let agents = vec![
        Agent {
            id: "a1".into(),
            host_id: "h1".into(),
            kind: "claude-code".into(),
            name: "Claude Code".into(),
            state: "online".into(),
            capabilities: vec![
                "Read & edit files".into(),
                "Shell commands".into(),
                "Search codebase".into(),
                "Plan mode".into(),
            ],
            accepts_new_session: true,
        },
        Agent {
            id: "a2".into(),
            host_id: "h1".into(),
            kind: "opencode".into(),
            name: "OpenCode".into(),
            state: "online".into(),
            capabilities: vec![
                "Multi-file edits".into(),
                "Terminal tools".into(),
                "MCP tools".into(),
            ],
            accepts_new_session: true,
        },
        Agent {
            id: "a3".into(),
            host_id: "h2".into(),
            kind: "codex".into(),
            name: "Codex".into(),
            state: "busy".into(),
            capabilities: vec![
                "Code review".into(),
                "Test generation".into(),
                "Patch application".into(),
            ],
            accepts_new_session: false,
        },
        Agent {
            id: "a4".into(),
            host_id: "h2".into(),
            kind: "browser".into(),
            name: "Browser Agent".into(),
            state: "idle".into(),
            capabilities: vec![
                "Navigation".into(),
                "Form filling".into(),
                "Screenshots".into(),
            ],
            accepts_new_session: true,
        },
        Agent {
            id: "a5".into(),
            host_id: "h3".into(),
            kind: "local".into(),
            name: "Local Agent".into(),
            state: "offline".into(),
            capabilities: vec!["Offline chat".into(), "File summary".into()],
            accepts_new_session: false,
        },
    ];

    let step = |id: &str, label: &str, status: &str| SessionStep {
        id: id.into(),
        label: label.into(),
        status: status.into(),
    };
    let line = |id: &str, ts: i64, stream: &str, text: &str| OutputLine {
        id: id.into(),
        ts,
        stream: stream.into(),
        text: text.into(),
    };
    let file = |path: &str, additions: i64, deletions: i64, status: &str| FileChange {
        path: path.into(),
        additions,
        deletions,
        status: status.into(),
    };

    let sessions = vec![
        Session {
            id: "s1".into(),
            agent_id: "a1".into(),
            host_id: "h1".into(),
            title: "Refactor billing module".into(),
            project: "C:\\Projects\\admin-dashboard".into(),
            status: "running".into(),
            progress: 64,
            created_at: now - 9 * MIN,
            last_activity_at: now - 20_000,
            prompt: Some("Split the billing module into invoice, payment and tax services.".into()),
            timeline: vec![
                step("t1", "Read billing module", "done"),
                step("t2", "Extract invoice service", "done"),
                step("t3", "Extract payment service", "active"),
                step("t4", "Update tests", "pending"),
            ],
            output: vec![
                line(
                    "o1",
                    now - 8 * MIN,
                    "stdout",
                    "Read src/billing/index.ts (412 lines)",
                ),
                line(
                    "o2",
                    now - 7 * MIN,
                    "stdout",
                    "Extracting InvoiceService -> src/billing/invoice.ts",
                ),
                line(
                    "o3",
                    now - 5 * MIN,
                    "stdout",
                    "Extracting PaymentService -> src/billing/payment.ts",
                ),
                line(
                    "o4",
                    now - 40_000,
                    "stdout",
                    "Updating 6 call sites in src/api/routes.ts",
                ),
                line(
                    "o5",
                    now - 20_000,
                    "stdout",
                    "Running type check... 0 errors",
                ),
            ],
            files: vec![
                file("src/billing/invoice.ts", 148, 0, "added"),
                file("src/billing/payment.ts", 96, 0, "added"),
                file("src/billing/index.ts", 12, 187, "modified"),
                file("src/api/routes.ts", 24, 24, "modified"),
            ],
        },
        Session {
            id: "s2".into(),
            agent_id: "a2".into(),
            host_id: "h1".into(),
            title: "Install dependencies".into(),
            project: "C:\\Projects\\admin-dashboard".into(),
            status: "waiting".into(),
            progress: 30,
            created_at: now - 3 * MIN,
            last_activity_at: now - 60_000,
            prompt: Some("Install the new dependencies and update the lockfile.".into()),
            timeline: vec![
                step("t1", "Read package.json", "done"),
                step("t2", "Run npm install", "active"),
                step("t3", "Verify lockfile", "pending"),
            ],
            output: vec![
                line(
                    "o1",
                    now - 3 * MIN,
                    "stdout",
                    "Detected package-lock.json, npm workspace root found",
                ),
                line(
                    "o2",
                    now - 65_000,
                    "system",
                    "Permission required: run shell command",
                ),
            ],
            files: vec![],
        },
        Session {
            id: "s3".into(),
            agent_id: "a3".into(),
            host_id: "h2".into(),
            title: "Regression test suite".into(),
            project: "srv/api-gateway".into(),
            status: "running".into(),
            progress: 45,
            created_at: now - 12 * MIN,
            last_activity_at: now - 8_000,
            prompt: Some("Run the regression suite and fix flaky tests.".into()),
            timeline: vec![
                step("t1", "Collect failing tests", "done"),
                step("t2", "Run suite", "active"),
                step("t3", "Patch flaky tests", "pending"),
            ],
            output: vec![
                line("o1", now - 11 * MIN, "stdout", "vitest run --reporter=dot"),
                line(
                    "o2",
                    now - 6 * MIN,
                    "stdout",
                    "✓ auth/session.test.ts (18 tests) 241ms",
                ),
                line(
                    "o3",
                    now - 3 * MIN,
                    "stderr",
                    "✗ retry/rate-limit.test.ts — timed out after 5000ms",
                ),
                line(
                    "o4",
                    now - 8_000,
                    "stdout",
                    "Re-running 1 flaky test with seeded clock",
                ),
            ],
            files: vec![file("tests/retry/rate-limit.test.ts", 8, 3, "modified")],
        },
        Session {
            id: "s4".into(),
            agent_id: "a4".into(),
            host_id: "h2".into(),
            title: "Scrape competitor pricing".into(),
            project: "work/monitoring".into(),
            status: "completed".into(),
            progress: 100,
            created_at: now - 60 * MIN,
            last_activity_at: now - 34 * MIN,
            prompt: Some("Collect public pricing pages and summarise plan changes.".into()),
            timeline: vec![
                step("t1", "Open pricing pages", "done"),
                step("t2", "Extract plans", "done"),
                step("t3", "Write summary", "done"),
            ],
            output: vec![
                line("o1", now - 40 * MIN, "stdout", "Visited 4 pricing pages"),
                line(
                    "o2",
                    now - 34 * MIN,
                    "stdout",
                    "Summary written to work/monitoring/pricing-2026-10.md",
                ),
            ],
            files: vec![file("work/monitoring/pricing-2026-10.md", 62, 0, "added")],
        },
        Session {
            id: "s5".into(),
            agent_id: "a5".into(),
            host_id: "h3".into(),
            title: "Summarize incident logs".into(),
            project: "logs/2026-10-14".into(),
            status: "interrupted".into(),
            progress: 20,
            created_at: now - 3 * 60 * MIN,
            last_activity_at: now - 2 * 60 * MIN,
            prompt: Some("Summarise the incident logs from 14 October.".into()),
            timeline: vec![
                step("t1", "Index log files", "done"),
                step("t2", "Summarise", "pending"),
            ],
            output: vec![line(
                "o1",
                now - 2 * 60 * MIN,
                "system",
                "Interrupted by user after 12m",
            )],
            files: vec![],
        },
    ];

    let perm = |id: &str,
                session_id: &str,
                agent_id: &str,
                host_id: &str,
                action: &str,
                title: &str,
                command: &str,
                args: Vec<&str>,
                directory: &str,
                risk: &str,
                requested_at: i64,
                expires_at: i64,
                fingerprint: &str,
                context: &str| PermissionRequest {
        id: id.into(),
        native_request_id: id.into(),
        session_id: session_id.into(),
        agent_id: agent_id.into(),
        host_id: host_id.into(),
        action: action.into(),
        title: title.into(),
        command: command.into(),
        args: args.into_iter().map(str::to_string).collect(),
        directory: Some(directory.into()),
        risk: risk.into(),
        status: "pending".into(),
        supported_decisions: vec![
            "approve-once".into(),
            "approve-session".into(),
            "reject".into(),
        ],
        revision: 1,
        fingerprint: Some(fingerprint.into()),
        requested_at,
        expires_at,
        context: context.into(),
        decided_at: None,
        decision: None,
    };

    let permissions = vec![
        perm(
            "p1",
            "s2",
            "a2",
            "h1",
            "shell",
            "Run shell command",
            "npm install",
            vec!["--save-dev", "vitest@^3"],
            "C:\\Projects\\admin-dashboard",
            "medium",
            now - 60_000,
            now + 4 * MIN,
            "mock:shell:npm install",
            "Adding the test runner requested in the last prompt. Writes node_modules and package-lock.json.",
        ),
        perm(
            "p2",
            "s1",
            "a1",
            "h1",
            "file-write",
            "Write file",
            "src/billing/invoice.ts",
            vec![],
            "C:\\Projects\\admin-dashboard",
            "low",
            now - 40_000,
            now + 3 * MIN,
            "mock:file-write:src/billing/invoice.ts",
            "Step 3 of 5: creating the extracted InvoiceService with tests alongside.",
        ),
        perm(
            "p3",
            "s3",
            "a3",
            "h2",
            "network",
            "Open network connection",
            "GET https://registry.npmjs.org/vitest",
            vec![],
            "srv/api-gateway",
            "high",
            now - 20_000,
            now + 2 * MIN,
            "mock:network:GET https://registry.npmjs.org/vitest",
            "Resolving a pinned test-runner version from the public registry.",
        ),
    ];

    let act = |id: &str, ts: i64, kind: &str, text: &str, ref_id: Option<&str>| ActivityEvent {
        id: id.into(),
        ts,
        kind: kind.into(),
        text: text.into(),
        ref_id: ref_id.map(str::to_string),
    };

    let activity = vec![
        act(
            "ev1",
            now - 8_000,
            "session",
            "Codex resumed the regression suite on edge-prod-02",
            Some("s3"),
        ),
        act(
            "ev2",
            now - 20_000,
            "session",
            "Claude Code passed type check in admin-dashboard",
            Some("s1"),
        ),
        act(
            "ev3",
            now - 40_000,
            "permission",
            "Claude Code requested to write src/billing/invoice.ts",
            Some("p2"),
        ),
        act(
            "ev4",
            now - 60_000,
            "permission",
            "OpenCode requested to run “npm install”",
            Some("p1"),
        ),
        act(
            "ev5",
            now - 120_000,
            "session",
            "Browser Agent finished “Scrape competitor pricing”",
            Some("s4"),
        ),
        act(
            "ev6",
            now - 180_000,
            "agent",
            "Codex reported busy on edge-prod-02",
            Some("a3"),
        ),
        act(
            "ev7",
            now - 42 * MIN,
            "host",
            "lab-nas went offline",
            Some("h3"),
        ),
        act(
            "ev8",
            now - 60 * MIN,
            "host",
            "edge-prod-02 connected",
            Some("h2"),
        ),
    ];

    Snapshot {
        hosts,
        agents,
        sessions,
        permissions,
        activity,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_invariants() {
        let s = create_seed_snapshot(crate::protocol::now_ms());
        assert_eq!(s.hosts.len(), 3);
        assert_eq!(s.agents.len(), 5);
        assert_eq!(s.sessions.len(), 5);
        assert_eq!(s.permissions.len(), 3);
        assert_eq!(s.activity.len(), 8);
        // ids referenced by permissions exist
        for p in &s.permissions {
            assert!(s.sessions.iter().any(|x| x.id == p.session_id));
            assert!(s.agents.iter().any(|x| x.id == p.agent_id));
            assert!(s.hosts.iter().any(|x| x.id == p.host_id));
        }
        // pending seeded permissions are within TTL at seed time
        for p in &s.permissions {
            assert!(p.status == "pending");
            assert!(p.expires_at > p.requested_at);
        }
    }
}
