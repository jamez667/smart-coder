# dumb-coder as an MCP server

Run dumb-coder as a **fire-and-poll parallel coding agent** inside Claude Code (or
any MCP client). Claude delegates a self-contained coding task to a fast local
small-model agent, gets a job id back immediately, and polls for the result —
issuing several at once to run local workers in parallel while it does other work.

## Topology

Three separate pieces, three separate homes:

```
┌─ Claude Code (VSCode, on the host) ─────────────────────────┐
│   launches the MCP server via `docker run -i` over stdio    │
└───────────────┬─────────────────────────────────────────────┘
                │ JSON-RPC over stdin/stdout
┌───────────────▼─────────────── MCP container ───────────────┐
│  dumb-coder-mcp  ──spawns──▶  dumb-coder run/swarm --json    │
│  workspace bind-mounted at /workspace  (edits land here)     │
└───────────────┬─────────────────────────────────────────────┘
                │ HTTP — round-robin over DC_BASE_URLS
┌──────────────┴──────────── llama.cpp pools ─────────────────┐
│  pool 0: Qwen3-8B on :11439 (GPU 0, 3 slots)                │
│  pool 1: Qwen3-8B on :11440 (GPU 1, 2 slots)   = 5 agents   │
└─────────────────────────────────────────────────────────────┘
```

* The **model backends** are their own containers (one llama.cpp pool per GPU).
* The **MCP server + the `dumb-coder` agent** run in *this* container.
* The **workspace** (your repo on the host) is bind-mounted at `/workspace`; that
  is where the agent's edits land, visible immediately on the host.

The MCP container reaches the models via `DC_BASE_URLS` (comma-separated) and
**round-robins each new job across the pools**, so both GPUs are used evenly — no
external load balancer. `--add-host host.docker.internal:host-gateway` makes that
hostname resolve on Linux Docker. To instead share a docker network, put everything
on it and use the container names in `DC_BASE_URLS`. (`DC_BASE_URL`, singular, still
works for a one-pool setup.)

## Build

```
docker build -f docker/mcp/Dockerfile -t dumb-coder-mcp .
```

## Register with Claude Code

Copy `mcp.json.example` to your project's `.mcp.json` (VSCode expands
`${workspaceFolder}`), or add the same block to your Claude Code MCP settings.
Adjust `DC_MODEL` to the tag your llama.cpp container actually serves
(`curl localhost:11439/v1/models`).

## Backend: the 8B pools

Bring the models up with `pwsh scripts/pool-8b.ps1` (tear down with `-Down`). It
launches **one llama.cpp server per GPU**, each with parallel slots (`-np N
--cont-batching`), serving `qwen3-8b`:

| Pool | GPU | Port | Slots | Context | Per slot |
| --- | --- | --- | --- | --- | --- |
| 0 | 3080 Ti | :11439 | 3 | 36864 | 12288 |
| 1 | 3080 | :11440 | 2 | 32768 | 16384 |

= **5 concurrent agents**. Design notes:

* Weights load **once per server** (~4.7GB) — loading the 8B once per job wouldn't
  fit. Each pool is pinned to one card (`CUDA_VISIBLE_DEVICES`), never tensor-split
  across the slow x4 link.
* Context is spent on KV cache to fill spare VRAM (bigger KV costs nothing in
  throughput — it just widens each job's window). The 3080 Ti is shared with the
  Windows desktop so it keeps a ~1.5GB buffer (49152 OOMs); the 3080 has no desktop
  load so it pushes further. Retune the table in the script.
* The MCP round-robins jobs across both pools; within a pool, llama.cpp's batching
  scheduler runs the slots concurrently. Nothing else to configure — every job just
  points at the URL it was assigned. A lone job gets a whole slot at near-full speed;
  under contention each slot is ~20% slower but aggregate throughput is far higher.

Concurrent `dumb_coder_code` jobs decode in parallel across all five slots — the
MCP spreads them across the two pools and llama.cpp batches within each. `status`
reports which `backend` a job landed on.

## Tools

| Tool | Purpose |
| --- | --- |
| `dumb_coder_code` | Start a coding job. Args: `task` (required), `workspace` (default `/workspace`), `decompose` (bool — `true` fans the task out across dumb-coder's own parallel workers for larger tasks). Returns a `job_id` immediately. |
| `dumb_coder_status` | Poll a `job_id`. Returns `state` (running/done/failed), `stop_reason`, `finished_ok`, `exit_code`, and the tail of the event stream. |
| `dumb_coder_health` | Check the model backend is reachable (`dumb-coder doctor`). |

The agent **writes no tests and self-decides when done** (`stop_reason: Finished`).
Verify the diff yourself after a job completes — e.g. `git diff` + your own tests.
Shell commands the agent needs are auto-approved (`DC_MCP_YOLO=1`); set
`DC_MCP_YOLO=0` to deny shell (edits-only, safer, but a run needing a command will
stall).

## Configuration (env)

| Var | Default | Meaning |
| --- | --- | --- |
| `DC_BASE_URLS` | (falls back to `DC_BASE_URL`) | Comma-separated backend URLs; jobs round-robin across them. |
| `DC_BASE_URL` | `http://host.docker.internal:11439/v1` | Single backend URL (used when `DC_BASE_URLS` is unset). |
| `DC_MODEL` | `qwen3-8b` | Model tag to request. |
| `DC_MCP_WORKSPACE` | `/workspace` | Default workspace when `code` omits one. |
| `DC_MCP_BINARY` | `dumb-coder` | Path to the agent binary. |
| `DC_MCP_YOLO` | `1` | Auto-approve shell; `0`/`false`/`no` to deny. |
