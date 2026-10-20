# CHERNOBYL — Complete Backend Product Requirements Document

**Document type:** Backend, Desktop Companion, Connectivity and Agent Integration PRD
**Version:** 1.0
**Status:** Implementation specification
**Product:** CHERNOBYL
**Tagline:** ONE INTERFACE. EVERY AGENT.
**Repository:** `G:\Chernobyl`
**Primary objective:** Evolve the verified prototype into a complete, secure, usable product for controlling existing AI agent sessions from a mobile device anywhere in the world.

---

## 1. Executive Summary

CHERNOBYL is a remote control and observability platform for AI coding agents, general-purpose agents, local AI runtimes and browser automation.

The product consists of three primary runtime components:

1. **CHERNOBYL Mobile:** A lightweight React Native application that displays current state, streams activity, sends instructions and handles supported permission requests.
2. **CHERNOBYL Companion:** A persistent desktop background service that discovers existing agent installations and sessions, maintains local connections, normalizes events, executes authorized control operations and keeps working when the mobile app is offline.
3. **CHERNOBYL Relay and Account Backend:** A remotely accessible service that authenticates users, registers companions, maintains outbound companion connections and securely relays commands and events between mobile devices and computers across different networks.

The system must work with existing installations and sessions wherever supported interfaces allow it. Users should not have to abandon their normal OpenCode, Claude Code, Hermes or other workflows to benefit from CHERNOBYL.

The companion performs the heavy integration work. The mobile application remains a responsive client, not an agent runtime or a remote terminal implementation.

### 1.1 Product promise

A user should be able to:

- Install CHERNOBYL Companion on a Windows, Linux or macOS computer.
- Connect existing agent installations.
- Discover available agents and their existing sessions.
- Open an existing session from the phone.
- Read live activity and conversation output.
- Send additional instructions to a supported session.
- Interrupt or cancel supported work.
- Inspect workspace changes and supported artifacts.
- Review and resolve real pending permission requests.
- Monitor several computers from one mobile app.
- Leave work running when the mobile app closes.
- Reconnect from another network or location.
- Receive notifications about meaningful events.
- Revoke access when a device is lost or no longer trusted.

The product must distinguish between features that work fully, features that are limited by an adapter, and features that are not supported.

### 1.2 Definition of a finished product

A finished product is not merely a mobile UI connected to a functioning WebSocket server.

The release must include:

- A usable desktop companion with installation, background execution and updates.
- Secure account and device pairing.
- A globally reachable relay service.
- Existing-session discovery and actual agent control.
- Persistent host-side state and reliable reconnection.
- Real permission workflows where the underlying runtime supports them.
- Push notifications.
- Recovery from application restarts, host restarts and temporary network failures.
- A documented, tested security model.
- A production build and installation instructions.
- A clear privacy policy and data-retention configuration.
- Automated tests covering the actual production code paths.

A feature is complete only when its real end-to-end behavior has been verified.

---

## 2. Current Repository and Implementation Baseline

The existing repository is located at `G:\Chernobyl`.

The current architecture contains three workspaces:

| Workspace | Existing responsibility |
|---|---|
| `packages/protocol` | Shared TypeScript contracts, validators, seed data and `MockEngine` |
| `services/host` | Node.js WebSocket service with authentication, version validation, permission expiry, deduplication and health endpoint |
| `apps/mobile` | Expo/React Native app using Expo Router, TanStack Query, Zustand, React Hook Form, Zod, SQLite and SecureStore |

The project already has a functioning mobile prototype with onboarding, sign-in, dashboard, agents, sessions, session workspace, approvals, profile, settings, new-task and connect-host flows.

The current backend has verified WebSocket authentication, protocol-version checks, heartbeat handling, payload limits, snapshot resume, broadcasting and duplicate-command suppression.

The current system deliberately uses a mock engine. It does not yet integrate actual agent runtimes, provide a production desktop companion, support worldwide relay access, or deliver server-backed accounts and push notifications.

### 2.1 Mandatory preservation requirements

The implementation agent MUST:

1. Inspect the actual repository before modifying it.
2. Read the existing protocol types, validators, mock engine and transport implementation.
3. Run the current verification suite and record its baseline.
4. Preserve the existing mobile UI and its established design direction.
5. Preserve the shared message-stream concept so mock and live modes remain compatible.
6. Preserve and extend the existing command deduplication behavior.
7. Reuse the existing workspace tooling unless a documented technical issue justifies a change.
8. Avoid unnecessary dependency changes and large generated artifacts.
9. Keep the mock mode functional for UI development and automated tests.
10. Implement new functionality incrementally, with a passing test suite after each major milestone.

Do not replace the current project with a new scaffold. Do not remove working functionality simply to make the new architecture easier to implement.

### 2.2 Current behavior versus target behavior

| Capability | Current state | Target |
|---|---|---|
| Mobile UI | Implemented prototype | Preserve and connect to production backend |
| Mock transport | Implemented | Keep as a supported development/test mode |
| Authenticated host WebSocket | Implemented prototype | Harden and version for production |
| Existing-agent discovery | Missing | Real adapter-based discovery |
| Desktop companion | Missing | Installable background service |
| Global relay | Missing | Secure outbound connection architecture |
| User accounts | Missing | Production identity and authorization |
| Existing sessions | Mock only | Discover, read and control where supported |
| Permissions | Mock only | Native runtime-backed decisions |
| Audit history | Device-local only | Durable host-side and account-level event records |
| Push notifications | Local only | Remote push through configured provider |
| Multi-host management | Prototype | Production registration and connection management |
| Installation/update lifecycle | Missing | Supported installation and upgrade path |
| Production security testing | Partial | Required before release |

---

## 3. System Architecture

### 3.1 Logical architecture

The system has three principal runtime layers.

#### A. Mobile application

The mobile app is the user-facing control plane. It presents normalized state and sends typed user intentions to the backend.

It must not be responsible for discovering operating-system processes, running agent CLIs, scanning arbitrary directories or maintaining agent-specific process logic.

#### B. Desktop companion

The companion is the local execution and integration layer. It runs on each registered computer, manages supported adapters, watches agent activity, maintains local state and executes authorized operations through native interfaces.

The companion must continue operating when the phone is offline.

#### C. Cloud relay and account backend

The cloud backend provides identity, device registration, companion presence, remote connectivity, command routing, push-notification orchestration and revocation.

It must not need to receive full agent transcripts or repository contents merely to route control messages.

### 3.2 Deployment diagram

```text
                 CHERNOBYL MOBILE
              React Native + Expo
                       |
                HTTPS / WSS
                       |
             CHERNOBYL CLOUD BACKEND
          +------------+-------------+
          |            |             |
       Accounts      Registry      Relay
          |            |             |
          +------------+-------------+
                       |
              Outbound secure WSS
                from each host
                       |
           CHERNOBYL DESKTOP COMPANION
          +------------+-------------+
          |            |             |
      Discovery     Event Hub      Policy
          |            |             |
      Adapter Manager / Command Queue
          |            |             |
     +----+----+-------+-------+-----+
     |         |       |       |     |
  OpenCode  Claude   Hermes  Ollama  Other
                      Code          Workers
```

The mobile app and companion communicate through the cloud relay when direct connectivity is unavailable. A future direct-network optimization may be added, but it must not be required for the core product.

### 3.3 Network topology

The system must support the following situations:

- Phone and computer on the same Wi-Fi.
- Phone on mobile data and computer at home.
- Phone on a different Wi-Fi network from the computer.
- Computer behind a typical home router or NAT.
- Computer connected through a VPN.
- Phone temporarily offline while work continues.
- Computer temporarily offline and reconnecting later.
- Several computers connected to the same account.
- Several mobile devices authorized to access the same account.

The default worldwide-access path is an authenticated, outbound companion connection to the relay. The companion must not require inbound public ports, router port forwarding or a fixed public IP address.

The product must explain that remote control requires the companion machine to be powered on, the companion service to be running, the agent runtime to be available and the network path to be healthy.

