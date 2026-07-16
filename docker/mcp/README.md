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
                │ HTTP — DC_BASE_URL (or round-robin over DC_BASE_URLS)
┌──────────────┴──────────── llama.cpp backend ───────────────┐
│  Qwen3-Coder-30B on :11435 (split across both GPUs)         │
└─────────────────────────────────────────────────────────────┘
```

* The **model backend** is its own container (one llama.cpp server, the 30B split
  across both cards).
* The **MCP server + the `dumb-coder` agent** run in *this* container.
* The **workspace** (your repo on the host) is bind-mounted at `/workspace`; that
  is where the agent's edits land, visible immediately on the host.

The MCP container reaches the model via `DC_BASE_URL`.
`--add-host host.docker.internal:host-gateway` makes that hostname resolve on Linux
Docker. To instead share a docker network, put everything on it and use the container
names in the URL. `DC_BASE_URLS` (comma-separated) still works if you run several
backends and want jobs round-robined across them — see the note below.

## Build

```
docker build -f docker/mcp/Dockerfile -t dumb-coder-mcp .
```

## Register with Claude Code

Copy `mcp.json.example` to your project's `.mcp.json` (use an absolute path or
`${PWD}` for the workspace mount — the CLI does **not** expand `${workspaceFolder}`,
that's VSCode-only). Or add the same block to your Claude Code MCP settings.
Adjust `DC_MODEL` to the tag your llama.cpp container actually serves
(`curl localhost:11435/v1/models`).

## Backend: the 30B

Bring the model up with `pwsh coder-30b.ps1` (in the dumb-coder-ops repo). It
launches **one llama.cpp server** serving `qwen3-coder-30b` (a 30B MoE) split across
both GPUs on `:11435`, ~112 tok/s. This is the shipped default — it strictly beats
the 8B and clears the whole difficulty ladder.

For concurrent-agent throughput you can instead run several backends and list them
in `DC_BASE_URLS` (comma-separated); the MCP round-robins each new job across them,
so multiple GPUs are used evenly with no external load balancer. `status` reports
which `backend` a job landed on. It's either/or with the single 30B for VRAM.

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
| `DC_BASE_URL` | `http://host.docker.internal:11435/v1` | Single backend URL (used when `DC_BASE_URLS` is unset). |
| `DC_MODEL` | `qwen3-coder-30b` | Model tag to request. |
| `DC_MCP_WORKSPACE` | `/workspace` | Default workspace when `code` omits one. |
| `DC_MCP_BINARY` | `dumb-coder` | Path to the agent binary. |
| `DC_MCP_YOLO` | `1` | Auto-approve shell; `0`/`false`/`no` to deny. |
