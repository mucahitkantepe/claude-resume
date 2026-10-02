#!/usr/bin/env python3
"""Write made-up Claude Code transcripts for the demo recording.

    scripts/demo-sessions.py <projects dir>

The sessions are fictional, and their timestamps are relative to now, so the picker shows
"2h ago" and "yesterday" whenever the demo is recorded.
"""

import json
import os
import sys
import uuid
from datetime import datetime, timedelta, timezone

# (title, project, branch, hours ago, turns). A turn is ("you", text), ("claude", text),
# ("bash", command, output), ("edit", file, old, new) or ("pr", url).
SESSIONS = [
    ("add user authentication with OAuth2 and JWT tokens", "web-platform", "feat/auth", 2, [
        ("you", "add user authentication to the web platform: OAuth2 login with Google and GitHub, JWT access tokens"),
        ("claude", "I'll add a passport strategy per provider, issue 15-minute JWT access tokens and keep the refresh token in an httpOnly, secure cookie."),
        ("edit", "src/auth/strategies.ts", "export const strategies = [];", "export const strategies = [googleStrategy, githubStrategy];"),
        ("you", "refresh tokens should rotate on every use, and add rate limiting on /login"),
        ("claude", "Refresh tokens now rotate, and reusing an old one revokes the whole token family. /login allows 5 attempts per minute per IP."),
        ("bash", "npm test -- auth", "Test Suites: 6 passed, 6 total\nTests:       42 passed, 42 total"),
        ("you", "move the client secrets to env vars and document how to rotate them"),
        ("claude", "Secrets are read from AUTH_GOOGLE_SECRET and AUTH_GITHUB_SECRET; docs/auth.md describes rotation without downtime."),
        ("pr", "https://github.com/acme/web-platform/pull/482"),
    ]),
    ("fix memory leak in websocket connection pool", "realtime-engine", "fix/ws-leak", 5, [
        ("you", "the realtime engine leaks memory: the heap grows ~200MB an hour until the pod is OOMKilled. I suspect the websocket connection pool"),
        ("claude", "The heap profile shows closed websocket connections never leave the pool: each one holds an Arc back to the pool, a reference cycle."),
        ("bash", "cargo run --release --bin loadtest -- --connections 5000 --duration 10m", "peak RSS 1.9 GB\nafter all connections closed: 1.8 GB"),
        ("you", "break the cycle and add an idle timeout so stale websocket connections get cleaned up"),
        ("claude", "The back-reference is now a Weak pointer, and a cleanup task drops connections idle for 90s. After the same load test RSS falls back to 310 MB."),
        ("edit", "src/pool.rs", "pool: Arc<Pool>,", "pool: Weak<Pool>,"),
        ("you", "reconnects should back off with jitter, clients hammer us after a deploy"),
        ("claude", "Clients now reconnect with exponential backoff (500ms to 30s) and full jitter."),
        ("you", "add prometheus metrics for open connections and pool size, plus a grafana panel"),
        ("claude", "Added ws_connections_open and ws_pool_size gauges and a panel in dashboards/realtime.json."),
        ("you", "write a regression test that fails if connections stay in the pool after close"),
        ("bash", "cargo test pool::", "running 7 tests\ntest result: ok. 7 passed; 0 failed"),
    ]),
    ("migrate database from PostgreSQL to CockroachDB", "data-service", "feat/crdb-migration", 26, [
        ("you", "plan the migration of the data service from PostgreSQL to CockroachDB, we need multi-region"),
        ("claude", "Main incompatibilities: SERIAL keys (switch to UUIDs), a few advisory locks, and the default SERIALIZABLE isolation. I'd dual-write in shadow mode first."),
        ("you", "set up the dual write and a job that compares both databases"),
        ("bash", "make compare-shadow", "orders: 1,204,331 rows, 0 mismatches\ncustomers: 88,410 rows, 2 mismatches (timestamp precision)"),
        ("claude", "The two mismatches are timestamp precision; CockroachDB keeps microseconds where we truncated. Fixed in the writer."),
    ]),
    ("build CLI tool for infrastructure deployment", "deploy-cli", "main", 30, [
        ("you", "start a Go CLI that deploys our services: plan, apply and rollback. It should wrap terraform and support blue/green"),
        ("claude", "Scaffolded deployctl with cobra: `plan` and `apply` shell out to terraform with per-environment workspaces, and `rollback` flips the load balancer back to the previous target group."),
        ("bash", "go test ./...", "ok  \tdeployctl/cmd\t0.412s\nok  \tdeployctl/terraform\t1.031s"),
        ("you", "add canary deployments with automatic rollback when the error rate goes up"),
        ("claude", "`apply --canary 10` shifts 10% of traffic and rolls back if the 5xx rate is above 1% for 5 minutes."),
    ]),
    ("implement real-time collaborative editor with CRDT", "notepad-app", "feat/collab", 50, [
        ("you", "make the editor collaborative, several people typing in the same note at once"),
        ("claude", "I'd use Yjs: a CRDT document per note, synced over WebRTC with a small signaling server, and awareness for cursors and selections."),
        ("you", "show other people's cursors with their name and colour"),
        ("claude", "Remote cursors and selections now render from the awareness state, with the user's name and colour."),
    ]),
    ("set up monitoring stack with Prometheus and Grafana", "platform-infra", "feat/observability", 74, [
        ("you", "set up monitoring for the platform: prometheus, grafana and alertmanager, deployed with terraform and helm"),
        ("claude", "Added a terraform module that installs kube-prometheus-stack with helm, plus Loki for logs and Tempo for traces."),
        ("bash", "terraform plan -target=module.monitoring", "Plan: 14 to add, 0 to change, 0 to destroy."),
        ("you", "alert on SLO burn rate instead of raw error counts, and route pages to PagerDuty"),
        ("claude", "Added multi-window burn-rate alerts for the 99.9% availability SLO; critical alerts page through PagerDuty, the rest go to Slack."),
    ]),
    ("karabasan — atmospheric horror game prototype", "karabasan", "feat/world-gen", 98, [
        ("you", "karabasan: a first-person horror prototype. Start with procedural world generation for a foggy forest"),
        ("claude", "The terrain uses layered simplex noise (6 octaves), with biomes from temperature and humidity maps. Volumetric fog is a ray-marched pass after lighting."),
        ("you", "the fog looks flat, make it move with the wind and react to the lantern"),
        ("claude", "Fog density is now advected by a wind field, and the lantern adds a scattering term so light blooms through it."),
        ("bash", "cargo run --release -- --seed 1337", "generated 64 chunks in 412ms\n60 fps at 2560x1440"),
        ("you", "add an animation state machine for the creature, it should stalk the player and freeze when looked at"),
        ("claude", "The creature has idle, stalk and freeze states; freeze triggers when it is inside the camera frustum and not occluded."),
    ]),
    ("optimize Docker build with multi-stage and layer caching", "api-gateway", "chore/docker", 120, [
        ("you", "our docker build takes 9 minutes and the image is 1.2GB, fix it"),
        ("claude", "A multi-stage build with BuildKit cache mounts for cargo, and a distroless runtime image."),
        ("bash", "docker build -t api-gateway .", "=> exporting to image\nimage size: 38.4MB, build time 1m12s (cached: 14s)"),
    ]),
    ("refactor payment processing to event-driven architecture", "billing-service", "feat/event-driven", 150, [
        ("you", "move payment processing to events on kafka, payments must never be charged twice"),
        ("claude", "Payments become a saga over Kafka with idempotency keys, and the outbox pattern publishes events in the same transaction as the database write."),
        ("you", "what happens when the card provider times out?"),
        ("claude", "The step is retried with exponential backoff using the same idempotency key, and after 5 attempts the payment lands in the dead letter queue for review."),
    ]),
    ("implement end-to-end encryption for chat messages", "messenger-app", "feat/e2ee", 190, [
        ("you", "add end-to-end encryption to chat, following the Signal protocol"),
        ("claude", "X3DH for the key agreement and the double ratchet per conversation; group chats use sender keys."),
        ("bash", "npm test -- crypto", "Tests: 118 passed, 118 total"),
    ]),
    ("debug flaky integration tests in CI pipeline", "web-platform", "fix/flaky-tests", 220, [
        ("you", "the integration tests fail about 1 run in 10 on CI and never locally"),
        ("claude", "Two tests share a database row and run in parallel workers; the failures are a race on that state. Each test now gets its own transaction that is rolled back."),
        ("bash", "for i in $(seq 50); do npm run test:integration || break; done", "50/50 runs passed"),
    ]),
    ("design and implement GraphQL federation gateway", "api-gateway", "feat/federation", 260, [
        ("you", "we have five GraphQL services, put a federated gateway in front of them"),
        ("claude", "Apollo Router with a composed supergraph; entities are shared with @key, and DataLoader batching fixes the N+1 queries."),
    ]),
    ("set up Terraform modules for multi-account AWS landing zone", "terraform-infra", "feat/landing-zone", 300, [
        ("you", "write terraform modules for a multi-account AWS landing zone: organization, SCPs, SSO and networking"),
        ("claude", "Modules for the organization and OUs, guardrail SCPs, Identity Center permission sets and a transit gateway hub. Accounts come from an account factory."),
        ("bash", "terraform validate && terraform plan", "Success! The configuration is valid.\nPlan: 87 to add, 0 to change, 0 to destroy."),
        ("you", "turn on CloudTrail, GuardDuty and SecurityHub in every account"),
        ("claude", "They are enabled org-wide from the security account, with findings aggregated there."),
    ]),
    ("build machine learning pipeline for recommendation engine", "recommender", "feat/ml-pipeline", 400, [
        ("you", "build the training pipeline for product recommendations"),
        ("claude", "Matrix factorisation for collaborative filtering combined with content embeddings, served from a FAISS HNSW index and tracked in MLflow."),
    ]),
    ("write comprehensive API documentation with OpenAPI spec", "web-platform", "docs/api", 500, [
        ("you", "document the public API with an OpenAPI spec and publish it with Redoc"),
        ("claude", "Wrote openapi.yaml for all 34 endpoints with examples, linted with spectral, and added a docs build to CI."),
    ]),
]