---

## 4. Product Scope

### 4.1 Required first production release

The first production release must include:

1. Mobile-to-cloud authentication.
2. Companion enrollment and revocation.
3. Secure outbound companion-to-cloud connection.
4. Multi-host registration and presence.
5. OpenCode integration.
6. Existing-session discovery and live activity.
7. Sending messages to supported sessions.
8. Interrupting supported running sessions.
9. Real approval handling where supported.
10. Host-side command persistence and idempotency.
11. Session snapshots and event replay.
12. Remote notifications.
13. Secure session and workspace boundaries.
14. Background execution and restart recovery.
15. Production installation, configuration and troubleshooting documentation.

Claude Code and Hermes should be included in the initial integration program, with the exact production release gates determined by verified native capabilities. Ollama model inventory and health are required; Ollama-backed conversation control must be provided through a separately defined agent interface rather than misrepresenting the model server as a session manager.

### 4.2 Explicitly out of scope for the first release

- Building a new AI model runtime.
- Replacing OpenCode, Claude Code or Hermes.
- Hosting user agent processes in CHERNOBYL's cloud.
- A shared multi-user enterprise administration console.
- Arbitrary remote shell access exposed as a universal feature.
- A distributed Kubernetes-based backend.
- A mandatory cloud database containing complete transcripts.
- Automatically changing native agent permissions without user consent.
- Guaranteeing control over unsupported third-party interfaces.
- Automatically executing arbitrary generated code on behalf of the mobile app.

These can be considered later only after the core system is reliable and secure.

---

## 5. Technology and Repository Architecture

### 5.1 Required stack

Use the existing TypeScript ecosystem.

- Mobile: React Native, Expo and TypeScript.
- Companion: Node.js and TypeScript.
- Local API and control service: Fastify where appropriate.
- Shared contracts: Zod schemas and inferred TypeScript types.
- Cloud backend: TypeScript service with clear module boundaries.
- Cloud database: PostgreSQL for accounts, devices, host registration, relay metadata and push registrations.
- Companion database: SQLite for local state, event cursors, commands, approvals and audit records.
- Realtime: WebSocket over TLS.
- Local runtime integration: native APIs, event streams, documented hooks and supported CLI integrations.
- Background lifecycle: Windows Service or a user-level background process where appropriate, systemd user/system service on Linux, and launchd on macOS.
- Tests: existing test framework plus contract, integration, security and end-to-end suites.
- Deployment: containerized cloud services with managed PostgreSQL and secure secret management.

Do not introduce Redis, Kafka or another message broker without demonstrating a concrete requirement. The initial relay should be stateless where possible, with persistent state in PostgreSQL and bounded in-memory connection routing.

### 5.2 Proposed repository structure

Adapt this structure to the existing files rather than moving everything mechanically.

```text
chernobyl/
├── apps/
│   └── mobile/
│
├── services/
│   ├── host/                 # Existing local control/API core
│   ├── companion/            # Background desktop entry point
│   ├── cloud-api/            # Accounts, hosts, devices, registry
│   ├── relay/                # Authenticated live connection routing
│   └── notification-worker/  # Push delivery and retries
│
├── packages/
│   ├── protocol/             # Existing shared contracts
│   ├── domain/               # Canonical entities and state machines
│   ├── adapter-sdk/          # Adapter interface and conformance suite
│   ├── security/
│   ├── persistence/
│   ├── config/
│   └── telemetry/
│
├── adapters/
│   ├── opencode/
│   ├── claude-code/
│   ├── hermes/
│   ├── ollama/
│   ├── browser-playwright/
│   └── custom-worker/
│
├── infra/
│   ├── docker/
│   ├── migrations/
│   ├── deployment/
│   └── monitoring/
│
├── docs/
│   ├── architecture/
│   ├── reverse-engineering/
│   ├── protocol/
│   ├── adapters/
│   ├── security/
│   ├── operations/
│   └── decisions/
│
└── tests/
    ├── contract/
    ├── integration/
    ├── security/
    └── e2e/
```

#### Package ownership

`packages/protocol` owns the wire format and schema definitions.

`packages/domain` owns normalized entities, lifecycle transitions and domain invariants.

`packages/adapter-sdk` defines what an adapter must implement and provides conformance tests.

`services/host` contains reusable local API, authorization, command and event services.

`services/companion` owns desktop startup, service lifecycle, installation integration, configuration and adapter discovery.

`services/cloud-api` owns identity, account/device registration and metadata.

`services/relay` owns authenticated connection routing and bounded message forwarding.

`apps/mobile` remains a client of the published protocol. It must not import implementation details from the host or cloud services.

---

## 6. Desktop Companion Requirements

### 6.1 Purpose

The companion is the most important missing component.

It must run in the background on the user's own computer and serve as the local integration gateway between CHERNOBYL and existing agent runtimes.

The companion must:

- Start automatically after installation when the user chooses that behavior.
- Run without an open companion window.
- Reconnect to the cloud after temporary network failure.
- Discover configured and supported agent installations.
- Discover existing sessions where native interfaces expose them.
- Track live events without requiring the mobile app to stay connected.
- Send authorized instructions to existing sessions.
- Surface supported permission requests.
- Persist commands and events needed for recovery.
- Provide a local diagnostic interface.
- Expose no unauthenticated public control endpoint.
- Survive companion process restarts without silently duplicating commands.
- Allow the user to pause, disable or uninstall it.

### 6.2 Desktop platform support

The companion architecture must be portable across:

- Windows 10/11.
- Modern Linux distributions, including Debian and Ubuntu.
- macOS.

Implement and verify one primary desktop platform first, but avoid hard-coding the entire system around Windows-only APIs.

The companion should be a background service/process rather than a permanently open Electron application. A lightweight optional tray or settings UI may be added later.

Do not introduce a heavy desktop UI framework solely to keep the process alive.

### 6.3 Installation and service lifecycle

The companion must provide:

- A signed or integrity-verifiable installer where the distribution platform supports it.
- Installation and uninstallation instructions.
- Optional start-on-login/service startup.
- Configuration migration between versions.
- Graceful shutdown.
- Health status and diagnostics.
- Controlled automatic updates, with version validation and rollback strategy.
- A way to stop the service without deleting user agent data.
- A way to fully revoke the computer from the user's account.

The companion must not silently install browser extensions, modify unrelated agent settings or change native security policies.

If an integration requires a configuration change, show the exact change and request explicit consent.

### 6.4 Background operation

The companion must continue processing events and authorized work when:

- The mobile app is closed.
- The mobile device is unreachable.
- The user changes phone networks.
- The relay connection is temporarily unavailable.
- One adapter crashes while other adapters remain healthy.

A relay outage must not cause the companion to kill agent processes. Local event capture should continue when possible, and the companion should reconcile state after reconnecting.

The companion must not promise continued agent execution if the operating system suspends the machine, the process exits or the agent runtime itself stops.

### 6.5 Resource limits

The companion should be lightweight and must not run a second AI model just to observe other agents.

Requirements:

- No continuous full-disk scanning.
- No polling at extremely short intervals when native event streams exist.
- Use native APIs and filesystem watchers where appropriate.
- Use bounded queues and backpressure.
- Batch high-volume events.
- Limit retained logs and events.
- Avoid keeping large transcripts or file contents in memory unnecessarily.
- Keep idle CPU and memory usage measurable in performance tests.
- Do not download models or run inference automatically.

Establish baseline resource measurements on Windows and Debian before release. Define explicit maximums after measuring realistic idle and active workloads rather than inventing a universal memory target.

---

## 7. Existing-Agent Discovery and Integration

### 7.1 Core requirement

CHERNOBYL must be compatible with existing sessions wherever a supported interface permits it.

The companion must not assume that all sessions were created by CHERNOBYL. Existing sessions must be discoverable independently of the phone's creation workflow.

Session discovery and session control are separate capabilities. An adapter may list or read sessions without supporting prompt submission, interruption or permission decisions.

### 7.2 Discovery model

