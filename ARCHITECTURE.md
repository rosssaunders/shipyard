# Shipyard Architecture

## The Brain

The brain is a **state-of-the-art model** (Claude Opus / GPT-5.4 Pro) that acts as an engineering lead. It's not a router — it thinks.

### What the brain does

1. **Intake** — reads the GitHub issue, understands context, estimates scope
2. **Planning** — decides: one agent or multiple? what order? what dependencies?
3. **Prompt engineering** — writes detailed, context-rich prompts for coding agents
   - Specific files to study
   - Architecture constraints
   - Testing commands
   - Commit message format
   - What NOT to do
4. **Dispatch** — spawns agents in worktrees with the crafted prompts
5. **Monitor** — watches agent progress, intervenes if stuck
6. **Review** — reads the diff, checks for:
   - Regressions (WASM compat, test failures)
   - Architecture violations
   - Missing edge cases
   - Quality issues
7. **Recovery** — if agent fails: retry with better prompt? break into smaller pieces? escalate to human?
8. **Merge** — handles rebasing, conflict resolution, CI fixes

### Brain invocation points (each is one LLM call)

```
Issue arrives
    ↓
[BRAIN: Plan] — read issue, codebase context → detailed prompt + subtask breakdown
    ↓
Agent(s) spawned with crafted prompts
    ↓
Agent finishes
    ↓
[BRAIN: Review] — read diff, run quality checks → approve / request changes / reject
    ↓
If approved → [BRAIN: Merge] — handle conflicts, CI, push
    ↓
If rejected → [BRAIN: Retry] — new prompt with failure context
```

### Cost model

- Brain: 3-4 calls per task × ~$0.25/call = **~$1/task**
- Coding agent: 1 run × $2-10 = **$2-10/task**  
- Total: **$3-11/task**

The brain is <10% of the cost but provides >90% of the value.

### What the brain knows (context)

Per-project knowledge that persists:
- **Architecture map** — key files, module structure, how things connect
- **Build system** — test commands, CI config, lint rules, WASM targets
- **Past failures** — "Codex always forgets --no-verify on push", "WASM doesn't have tokio"
- **Coding standards** — commit message format, PR conventions, review criteria
- **Dependencies** — "Phase 2 needs Phase 1 merged first"

This is stored in a project config that the brain reads before planning.

## Agent Runtime

Coding agents are ephemeral:
- Spawned in git worktrees
- Given a detailed prompt from the brain
- Run to completion (or timeout)
- Output captured for brain review
- Worktree cleaned up after merge

Supported agents:
- **Codex** (`codex --yolo -m <model>`)
- **Claude Code** (`claude -p <prompt> --dangerously-skip-permissions`)
- Future: any CLI agent that takes a prompt and works in a directory

## Quality Gates

Automated, run after agent completes:
1. `cargo test` (or project-specific test command)
2. `cargo clippy` / `eslint` / project linter
3. WASM build check (if applicable)
4. Benchmark regression check (if baseline exists)
5. Brain review (LLM reads the diff)
6. Auto-merge (if all gates pass and enabled)

## Phone UI

The user sees the brain's narrative, not raw terminal output:
- Task cards with threaded event timelines
- Brain's thinking visible: "Breaking this into 3 subtasks because..."
- Quality gate results inline
- One-tap approval for merge
- Push notifications for decisions needed