def write_session(projects, title, project, branch, hours_ago, turns):
    sid = str(uuid.uuid5(uuid.NAMESPACE_URL, "claude-resume-demo/" + title))
    cwd = "/home/dev/" + project
    end = datetime.now(timezone.utc) - timedelta(hours=hours_ago)
    clock = end - timedelta(minutes=4 * len(turns))
    lines = [{"type": "ai-title", "aiTitle": title, "sessionId": sid}]
    seq = 0

    def entry(kind, message, **extra):
        nonlocal clock
        clock += timedelta(minutes=2)
        return {
            "parentUuid": None, "isSidechain": False, "userType": "external", "cwd": cwd,
            "sessionId": sid, "version": "2.1.286", "gitBranch": branch, "type": kind,
            "message": message, "uuid": str(uuid.uuid4()),
            "timestamp": clock.strftime("%Y-%m-%dT%H:%M:%S.000Z"), **extra,
        }

    def tool(name, tool_input, result):
        nonlocal seq
        seq += 1
        tool_id = f"toolu_demo{seq:03}"
        lines.append(entry("assistant", {
            "id": f"msg_demo{seq:03}", "role": "assistant",
            "content": [{"type": "tool_use", "id": tool_id, "name": name, "input": tool_input}],
        }))
        lines.append(entry("user", {
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": tool_id, "content": result}],
        }, toolUseResult={"stdout": result, "stderr": "", "interrupted": False}))

    for turn in turns:
        kind = turn[0]
        if kind == "you":
            lines.append(entry("user", {"role": "user", "content": turn[1]}))
        elif kind == "claude":
            seq += 1
            lines.append(entry("assistant", {
                "id": f"msg_demo{seq:03}", "role": "assistant", "model": "claude-opus-5-5",
                "content": [{"type": "text", "text": turn[1]}],
            }))
        elif kind == "bash":
            tool("Bash", {"command": turn[1]}, turn[2])
        elif kind == "edit":
            tool("Edit", {"file_path": f"{cwd}/{turn[1]}", "old_string": turn[2],
                          "new_string": turn[3]}, f"The file {cwd}/{turn[1]} has been updated.")
        elif kind == "pr":
            lines.append({"type": "pr-link", "prNumber": int(turn[1].rsplit("/", 1)[1]),
                          "prUrl": turn[1], "prRepository": turn[1].split("/")[3] + "/" + project,
                          "sessionId": sid, "timestamp": clock.strftime("%Y-%m-%dT%H:%M:%S.000Z")})

    directory = os.path.join(projects, cwd.replace("/", "-").replace(".", "-"))
    os.makedirs(directory, exist_ok=True)
    path = os.path.join(directory, sid + ".jsonl")
    with open(path, "w") as f:
        f.writelines(json.dumps(line) + "\n" for line in lines)
    os.utime(path, (clock.timestamp(), clock.timestamp()))


def main():
    if len(sys.argv) != 2:
        sys.exit("usage: scripts/demo-sessions.py <projects dir>")
    for session in SESSIONS:
        write_session(sys.argv[1], *session)


if __name__ == "__main__":
    main()