Each adapter should use the following discovery priority:

1. Documented local API or supported SDK.
2. Native event stream or supported lifecycle hook.
3. Explicitly configured local service endpoint.
4. Supported CLI query or structured export.
5. Read-only local metadata discovery, where permitted and safe.

Avoid scraping terminal output, injecting keystrokes into terminal windows or reading undocumented internal database schemas as the default strategy.

Any filesystem or process discovery must be restricted to documented locations, explicit user configuration or well-defined operating-system interfaces.

### 7.3 Adapter capability declaration

Every adapter must declare capabilities individually.

Suggested capability identifiers:

- `agent.discover`
- `agent.health.read`
- `model.list`
- `model.runtime.status`
- `session.list`
- `session.read`
- `session.history.read`
- `session.activity.stream`
- `session.message.send`
- `session.interrupt`
- `session.fork`
- `session.archive`
- `permission.list`
- `permission.review`
- `workspace.list`
- `files.read`
- `diff.read`
- `browser.inspect`
- `process.inspect`

These capabilities are not a universal guarantee. The adapter must report whether each one is available, unavailable, limited or dependent on configuration.

### 7.4 OpenCode adapter

Use OpenCode's documented server interface and event stream as the primary integration.

Required features, where supported by the installed version:

- Detect an existing OpenCode server.
- Verify its health and version.
- List available projects.
- List existing sessions.
- Read session metadata and messages.
- Subscribe to global or session-related events.
- Submit prompts to existing sessions.
- Interrupt running sessions.
- Read supported session diffs.
- Retrieve and respond to real pending permission requests.
- Discover native capability changes.
- Reconnect to the event stream.
- Handle server restarts and missing sessions.

Do not start multiple competing OpenCode servers on the same workspace without an explicit reason. Do not expose the OpenCode server directly to the internet.

Prefer local authenticated connections and validate the exact API surface against the installed OpenCode version.

Official reference: https://docs.opencode.ai/docs/server/

### 7.5 Claude Code adapter

Use documented Claude Code hooks and supported interfaces.

Required goals:

- Detect configured Claude Code environments.
- Associate hook events with a session and workspace.
- Capture supported lifecycle events.
- Observe tool activity where hooks provide it.
- Capture genuine permission requests when the runtime exposes them.
- Support additional instructions only through a verified supported mechanism.
- Report which session-control operations are not available through hooks alone.
- Recover safely if a hook invocation or local bridge fails.

The adapter may require a small, explicitly installed hook bridge to communicate with the companion.

Do not assume hooks alone provide complete transcript history, arbitrary session attachment or universal remote message injection. Investigate and document supported native interfaces before claiming those capabilities.

Do not disable Claude Code permission checks to simplify CHERNOBYL integration.

Official reference: https://code.claude.com/docs/

### 7.6 Hermes adapter

Investigate the installed Hermes Agent version and its supported APIs.

Potential integration paths include its authenticated session-control API and supported session storage interfaces.

Required goals:

- Detect Hermes installation and version.
- Detect whether the relevant local API is enabled.
- Authenticate to the local API without exposing the API key to the phone.
- List existing sessions.
- Read session metadata and message history where supported.
- Stream or subscribe to activity through documented interfaces.
- Send instructions through supported session APIs.
- Cancel or interrupt work only if a supported mechanism exists.
- Report gateway, session and model status accurately.
- Distinguish interactive CLI sessions from gateway-managed sessions.

Do not assume that a Hermes gateway session, an interactive CLI session and a stored conversation are interchangeable.

Do not parse or modify Hermes's private SQLite database as a substitute for a supported write API. Read-only metadata inspection should be separately reviewed and must never corrupt or migrate the user's own database.

Official reference: https://hermes-agent.nousresearch.com/docs/user-guide/features/api-server

### 7.7 Ollama adapter

Treat Ollama primarily as a local model-runtime provider.

Required capabilities:

- Detect whether the local Ollama API is available.
- Read runtime health and version.
- List installed models.
- List running models where supported.
- Retrieve model metadata.
- Provide runtime status and resource-related metadata when available.
- Submit generation/chat requests only for an explicitly configured CHERNOBYL-managed agent workflow.

Ollama alone does not represent the full session manager for every application using its models.

If a user runs an agent through OpenCode, Hermes or another framework backed by Ollama, the session should be managed through that framework's adapter. The Ollama adapter should provide the model-runtime information separately.

Do not create an implicit chat-history store and claim that it is an existing agent session.

Official reference: https://docs.ollama.com/api

### 7.8 Future adapter support

The adapter SDK must allow additional runtimes to be added without changing mobile screen implementations.

Potential future adapters include Codex App Server, browser automation agents, local scripts, custom workers and other agent frameworks.

Each new adapter must pass the common contract test suite before being advertised as supported.

### 7.9 Reverse-engineering and evidence requirement

Before implementing an adapter, the development agent must inspect the official documentation and the installed runtime.

Create:

`docs/reverse-engineering/<adapter-name>.md`

Each document must include:

- Runtime and version tested.
- Official documentation references.
- APIs, events, hooks and authentication used.
- Discovery behavior.
- Existing-session compatibility.
- Read and write capabilities.
- Permission semantics.
- Known limitations.
- Error and restart behavior.
- Licensing and distribution considerations.
- Evidence from tests.

Use REA as a development-only reverse-engineering aid when appropriate. Do not make REA a runtime dependency of CHERNOBYL.

---

## 8. Shared Domain Model

All components must use a consistent canonical model.

### 8.1 Host

Represents a registered computer running the companion.

Fields:

- `hostId`
- `accountId`
- `displayName`
- `platform`
- `platformVersion`
- `companionVersion`
- `protocolVersion`
- `createdAt`
- `lastSeenAt`
- `connectionStatus`
- `healthStatus`
- `capabilities`
- `revokedAt`

Do not expose private machine details unless needed and authorized.

### 8.2 Agent installation

Represents an adapter-connected runtime or installation.

Fields:

- `agentId`
- `hostId`
- `adapterType`
- `nativeInstanceId`
- `displayName`
- `runtimeVersion`
- `healthStatus`
- `capabilities`
- `lastDiscoveredAt`
- `lastSuccessfulOperationAt`
- `diagnosticCode`

### 8.3 Session

Fields:

- `sessionId`
- `hostId`
- `agentId`
- `adapterType`
- `nativeSessionId`
- `title`
- `workspaceId`
- `status`
- `statusConfidence`
- `createdAt`
- `updatedAt`
- `lastActivityAt`
- `lastEventSequence`
- `capabilities`
- `metadata`

A session's identity must be scoped to the host and adapter. Never assume native session IDs are globally unique.

### 8.4 Turn

Fields:

- `turnId`
- `sessionId`
- `nativeTurnId`
- `status`
- `startedAt`
- `finishedAt`
- `resultSummary`
- `failureCode`

### 8.5 Workspace

Fields:

- `workspaceId`
- `hostId`
- `displayName`
- `canonicalPath`
- `repositoryMetadata`
- `accessPolicy`
- `registeredAt`

Workspace paths are sensitive local data. Do not expose arbitrary filesystem paths in push notifications or unauthenticated APIs.

### 8.6 Permission request

Fields:

- `approvalId`
- `hostId`
- `agentId`
- `sessionId`
- `nativeRequestId`
- `requestType`
- `targetDescription`
- `riskContext`
- `supportedDecisions`
- `status`
- `revision`
- `fingerprint`
- `createdAt`
- `expiresAt`
- `resolvedAt`
- `resolvedByDeviceId`

### 8.7 Command

Fields:

- `commandId`
- `accountId`
- `deviceId`
- `hostId`
- `sessionId`
- `commandType`
- `idempotencyKey`
- `payload`
- `status`
- `createdAt`
- `acceptedAt`
- `startedAt`
- `completedAt`
- `result`
- `errorCode`
- `correlationId`

### 8.8 Activity event

Fields:

