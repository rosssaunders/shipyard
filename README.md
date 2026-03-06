# ⚓ Shipyard

**Phone-first AI engineering manager.** Orchestrate coding agents, manage projects, merge from your pocket.

## What is this?

Shipyard is a lightweight orchestration layer that sits between you and your coding agents (Codex, Claude Code, etc.). You control everything from a PWA on your phone. It handles the boring parts — spawning agents in worktrees, running quality gates, monitoring progress, notifying you when something needs attention.

**You make decisions. Agents write code. Shipyard manages the pipeline.**

## Architecture

```
┌─────────────────────────────┐
│  📱 Phone (PWA)             │
│  Preact + Tailwind (<50KB)  │
│  Push notifications         │
│  Works offline              │
└──────────┬──────────────────┘
           │ WebSocket
┌──────────▼──────────────────┐
│  🧠 Orchestrator            │
│  Small model (3-4B params)  │
│  Rust single binary         │
│  Agent lifecycle manager    │
│  Quality gates & CI         │
└──────────┬──────────────────┘
           │ Spawns
┌──────────▼──────────────────┐
│  🔨 Coding Agents           │
│  Big models (Codex, Claude) │
│  One per task / worktree    │
│  Expensive, short-lived     │
└─────────────────────────────┘
```

### Two-tier model intelligence

The orchestrator uses a **small, cheap model** (~$0.01/day) for:
- Parsing your intent ("merge that", "what's blocked?")
- Routing tasks to the right agent
- Aggregating status across projects
- Deciding when to notify you

Coding agents use **big, expensive models** ($2-10/task) for:
- Actually writing and debugging code
- Reviewing PRs
- Making architectural decisions

## Features (MVP)

- [ ] **Connect GitHub repos** — OAuth, see issues/PRs/CI
- [ ] **Kanban view** — Issues flow: Backlog → Agent Working → PR Ready → Merged
- [ ] **One-tap agent launch** — tap an issue, agent spawns in a worktree
- [ ] **Live agent output** — watch coding agents work in real-time from your phone
- [ ] **Quality gates** — tests, clippy, benchmarks run automatically before you see the PR
- [ ] **Push notifications** — agent finished, CI broke, PR needs review
- [ ] **Chat input** — ad hoc commands ("run benchmarks", "what broke CI?")
- [ ] **Multi-project** — switch between repos with a swipe

## Tech Stack

| Layer | Tech | Why |
|-------|------|-----|
| Frontend | Preact + Tailwind | <50KB bundle, PWA installable, feels native |
| Backend | Rust (axum) | Single binary, WebSocket server, fast |
| State | SQLite | Zero-config, embedded, reliable |
| Git | libgit2 / gh CLI | Worktree management, PR creation |
| Agent runtime | Process manager | Spawn/monitor/kill coding agents |
| Small model | llama.cpp / API | Local inference or GPT-5-nano |
| Auth | GitHub OAuth | Repo access, no extra accounts |

## Self-hosted

Shipyard runs on **your machine** or a **$5/mo VPS**. Zero cloud dependency. Your code never leaves your infrastructure.

```bash
# Install (coming soon)
cargo install shipyard

# Start
shipyard serve --port 3000

# Open on phone
# https://your-machine:3000
```

## Pricing (hosted version, coming later)

| Tier | Price | What you get |
|------|-------|-------------|
| **Free** | $0 | 1 repo, 1 concurrent agent, bring your own API keys |
| **Pro** | $49/mo | 5 repos, 3 concurrent agents, quality gates, benchmarks |
| **Team** | $149/mo | Unlimited repos & agents, shared dashboard, team features |

## Status

🚧 **Early development** — building the MVP.

## License

MIT
