# Spawn Tool — Background Tasks & Auto-Delivery

> **Since:** v0.5.0-beta.348

The `spawn` tool enables the main agent to create sub-agents (workers) that execute tasks independently. When a background task completes, results are **automatically delivered** back to the user through the originating channel or CLI.

## Overview

```
┌─────────────┐     spawn(wait=false)     ┌──────────────┐
│  Main Agent │ ───────────────────────►   │  Sub-Agent   │
│  (Telegram) │   "Returns immediately"   │  (Worker)    │
└──────┬──────┘                            └──────┬───────┘
       │                                          │
       │               broadcast                  │
       │◄─────────── SpawnCompletion ─────────────┘
       │            (when task finishes)
       ▼
┌──────────────┐
│ Delivery to  │
│ Telegram/CLI │
└──────────────┘
```

## Parameters

| Parameter | Type   | Default  | Description                                    |
|-----------|--------|----------|------------------------------------------------|
| `task`    | string | required | What the sub-agent should do                   |
| `agent`   | string | `worker` | Which agent config to use (from `[agents.*]`)  |
| `label`   | string | `""`     | Human-readable label for status tracking       |
| `wait`    | bool   | `true`   | Wait for result (`true`) or fire-and-forget    |

## Usage Examples

### Inline (wait=true, default)

```
User: spawn subagent lấy giá vàng
Agent: [uses spawn with wait=true, returns result inline]
→ Giá vàng XAU/USD: $4,976.30/oz
```

### Background (wait=false)

```
User: spawn subagent chạy nền, không chờ kết quả, kiểm tra disk usage
Agent: ✅ Đã spawn chạy nền kiểm tra disk usage.

... (a few seconds later, auto-delivered) ...

✅ Spawn "disk-check" (completed):
Filesystem      Size  Used Avail Use%
/dev/sda1       500G  234G  266G  47%
```

## Configuration

### Agent Config (`config.toml`)

```toml
[agents.worker]
provider = "openrouter"
model = "anthropic/claude-sonnet-4-20250514"
agentic = true
max_iterations = 100
allowed_tools = ["*"]   # Wildcard: give all tools to sub-agent
# Or specify explicitly:
# allowed_tools = ["shell", "file_read", "web_fetch", "web_search"]
```

### Wildcard `allowed_tools`

- `["*"]` — sub-agent receives **all** parent tools (except `delegate` and `spawn`)
- `["shell", "web_fetch"]` — sub-agent only gets listed tools
- `[]` or omitted — sub-agent gets default basic tools (shell, file_read, file_write, etc.)

## Architecture

### Broadcast Mechanism

The `SubagentManager` uses a `tokio::sync::broadcast` channel to emit `SpawnCompletion` events when tasks finish:

- **`SpawnCompletion`** — carries task ID, label, status, result, and reply context
- **`SpawnReplyCtx`** — captures where to deliver (channel name, recipient, thread ID)

### Delivery Listeners

| Context    | Listener Location       | Behavior                                    |
|------------|------------------------|---------------------------------------------|
| Telegram   | `channels/mod.rs`      | Sends result as message to originating chat |
| Discord    | `channels/mod.rs`      | Sends result to originating channel/thread  |
| Slack      | `channels/mod.rs`      | Sends result to originating thread          |
| CLI        | `agent/loop_.rs`       | Prints result to stdout                     |

### Reply Context Flow

1. Before each message processing, `set_reply_context()` stores the current channel/recipient/thread
2. When `spawn(wait=false)` fires, the context is captured and attached to the background task
3. On completion, `run_task()` broadcasts `SpawnCompletion` with the captured context
4. The appropriate listener receives the event and delivers the result

## Files Changed

| File | Change |
|------|--------|
| `src/tools/spawn.rs` | Core spawn/subagent logic, broadcast mechanism |
| `src/tools/spawn_status.rs` | Status checking tool for spawned tasks |
| `src/tools/mod.rs` | Export `SubagentManager`, return handle from `all_tools` |
| `src/channels/mod.rs` | Channel delivery listener, reply context injection |
| `src/agent/loop_.rs` | CLI delivery listener |
| `src/agent/agent.rs` | Updated caller signature |
| `src/gateway/mod.rs` | Updated caller signature |