- `eventId`
- `hostId`
- `sequence`
- `entityType`
- `entityId`
- `eventType`
- `payload`
- `occurredAt`
- `receivedAt`
- `correlationId`
- `causationId`

### 8.9 Status and uncertainty

Use explicit states rather than a single `online` boolean.

Host connection states:

- `online`
- `connecting`
- `degraded`
- `offline`
- `revoked`

Session states:

- `idle`
- `queued`
- `running`
- `waiting`
- `awaiting_approval`
- `completed`
- `failed`
- `interrupted`
- `unknown`

Track freshness separately from execution state. A session may be running while its host is temporarily unreachable.

---

## 9. Companion Internal Services

The companion should be decomposed into independently testable modules.

### 9.1 Discovery manager

Responsibilities:

- Load configured adapters.
- Detect supported installations.
- Discover available runtime endpoints.
- Track discovered sessions.
- Periodically reconcile session inventories.
- Avoid duplicate adapter registrations.
- Respect configured directory and runtime boundaries.
- Expose discovery diagnostics.

Discovery must be incremental. Do not scan the entire disk continuously.

### 9.2 Adapter manager

Responsibilities:

- Load adapters.
- Validate adapter manifests.
- Initialize adapters.
- Maintain adapter health.
- Start and stop native event subscriptions.
- Handle reconnection.
- Isolate adapter errors.
- Enforce adapter-level timeouts and resource limits.

A failing adapter must not crash the companion or stop unrelated agents from being monitored.

### 9.3 Session manager

Responsibilities:

- Maintain normalized session records.
- Map native IDs to canonical IDs.
- Merge discovery results with live events.
- Track freshness and status confidence.
- Maintain session-to-workspace associations.
- Request snapshots when event history is incomplete.
- Prevent one adapter from modifying another adapter's sessions.

### 9.4 Command dispatcher

Responsibilities:

- Validate typed command payloads.
- Authenticate the origin through the established cloud/device trust path.
- Enforce account, host, session and capability boundaries.
- Check user policy.
- Persist command acceptance.
- Deduplicate commands.
- Route commands to the correct adapter.
- Track execution status.
- Publish results and errors.
- Recover safely after restarts.

Do not expose a generic `execute arbitrary command string` operation as the universal control interface.

### 9.5 Permission manager

Responsibilities:

- Receive actual native permission requests.
- Persist pending request metadata.
- Assign canonical IDs.
- Track expiry and native request identity.
- Publish approval events.
- Revalidate decisions.
- Reject stale or duplicate decisions.
- Forward valid decisions to the native runtime.
- Record the outcome.
- Reconcile if the runtime has already resolved or withdrawn the request.

### 9.6 Event hub

Responsibilities:

- Normalize native events.
- Assign per-host event sequence numbers.
- Persist events before acknowledging durable publication.
- Broadcast events to authorized local subscribers.
- Forward events to the relay.
- Support event cursor replay.
- Coalesce high-volume deltas where appropriate.
- Trigger resynchronization when retained history is insufficient.

### 9.7 Local persistence service

Use SQLite for:

- Companion configuration.
- Adapter registry metadata.
- Session metadata and native references.
- Command records.
- Pending approvals.
- Event sequence and replay metadata.
- Audit entries.
- Notification delivery metadata where appropriate.
- Schema migrations.

Do not unnecessarily duplicate complete agent transcripts or repositories into SQLite.

### 9.8 Health manager

Track:

- Companion process health.
- Cloud connection health.
- Adapter health.
- Agent runtime health.
- Database health.
- Queue depth.
- Event processing lag.
- Last successful discovery.
- Last successful command.
- Last successful synchronization.

Expose structured diagnostic codes instead of dumping secrets or raw environment variables into logs.

---

## 10. Worldwide Relay and Account Backend

### 10.1 Purpose

The cloud backend must make the companion reachable without exposing a public inbound control port on the user's computer.

The companion establishes an outbound secure WebSocket connection to the relay. The relay authenticates the companion and associates its connection with the registered host.

The mobile client authenticates with the account service, requests access to a registered host, and opens an authorized realtime connection.

The relay routes permitted messages between the mobile client and companion.

### 10.2 Required cloud services

#### Identity service

Responsibilities:

- User registration and login.
- Session and token management.
- Credential refresh and revocation.
- Device enrollment authorization.
- Account deletion and recovery.
- Abuse protection and rate limiting.

Use a maintained authentication implementation or a reputable identity provider rather than inventing a password system. Store passwords only through an appropriate password-hashing implementation if the product manages passwords directly.

Support secure mobile credential storage.

#### Host registry

Responsibilities:

- Register companions to accounts.
- Maintain host metadata.
- Track last-seen timestamps.
- Track companion version and protocol compatibility.
- Record revoked hosts.
- Authorize host access.
- Support renaming and removal.

#### Device registry

Responsibilities:

- Register mobile devices.
- Track device identity and public-key metadata where applicable.
- Track push-notification tokens.
- Revoke individual devices.
- Enforce device and account limits.
- Audit enrollment and revocation.

#### Relay service

Responsibilities:

- Accept authenticated companion connections.
- Accept authenticated mobile connections.
- Authorize access to specific hosts.
- Route commands and events.
- Enforce message-size limits and rate limits.
- Handle connection replacement and reconnects.
- Report presence.
- Avoid durable storage of full transcripts.
- Support request correlation and delivery acknowledgements.

#### Notification worker

Responsibilities:

- Consume eligible event notifications.
- Apply account and device preferences.
- Send push notifications.
- Retry transient delivery failures with bounded backoff.
- Avoid duplicate notifications where possible.
- Remove invalid provider tokens.
- Record delivery status without storing unnecessary message content.

### 10.3 Relay security

The relay must not behave as a universal shell proxy.

Every forwarded command must be bound to:

- Authenticated account.
- Authorized device.
- Registered host.
- Target session or resource.
- Typed operation.
- Request ID.
- Idempotency key where applicable.
- Protocol version.

The companion independently validates the command before execution.

The relay must not be able to bypass host-side policy simply because it has a live connection.

### 10.4 Relay delivery guarantees

The relay must provide authenticated, ordered delivery within a connection and explicit acknowledgements for command routing.

It must not promise exactly-once execution merely because WebSocket delivery is reliable.

The actual guarantee is:

- At-least-once command delivery may occur during reconnection.
- Command processing is idempotent by stable command ID.
- The companion persists accepted commands before execution.
- A duplicate returns the existing command state.
- The agent runtime's native execution semantics determine whether an operation can be retried safely.
- Ambiguous outcomes must be reported as unknown until reconciled.

### 10.5 Relay data minimization

The relay should normally route:

- Host and session metadata.
- Status events.
- Command envelopes.
- Approval metadata.
- Activity events.
- Bounded message fragments.
- Authorized file/diff request metadata.

Full transcripts, repository files and large artifacts should not be stored in cloud infrastructure by default.

Where possible, the relay forwards encrypted transport payloads without logging their contents. Account-level metadata and event routing records may still be required for reliable delivery and abuse prevention.

The privacy policy must clearly describe the metadata that the service stores and the content that passes through its infrastructure.

### 10.6 Connection protocol

Companion connection lifecycle:

1. Companion starts.
2. Companion loads its locally stored device credential.
3. Companion obtains or refreshes short-lived connection authorization.
4. Companion opens an outbound WSS connection.
5. Relay validates identity, revocation, version and account status.
6. Relay registers the active host connection.
7. Companion sends its host snapshot and last event cursor.
8. Relay acknowledges registration.
9. Companion and relay exchange heartbeats and event messages.
10. On disconnect, companion reconnects with bounded exponential backoff and jitter.
11. On reconnect, companion reconciles commands and event cursors before declaring itself fully synchronized.

A second connection for the same host must not silently create two active command executors. The relay must define how connection replacement and stale connections are handled.

---

## 11. Authentication and Pairing

### 11.1 Account authentication

The mobile app must authenticate with the account service and receive a scoped access token with a limited lifetime.

Refresh credentials must be stored in secure platform storage. Access tokens must not be included in URLs or written to ordinary logs.

