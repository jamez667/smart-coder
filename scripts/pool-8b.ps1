# Launch the Qwen3-8B pool that backs the dumb-coder MCP server: ONE llama.cpp
# server with N parallel slots (`-np N --cont-batching`), pinned to the 3080 Ti
# (GPU 0). Weights load once (~4.7GB Q4) and N concurrent requests decode via
# continuous batching — N MCP coding jobs run at the same time on one card,
# instead of loading the 8B N times (which wouldn't fit our ~11GB free).
#
# Context is split evenly across the slots: -c 36864 / 3 slots = 12288 tokens per
# slot. That 36864 is sized to spend the 3080 Ti's spare VRAM on KV cache while
# leaving a ~1.5GB buffer for the Windows desktop (measured: ~10.4GB used, ~1.6GB
# free). Bigger KV costs nothing in throughput — it just gives each parallel job a
# larger window (12288 vs. dumb-coder's 8192 budget = 50% headroom). Retune with
# -Ctx / -Slots; the safe ceiling on a desktop-shared 12GB Ti is ~36864 at 3 slots
# (44032 fits but leaves <1GB — risky when the desktop spikes; 49152 OOMs).
#
# Pinned to GPU 0 on purpose: the whole model fits on the Ti, so we never want KV
# cache spilling onto the 3080 across the slow x4 link (that tanks throughput —
# see the EXL3/llama.cpp Ampere memory). The 3080 (GPU 1) stays free.
#
# Backs the MCP at http://host.docker.internal:11439/v1 (model alias qwen3-8b).
#
# Usage:  pwsh scripts/pool-8b.ps1                 # bring up (3 slots, -c 36864)
#         pwsh scripts/pool-8b.ps1 -Slots 2        # 2 slots -> ~18432 tokens/slot
#         pwsh scripts/pool-8b.ps1 -Ctx 24576      # smaller total context
#         pwsh scripts/pool-8b.ps1 -Down           # tear down
param(
    [switch]$Down,
    [int]$Slots = 3,
    [int]$Ctx = 36864
)

$name  = "dc-qwen8b-pool"
$port  = 11439
$model = "/models/Qwen3-8B-Q4_K_M.gguf"
$image = "ghcr.io/ggml-org/llama.cpp:server-cuda"
$mount = "C:\Users\mail\.ai\llm:/models"

if ($Down) {
    docker rm -f $name 2>$null | Out-Null
    "pool torn down"
    return
}

docker rm -f $name 2>$null | Out-Null
# `--gpus all` exposes both cards; CUDA_VISIBLE_DEVICES=0 restricts llama.cpp to the
# 3080 Ti so the model is single-card (no tensor-split, no cross-card PCIe traffic).
docker run -d --name $name --gpus all `
    -e CUDA_VISIBLE_DEVICES=0 `
    -p "$($port):8080" `
    -v $mount `
    $image `
    -m $model -ngl 99 -c $Ctx -np $Slots --cont-batching --jinja `
    --host 0.0.0.0 --port 8080 --alias qwen3-8b | Out-Null
$perSlot = [math]::Floor($Ctx / $Slots)
"launched $name on :$port (GPU 0, $Slots slots, -c $Ctx = $perSlot tokens/slot)"

"`nwaiting for the server to serve..."
$ok = $false
foreach ($n in 1..90) {
    try { Invoke-RestMethod "http://localhost:$port/v1/models" -TimeoutSec 2 | Out-Null; $ok = $true; break }
    catch {
        if ((docker inspect $name --format "{{.State.Status}}" 2>$null) -eq "exited") { break }
        Start-Sleep -Seconds 1
    }
}
"$name: " + $(if ($ok) { "READY on :$port (alias qwen3-8b, 3 parallel slots)" } else { "FAILED"; docker logs $name --tail 12 })
"`n=== VRAM ==="
nvidia-smi --query-gpu=index,memory.used,memory.total --format=csv,noheader
