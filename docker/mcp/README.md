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
                │ HTTP (DC_BASE_URL)
┌───────────────▼──────── llama.cpp container ────────────────┐
│  the model backend (e.g. Qwen3-8B) on :11439                │
└─────────────────────────────────────────────────────────────┘
```

* The **model backend** is its own container (unchanged — llama.cpp on `:11439`).
* The **MCP server + the `dumb-coder` agent** run in *this* container.
* The **workspace** (your repo on the host) is bind-mounted at `/workspace`; that
  is where the agent's edits land, visible immediately on the host.

The MCP container reaches the model over the network via `DC_BASE_URL`. The default
`host.docker.internal:11439` works when llama.cpp publishes `:11439` on the host;
`--add-host host.docker.internal:host-gateway` makes that name resolve on Linux
Docker. To instead share a docker network, put both on it and set
`DC_BASE_URL=http://<llama-container-name>:11439/v1`.

## Build

```
docker build -f docker/mcp/Dockerfile -t dumb-coder-mcp .
```

## Register with Claude Code

Copy `mcp.json.example` to your project's `.mcp.json` (VSCode expands
`${workspaceFolder}`), or add the same block to your Claude Code MCP settings.
Adjust `DC_MODEL` to the tag your llama.cpp container actually serves
(`curl localhost:11439/v1/models`).

## Backend: the 8B pool

Bring the model up with `pwsh scripts/pool-8b.ps1` (tear down with `-Down`). That
runs **one** llama.cpp server with **three parallel slots** (`-np 3
--cont-batching`), pinned to GPU 0, serving `qwen3-8b` on `:11439`:

* Weights load **once** (~4.7GB) — three separate 8B servers would need ~14GB and
  wouldn't fit the ~11GB free across the two (un-poolable) cards.
* Context `24576 / 3 = 8192` tokens per slot, matching dumb-coder's per-run budget.
* Pinned to the 3080 Ti so the whole model stays on one card (no cross-card KV
  spill over the slow x4 link). The 3080 (GPU 1) is left free.

Three concurrent `dumb_coder_code` jobs decode in parallel across the three slots —
no load-balancing needed in the MCP, since llama.cpp's batching scheduler handles
it and every job points at the one URL.

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
| `DC_BASE_URL` | `http://host.docker.internal:11439/v1` | Model backend endpoint. |
| `DC_MODEL` | `qwen3-8b` | Model tag to request. |
| `DC_MCP_WORKSPACE` | `/workspace` | Default workspace when `code` omits one. |
| `DC_MCP_BINARY` | `dumb-coder` | Path to the agent binary. |
| `DC_MCP_YOLO` | `1` | Auto-approve shell; `0`/`false`/`no` to deny. |