The companion must have a distinct identity and credential from the user's mobile devices.

### 11.2 Companion pairing

Use a short-lived pairing flow:

1. User signs in to the mobile app.
2. User chooses “Add Computer.”
3. Companion generates a one-time pairing code or equivalent enrollment challenge.
4. User enters or scans the challenge through the authenticated app.
5. Cloud service validates the challenge and the account.
6. User confirms the host name and requested access.
7. Cloud service registers the companion and issues its device credential.
8. Companion stores the credential securely.
9. Mobile app displays the host and connection status.
10. Pairing challenge expires and cannot be reused.

Do not expose long-lived secrets in QR codes or pairing links.

### 11.3 Revocation

Users must be able to revoke:

- One mobile device.
- One companion host.
- All sessions associated with a lost device.
- All access tokens for an account.

Revocation must prevent future commands promptly. Existing connections must be closed or invalidated, and the companion must refuse new control requests from revoked identities.

The UI must distinguish between removing a host from the dashboard and actually revoking its credentials.

### 11.4 Current prototype authentication migration

The current host sends a token through the WebSocket connection URL.

Before production, replace long-lived query-string credentials with a safer authentication flow. Use a short-lived single-use connection ticket or an authenticated handshake suitable for the selected client platform.

Do not assume the ticket itself grants unlimited access. Bind it to the device, host, intended audience and short expiry. Never log the ticket.

---

## 12. Command and Execution Model

### 12.1 Typed commands

Every mutating operation must be a typed command.

Initial command types:

- `session.send_message`
- `session.interrupt`
- `session.fork`
- `session.archive`
- `permission.decide`
- `workspace.refresh`
- `host.refresh`
- `agent.refresh`
- `artifact.read`

Additional operations may be added only after their schemas, authorization rules and failure semantics are defined.

Do not use arbitrary payloads as a substitute for command schemas.

### 12.2 Command lifecycle

Canonical states:

- `created`
- `queued`
- `accepted`
- `running`
- `completed`
- `failed`
- `cancelled`
- `expired`
- `unknown`

The state machine must prevent invalid transitions.

A command that has already started execution must not be marked cancelled merely because the phone disconnected.

### 12.3 Idempotency

Every command includes:

- Unique `commandId`.
- Idempotency key.
- Account and device context.
- Host ID.
- Optional session ID.
- Typed payload.
- Creation timestamp.
- Expiry or validity policy.

Persist accepted commands before dispatch.

A repeated command with the same ID returns the existing status rather than executing again.

If the companion crashes after sending a command to the native runtime but before recording the result, it must reconcile the runtime state before deciding whether retrying is safe.

### 12.4 Timeout handling

A timeout is not proof of failure.

If a client times out:

1. Query the existing command by ID.
2. Reconcile with the companion if connected.
3. Return the last known status.
4. Mark the result unknown when safe reconciliation is impossible.
5. Never automatically repeat a non-idempotent operation merely because the acknowledgement was lost.

### 12.5 Interrupt behavior

Interrupting work must call the adapter's supported native interrupt or cancellation mechanism.

The UI must distinguish between:

- Interrupt request submitted.
- Interrupt accepted by the runtime.
- Execution actually stopped.
- Runtime cannot confirm the outcome.

Never treat closing a session screen, closing the WebSocket or stopping the companion as a successful interrupt.

---

## 13. Permission Approval and Safety

### 13.1 Requirements

Permission requests must represent actual pending requests in the underlying runtime.

The system must not invent a permission prompt merely because an agent is executing a tool.

Each request contains:

- Native request ID.
- Host, agent and session references.
- Action and target description.
- Supported decisions.
- Creation and expiry timestamps.
- Revision or fingerprint.
- Current resolution state.

### 13.2 Approval flow

1. Native runtime or adapter reports a pending permission request.
2. Companion validates and persists the request.
3. Companion emits a normalized permission event.
4. Cloud relay forwards the event to authorized clients.
5. Mobile app displays the action and available decisions.
6. User chooses a decision.
7. Mobile app submits the request ID, revision and decision.
8. Cloud authorizes the request and routes it to the companion.
9. Companion rechecks that the request is still pending.
10. Companion invokes the native approval mechanism.
11. Companion records and publishes the result.

### 13.3 Supported decisions

Possible decisions include:

- Allow once.
- Reject.
- Allow a narrowly scoped future rule, where supported.
- Cancel or withdraw, where supported.

Only show choices supported by the actual runtime.

Persistent rules require explicit user consent and must not silently grant unrestricted future execution.

### 13.4 Expiry

The existing two-minute expiry behavior may be retained as an initial policy where appropriate, but the system must not claim that every native request expires after two minutes.

Track CHERNOBYL's presentation expiry separately from the native runtime's own expiry.

If a request has expired or already been resolved, reject the stale decision and refresh the UI.

### 13.5 Unsafe or unsupported operations

If an adapter cannot safely resolve permissions remotely, display the request as requiring local intervention.

Do not bypass the runtime's permission system, disable safety checks or fabricate a successful approval.

---

## 14. Event Streaming and Synchronization

### 14.1 Event envelope

All services use the shared protocol package.

```typescript
export interface HostEvent<T = unknown> {
  protocolVersion: number;
  eventId: string;
  hostId: string;
  sequence: number;
  occurredAt: string;

  entity: {
    type:
      | "host"
      | "agent"
      | "session"
      | "turn"
      | "approval"
      | "command"
      | "artifact";
    id: string;
  };

  type: string;
  payload: T;

  correlationId?: string;
  causationId?: string;
}
```

Payloads must be validated against schemas registered for the event type.

Unknown event types must not crash older clients. Incompatible protocol versions must be rejected or explicitly negotiated.

### 14.2 Initial event types

- `host.status.changed`
- `host.health.changed`
- `agent.discovered`
- `agent.health.changed`
- `session.discovered`
- `session.updated`
- `session.status.changed`
- `turn.started`
- `turn.completed`
- `turn.failed`
- `activity.message.delta`
- `activity.tool.started`
- `activity.tool.completed`
- `activity.tool.failed`
- `permission.requested`
- `permission.resolved`
- `command.accepted`
- `command.completed`
- `command.failed`
- `workspace.diff.updated`
- `connection.resync_required`

### 14.3 Event ordering

Each companion maintains a monotonically increasing event sequence for its host.

The sequence must be persisted so a restart does not silently reset it.

The relay preserves per-host ordering where possible. The mobile app detects gaps and requests replay or a full snapshot.

Do not require global total ordering across independent hosts.

### 14.4 Reconnection and resynchronization

When a connection is re-established:

1. Authenticate and verify host/device status.
2. Exchange protocol version and capabilities.
3. Submit the last known event cursor.
4. Replay retained events where possible.
5. Query current host and session snapshots.
6. Reconcile unresolved commands.
7. Reconcile pending approvals.
8. Mark the client synchronized.

If event history has expired, send `connection.resync_required` and rebuild from snapshots.

### 14.5 Backpressure

The companion and relay must not accumulate unlimited events.

Use:

- Bounded queues.
- Event batching.
- Coalescing for high-frequency status and output updates.
- Flow control or acknowledgement windows.
- Per-client subscriptions.
- Limits on transcript and artifact payload sizes.
- Snapshot recovery when replay is no longer practical.

Do not stream every log line from every agent to every mobile client.

---

## 15. API and Realtime Contracts

All APIs must be versioned and documented.

### 15.1 Cloud HTTP API

Initial endpoint families:

```text
POST   /v1/auth/register
POST   /v1/auth/login
POST   /v1/auth/refresh
POST   /v1/auth/logout

GET    /v1/account
DELETE /v1/account

GET    /v1/devices
POST   /v1/devices/pairing-challenges
POST   /v1/devices/pair
DELETE /v1/devices/:deviceId

GET    /v1/hosts
GET    /v1/hosts/:hostId
POST   /v1/hosts/enroll
DELETE /v1/hosts/:hostId
POST   /v1/hosts/:hostId/revoke

GET    /v1/hosts/:hostId/sessions
GET    /v1/sessions/:sessionId
GET    /v1/sessions/:sessionId/activity
GET    /v1/sessions/:sessionId/diff

POST   /v1/hosts/:hostId/commands
GET    /v1/commands/:commandId

GET    /v1/approvals
POST   /v1/approvals/:approvalId/decision

GET    /v1/notifications/preferences
PATCH  /v1/notifications/preferences
POST   /v1/notifications/devices
DELETE /v1/notifications/devices/:deviceId

GET    /v1/health
GET    /v1/version
```

This is a contract outline. The implementation may refine endpoint names, but it must not silently expose unauthorized host operations.

Every endpoint requires an explicit authentication and authorization policy.

### 15.2 Companion local API

The companion may expose a loopback-only diagnostic API for:

- Health.
- Version.
- Adapter status.
- Local setup.
- Diagnostics.
- Controlled configuration changes.

Bind to loopback by default. Do not expose this API on public interfaces.

If a local API permits changes, require local authentication or a protected setup channel and validate all inputs.

### 15.3 Companion-to-relay WebSocket

Required message families:

- `hello`
- `hello.ack`
- `heartbeat`
- `heartbeat.ack`
- `host.snapshot`
- `host.snapshot.ack`
- `event.batch`
- `event.ack`
- `command.deliver`
- `command.ack`
- `command.result`
- `approval.update`
- `resync.request`
- `protocol.error`
- `connection.close`

All messages require schema validation, size limits and authorization checks.

### 15.4 Mobile realtime WebSocket

Required message families:

- `client.hello`
- `client.hello.ack`
- `host.subscribe`
- `host.unsubscribe`
- `snapshot.request`
- `snapshot.response`
- `event.batch`
- `command.submit`
- `command.ack`
- `command.status`
- `approval.decision`
- `connection.resync_required`
- `protocol.error`

The relay must verify that a device is allowed to subscribe to each host before forwarding events.

### 15.5 API documentation

Maintain:

- OpenAPI specification for HTTP APIs.
- Versioned protocol schemas for realtime messages.
- Examples for pairing and connection setup.
- Error-code reference.
- Compatibility and deprecation policy.
- Adapter capability documentation.

---

## 16. Database Architecture

### 16.1 Cloud database

Use PostgreSQL for durable account and control-plane metadata.

Initial tables:

#### `accounts`

- Account ID.
- Identity-provider subject or securely managed credential reference.
- Status.
- Created and updated timestamps.
- Deletion timestamp.

#### `devices`

- Device ID.
- Account ID.
- Device type.
- Credential or public-key metadata.
- Display name.
- Created timestamp.
- Last-seen timestamp.
- Revocation timestamp.

#### `hosts`

- Host ID.
- Account ID.
- Companion public identity.
- Display name.
- Platform.
- Companion version.
- Protocol version.
- Status.
- Last-seen timestamp.
- Revocation timestamp.

#### `pairing_challenges`

- Challenge ID.
- Account ID.
- Challenge hash.
- Expiry.
- Attempt count.
- Consumed timestamp.

#### `push_tokens`

- Token ID.
- Device ID.
- Provider.
- Encrypted or appropriately protected token.
- Status.
- Last success.
- Invalidated timestamp.

#### `notification_preferences`

- Account ID.
- Device ID where applicable.
- Event categories.
- Quiet hours.
- Delivery preferences.

#### `relay_sessions`

- Connection ID.
- Host or device ID.
- Connection generation.
- Established timestamp.
- Last heartbeat.
- Closed timestamp.
- Diagnostic close code.

This table is for connection metadata, not full in-memory connection objects.

#### `audit_metadata`

Store security-relevant account/device operations where cloud-level audit is required. Do not duplicate complete agent activity into this table.

### 16.2 Companion database

Use SQLite for local durable operational state.

Initial tables:

- `schema_migrations`
- `companion_config`
- `adapter_instances`
- `native_entity_mappings`
- `sessions`
- `commands`
- `approvals`
- `events`
- `event_cursors`
- `audit_entries`
- `workspace_registry`
- `notification_outbox`

The companion database is the source of truth for locally observed events and local command execution history. Cloud metadata is the source of truth for account registration and device revocation.

### 16.3 Data retention

Define retention periods by data category.

- Connection heartbeat metadata: short retention.
- Command outcomes: retained long enough for reliable troubleshooting and duplicate suppression.
- Audit entries: configurable and protected from ordinary agent modification.
- Event history: bounded by age and storage quota.
- Full transcripts: not copied to cloud by default.
- File and diff content: fetched on demand and not retained remotely by default.
- Invalid push-token records: cleaned up.
- Deleted-account data: removed or anonymized according to the documented deletion policy and legal requirements.

The implementation must enforce retention rather than merely documenting it.

---

## 17. Notifications

### 17.1 Required notifications

Users should be able to receive notifications for:

- Permission requests requiring attention.
- Agent work completed.
- Agent work failed.
- Host disconnected for a meaningful duration.
- Host reconnected after a significant outage.
- Companion requiring an update.
- Security-relevant account or device events.

Users must be able to disable individual categories.

### 17.2 Delivery flow

1. Companion emits a relevant event.
2. Relay or notification service evaluates the event.
3. Account and device preferences are applied.
4. Notification is deduplicated where possible.
5. Push provider receives a minimal payload.
6. Delivery result is recorded.
7. Mobile app opens the appropriate screen through a deep link.
8. Mobile app fetches current authoritative state before enabling an action.

Push notifications must not be the authoritative source for permission state.

### 17.3 Privacy

Notification previews must not include raw shell commands, secrets, full prompts, complete file paths or sensitive tool output by default.

A notification should say, for example, “An agent is waiting for your approval” and direct the user to the authenticated approval screen.

---

## 18. Security and Threat Model

### 18.1 Protected assets

- Agent sessions and transcripts.
- Source code and workspace contents.
- Shell and browser capabilities.
- Provider API keys and credentials.
- Device credentials.
- Pending permission requests.
- Account identity.
- Companion configuration.
- Audit history.

### 18.2 Threats

The system must consider:

- Stolen mobile device.
- Compromised account.
- Revoked or replayed device credential.
- Unauthorized access to another user's host.
- Malicious agent output.
- Malformed adapter events.
- Duplicate or replayed commands.
- Stale permission approvals.
- Relay account/host mix-ups.
- WebSocket floods.
- Compromised local runtime.
- Malicious local process attempting to impersonate an adapter.
- Secret leakage through logs, prompts or notifications.
- Malicious workspace content influencing agent behavior.
- Companion update tampering.
- Denial of service against the relay or companion.

### 18.3 Required controls

- TLS for all remote connections.
- Secure credential storage.
- Short-lived access and connection tickets.
- Independently revocable device identities.
- Host-side authorization.
- Typed command schemas.
- Per-host and per-session access checks.
- Idempotency keys.
- Replay and expiry protection.
- Message-size limits.
- Rate limits and bounded queues.
- Secret redaction.
- Explicit workspace boundaries.
- Minimal OS privileges.
- Secure update verification.
- Dependency and supply-chain checks.
- Audit records for consequential actions.
- Safe handling of unknown protocol messages.
- Tests for cross-account and cross-host access attempts.

### 18.4 Local trust boundaries

The companion must not assume every process on the same computer is trusted to control it.

Where possible, use OS permissions and protected local communication channels. Keep local API access restricted to loopback or an explicitly configured private interface.

Never expose the companion's local control port through the relay without authenticated authorization and strict endpoint scoping.

### 18.5 Auditability

Audit records should include:

- Actor device ID.
- Account ID.
- Host ID.
- Session ID where relevant.
- Command or approval ID.
- Operation type.
- Authorization decision.
- Timestamp.
- Outcome.
- Failure code if applicable.

Avoid recording raw secrets or unnecessarily duplicating full prompts.

Ordinary agent output must never be allowed to rewrite or delete security audit records.

---

## 19. Reliability and Failure Recovery

### 19.1 Companion restart

After restart, the companion must:

- Load configuration and migrations.
- Reinitialize adapters.
- Reconcile native sessions.
- Restore event sequence counters.
- Load unresolved command records.
- Reconcile pending approvals.
- Reconnect to the relay.
- Resume synchronization.
- Avoid blindly repeating uncertain commands.

### 19.2 Relay outage

During relay outage:

- Existing agent processes continue independently.
- Companion continues observing local events where possible.
- Local command and event queues remain bounded.
- No remote command is reported as accepted unless it was actually accepted.
- Reconnection triggers reconciliation.
- Expired or stale approvals are not applied automatically.
- The mobile app clearly shows offline state.

### 19.3 Adapter outage

If one adapter fails:

- Other adapters continue running.
- The failed adapter enters a degraded state.
- Retry with bounded exponential backoff and jitter.
- Record diagnostics.
- Preserve its last known session metadata as stale.
- Reconcile on recovery.
- Do not mislabel stale data as live.

### 19.4 Database failure

The companion must fail closed for new consequential operations if durable command or approval state cannot be written safely.

Do not continue executing arbitrary control operations while silently losing audit and idempotency records.

### 19.5 Event backlog

If the companion cannot forward events quickly enough:

- Coalesce replaceable status updates.
- Batch output deltas.
- Apply bounded queue limits.
- Preserve security-relevant events and command outcomes.
- Mark event history gaps.
- Trigger snapshot resynchronization.

Never let a slow phone consume unbounded memory on the desktop.

---

## 20. Observability and Diagnostics

The system must be diagnosable by a normal user without exposing secrets.

### 20.1 Companion diagnostics

Display or export:

- Companion version.
- OS and architecture.
- Cloud connection state.
- Last successful heartbeat.
- Adapter health.
- Runtime version.
- Last successful discovery.
- Event backlog.
- Database status.
- Last successful command.
- Error code and timestamp.

Logs must use structured fields and redact credentials, tokens and sensitive payloads.

### 20.2 Cloud observability

Track:

- Active companion connections.
- Active mobile connections.
- Authentication failures.
- Pairing failures.
- Relay routing failures.
- Message latency.
- Disconnect frequency.
- Queue depth.
- Push delivery success.
- API error rate.
- Database health.
- Resource usage.

Avoid collecting complete agent transcripts for operational metrics.

### 20.3 Correlation

Use a correlation ID across:

Mobile request → Cloud API → Relay → Companion → Adapter → Runtime result.

Return the correlation ID in error details so users can report a problem without sharing secrets.

---

## 21. Performance Requirements

The app should remain responsive even when several hosts and sessions are active.

### 21.1 Mobile requirements

- Use cached snapshots for fast initial rendering.
- Show stale status accurately while reconnecting.
- Stream only relevant session events.
- Virtualize long activity and transcript lists.
- Batch high-frequency deltas.
- Avoid full-screen rerenders for every token.
- Keep file/diff content on demand.
- Keep background push handling lightweight.
- Use bounded local caches.
- Avoid storing unnecessary full transcripts.

### 21.2 Companion requirements

- Prefer event-driven integration over frequent polling.
- Use bounded event queues.
- Do not load complete session histories unless requested.
- Avoid duplicate discovery work.
- Isolate adapter failures.
- Limit retained event and diagnostic history.
- Avoid AI inference in the observer/relay path.

### 21.3 Relay requirements

- Forward only to authorized connected clients.
- Use bounded message buffers.
- Do not retain full transcript payloads by default.
- Avoid unnecessary database queries for every heartbeat.
- Use persistent records only where required for correctness.
- Define connection limits and per-account rate limits.

### 21.4 Measurement policy

Establish baseline measurements for:

- Companion idle CPU and memory.
- Companion CPU and memory with active sessions.
- Mobile cold start.
- Mobile warm reconnect.
- Command acknowledgement latency.
- Event propagation latency.
- Host restart recovery.
- Relay connection recovery.

Measure on real hardware and real agent runtimes. Do not claim a performance target has been met based only on a mock benchmark.

---

## 22. Testing Requirements

### 22.1 Existing test preservation

Keep the current host integration tests and protocol tests. Extend them rather than replacing them.

`npm run verify` must continue to run the repository's required checks, updated to cover new workspaces.

### 22.2 Unit tests

Cover:

- Domain state transitions.
- Command idempotency.
- Adapter capability validation.
- Permission expiry and revisions.
- Event sequence persistence.
- Reconnect backoff.
- Host and device authorization.
- Schema validation.
- Retention rules.
- Notification preference filtering.

### 22.3 Contract tests

Every adapter must pass tests for:

- Capability declarations.
- Entity mapping.
- Event normalization.
- Unsupported-operation behavior.
- Error mapping.
- Native session identity.
- Reconnect behavior.
- Command result handling.
- Permission lifecycle where supported.

### 22.4 Integration tests

Use real local services where possible.

Test:

- Mobile client to cloud API.
- Companion to relay.
- Relay routing to the correct host.
- OpenCode discovery and control.
- Hermes session discovery and control through supported APIs.
- Claude Code hooks and permission events.
- Ollama health and model inventory.
- SQLite migrations and recovery.
- PostgreSQL migrations.
- Push-notification job retries.

### 22.5 Security tests

Required scenarios:

- A user cannot access another account's host.
- A mobile device cannot access a revoked host.
- A revoked device cannot reuse an existing connection.
- A duplicate command is not executed twice.
- A stale approval cannot be applied.
- A request cannot target another host by changing an ID.
- Oversized messages are rejected.
- Invalid schemas are rejected.
- Expired pairing challenges cannot be reused.
- Invalid or revoked relay connections are closed.
- Agent output cannot alter the authorization policy.
- Secrets do not appear in logs or push payloads.

### 22.6 End-to-end tests

The first production-grade end-to-end test must demonstrate:

1. Install and start a companion.
2. Pair it to a real account.
3. Discover an existing OpenCode installation.
4. List an existing session.
5. Open the session on the mobile app.
6. Send a prompt from the phone.
7. Observe actual output.
8. Interrupt supported work.
9. Handle a genuine permission request where the runtime supports it.
10. Disconnect the phone while the agent continues.
11. Reconnect from a different network.
12. Recover current state without duplicating a command.
13. Restart the companion and reconcile.
14. Revoke the host and verify control is denied.

Mock-only tests do not satisfy this release gate.

---

## 23. Deployment and Operations

### 23.1 Cloud deployment

Deploy the cloud API, relay and notification worker as separate logical services. They may initially share a repository and deployment environment.

Use:

- HTTPS/WSS termination.
- PostgreSQL with migrations.
- Secret management.
- Automated backups and restore tests.
- Structured logs and metrics.
- Health and readiness checks.
- Rate limiting.
- Versioned deployment artifacts.
- Rollback procedures.

The relay must support long-lived WebSocket connections and the deployment platform's idle-timeout behavior must be understood and configured.

Do not assume a serverless HTTP-only platform can host the required persistent relay without verifying its connection and runtime constraints.

### 23.2 Development environment

Provide documented commands for:

- Installing dependencies.
- Starting the mobile app.
- Starting the companion locally.
- Starting cloud services.
- Starting the relay.
- Running migrations.
- Running tests.
- Running the complete verification suite.
- Using mock mode.
- Running real OpenCode integration tests.
- Building desktop installers.
- Building Android releases.

Provide `.env.example` files containing placeholder values only. Never commit live credentials.

### 23.3 Environment separation

Maintain separate development, staging and production environments.

Development must default to mock mode unless the developer explicitly enables a real integration.

Staging should use isolated credentials and test hosts.

Production must not use seed users, default passwords, mock approvals or hardcoded tokens.

### 23.4 Release and migration safety

Every release must include:

- Version metadata.
- Database migration compatibility.
- Protocol compatibility.
- Companion update notes.
- Rollback guidance.
- Security fixes.
- A tested upgrade path from the previous supported version.

If a mobile client and companion use incompatible protocol versions, show an actionable upgrade message instead of repeatedly reconnecting without explanation.

---

## 24. Privacy and Data Ownership

CHERNOBYL must communicate where data is processed and stored.

### 24.1 Local-first content handling

Agent execution remains on the user's computer.

The companion reads or forwards the content necessary for the user's requested operations. The relay must not store complete session histories by default.

### 24.2 User controls

Users must be able to:

- Disconnect a host.
- Revoke a host.
- Revoke a device.
- Disable selected adapters.
- Restrict registered workspace directories.
- Configure notifications.
- Export supported audit records.
- Delete their account.
- Understand what data is stored locally and in the cloud.

### 24.3 Account deletion

Account deletion must:

1. Revoke access tokens and device credentials.
2. Mark registered hosts as revoked.
3. Close active relay connections.
4. Remove cloud account data according to the deletion policy.
5. Invalidate push-notification registrations.
6. Preserve only data that must lawfully be retained, with documented justification.
7. Explain how local companion data is handled.

Deleting a cloud account must not silently delete user repositories, native agent sessions or local model files.

---

## 25. Implementation Roadmap and Release Gates

The project must be delivered through tested vertical slices.

### Phase 0 — Repository audit and reverse engineering

Deliverables:

- Existing project inventory.
- Current test/build baseline.
- Architecture decision records.
- Verified protocol documentation.
- Adapter research documents.
- Threat model.
- Identified technical debt.

Exit criteria:

- The implementation agent can explain the current message flow and persistence model.
- All current tests pass.
- No existing mobile screens have been unnecessarily removed.
- Adapter feasibility is documented with evidence.

### Phase 1 — Domain and protocol hardening

Deliverables:

- Canonical entities.
- Versioned event envelope.
- Typed commands.
- Command lifecycle state machine.
- Permission state machine.
- Adapter SDK.
- Contract tests.

Exit criteria:

- Mock engine and real transport use the same schemas.
- Invalid commands and events are rejected.
- Unsupported capabilities are represented explicitly.

### Phase 2 — Companion foundation

Deliverables:

- Background process entry point.
- Configuration management.
- SQLite persistence.
- Adapter lifecycle manager.
- Discovery interface.
- Health and diagnostics.
- Graceful shutdown and restart recovery.

Exit criteria:

- Companion runs without the mobile app.
- Adapter failure does not crash the companion.
- Commands and event cursors survive restarts.
- Resource usage is measured.

### Phase 3 — OpenCode integration

Deliverables:

- Existing server discovery.
- Existing session discovery.
- Session details and activity.
- Prompt submission.
- Interrupt.
- Supported diff access.
- Real permission request integration.
- Native event normalization.

Exit criteria:

- A real existing OpenCode session can be controlled end to end.
- No mock success responses remain in the real adapter.
- Reconnection and restart recovery work.

### Phase 4 — Account and pairing backend

Deliverables:

- Authentication.
- Host registration.
- Device registration.
- Pairing challenges.
- Credential revocation.
- PostgreSQL migrations.
- Account-level authorization.

Exit criteria:

- Two independent test accounts cannot access each other's hosts.
- Pairing challenges expire and cannot be reused.
- Revoked credentials stop working.

### Phase 5 — Secure relay

Deliverables:

- Companion outbound connection.
- Mobile connection authorization.
- Host presence.
- Command routing.
- Event forwarding.
- Heartbeats.
- Reconnect and resynchronization.
- Backpressure and rate limits.

Exit criteria:

- Phone controls a host across different networks without public inbound ports.
- Lost connections do not duplicate commands.
- Relay cannot bypass companion authorization.

### Phase 6 — Production mobile integration

Deliverables:

- Account-backed sign-in.
- Host list and connection state.
- Live session updates.
- Typed command submissions.
- Approval center.
- Reconnect and snapshot recovery.
- Device/host revocation UI.

Exit criteria:

- The existing UI is connected to real data.
- Mock mode remains available.
- Offline/stale states are honest.
- Mobile performance remains acceptable during streaming.

### Phase 7 — Additional adapters

Deliverables:

- Claude Code adapter.
- Hermes adapter.
- Ollama runtime adapter.
- Additional adapter contract tests.

Exit criteria:

- Existing-session support is verified separately for each runtime.
- Unsupported capabilities are documented.
- No adapter relies on unsafe permission bypasses.

### Phase 8 — Notifications and operations

Deliverables:

- Push registration.
- Notification worker.
- User preferences.
- Production metrics.
- Log redaction.
- Backup and restore.
- Deployment automation.
- Companion installer and update strategy.

Exit criteria:

- Notifications link to authoritative current state.
- Cloud and companion recovery have been tested.
- Installation and troubleshooting documentation is complete.

### Phase 9 — Production release gate

Required before public release:

- Security tests pass.
- Real end-to-end test passes.
- Installers work on the supported platforms.
- Account deletion and revocation work.
- Privacy and retention policies are published.
- Backup restore is tested.
- No production credentials are committed.
- All supported integrations have documented versions and capabilities.
- The application no longer advertises unimplemented mock features as real.

---

## 26. Definition of Done

A backend feature is complete only when all relevant criteria are met:

1. It has a documented requirement.
2. It has a defined protocol contract.
3. It has authentication and authorization rules.
4. It has validated input and output schemas.
5. It has persistence behavior where required.
6. It has error and retry semantics.
7. It has observability and diagnostics.
8. It has automated tests.
9. It works with the real runtime or service it claims to support.
10. It has documented limitations.
11. It does not break existing mock-mode functionality.
12. It is documented for installation and operation.

The overall product is ready only when a user can install the companion, pair it securely, connect from a phone over the internet, discover and control a real existing agent session, disconnect without interrupting the work, reconnect and recover state, and revoke access safely.

---

## 27. Implementation Instructions for the Coding Agent

Treat this document as the backend source of truth.

Before coding:

1. Inspect `G:\Chernobyl`.
2. Read the root package configuration and all existing workspace manifests.
3. Inspect `packages/protocol` and `services/host`.
4. Run the current verification suite.
5. Create `docs/architecture/CHERNOBYL_BACKEND_PRD.md` from this document.
6. Create a prioritized implementation plan.
7. Identify conflicts between the current implementation and this PRD.
8. Record major decisions in `docs/decisions/`.
9. Perform evidence-based reverse engineering for each agent integration.
10. Begin implementation only after the baseline and plan are documented.

Implement incrementally. Do not attempt to generate the complete production backend in one unverified code-writing pass.

Preserve the existing mobile UI. Preserve the mock engine. Reuse existing schemas where appropriate, but evolve them with explicit versioning and migration compatibility.

Do not claim that worldwide access works until the relay has been implemented and tested from different networks.

Do not claim that an agent integration works until it has been tested against a real installed runtime and existing session.

Do not claim that an approval is real unless it maps to an actual pending native permission request.

The first complete vertical slice is:

**Existing OpenCode session → desktop companion → authenticated relay → mobile app → prompt/interrupt → real runtime events → mobile state reconciliation.**

Only after that vertical slice is reliable should the implementation expand to additional adapters and advanced features.

---

## 28. Primary Technical References

Use the official documentation and verify behavior against installed versions:

- OpenCode server API: https://docs.opencode.ai/docs/server/
- OpenCode CLI and headless server: https://docs.opencode.ai/docs/cli/
- Claude Code documentation: https://code.claude.com/docs/
- Hermes Agent API server: https://hermes-agent.nousresearch.com/docs/user-guide/features/api-server
- Hermes Agent sessions: https://hermes-agent.nousresearch.com/docs/user-guide/sessions/
- Ollama API: https://docs.ollama.com/api

Documentation links are references for investigation, not proof that every possible feature is supported in every installed version.

**Final architectural rule:** CHERNOBYL must remain a lightweight mobile client, a reliable local companion and a secure remote-control system. Existing agents keep doing the work; CHERNOBYL provides the unified visibility, routing, authorization and control layer.
