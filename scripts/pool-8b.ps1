# Launch the Qwen3-8B pools that back the dumb-coder MCP server: one llama.cpp
# server per GPU, each with N parallel slots (`-np N --cont-batching`). Weights
# load once per server (~4.7GB Q4) and N concurrent requests decode via continuous
# batching — so several MCP coding jobs run at the same time on one card, instead
# of loading the 8B once per job (which wouldn't fit).
#
#   Pool 0 (3080 Ti, GPU 0):  :11439  3 slots  -c 36864  (12288 tokens/slot)
#   Pool 1 (3080,    GPU 1):  :11440  2 slots  -c 32768  (16384 tokens/slot)
#
# => 5 concurrent agents total. The MCP round-robins jobs across both URLs (set
# DC_BASE_URLS to the line this script prints), so both cards are used evenly with
# no external load balancer.
#
# Each pool is pinned to ONE physical card via CUDA_VISIBLE_DEVICES — single-card,
# no tensor-split, no cross-card PCIe traffic (splitting an 8B over the slow x4 link
# tanks throughput — see the EXL3/llama.cpp Ampere memory).
#
# Context sizing (KV cache is linear ~0.21 MiB/token and costs NOTHING in
# throughput, so spend spare VRAM on it):
#   * 3080 Ti is shared with the Windows desktop → leave a ~1.5GB buffer.
#     -c 36864 measured ~10.4GB used / ~1.6GB free. 49152 OOMs.
#   * 3080 has no desktop load → push harder. -c 32768 measured ~9.4GB used.
#
# Usage:  pwsh scripts/pool-8b.ps1          # bring both pools up
#         pwsh scripts/pool-8b.ps1 -Down    # tear both down
param([switch]$Down)

$model = "/models/Qwen3-8B-Q4_K_M.gguf"
$image = "ghcr.io/ggml-org/llama.cpp:server-cuda"
$mount = "C:\Users\mail\.ai\llm:/models"

# name, host-port, GPU index, slots, total context
$pools = @(
    @{ name = "dc-qwen8b-pool";  port = 11439; gpu = 0; slots = 3; ctx = 36864 },
    @{ name = "dc-qwen8b-pool2"; port = 11440; gpu = 1; slots = 2; ctx = 32768 }
)

if ($Down) {
    foreach ($p in $pools) { docker rm -f $p.name 2>$null | Out-Null }
    "pools torn down"
    return
}

foreach ($p in $pools) {
    docker rm -f $p.name 2>$null | Out-Null
    # `--gpus all` exposes both cards; CUDA_VISIBLE_DEVICES restricts llama.cpp to the
    # chosen one so each pool is single-card.
    docker run -d --name $p.name --gpus all `
        -e CUDA_VISIBLE_DEVICES=$($p.gpu) `
        -p "$($p.port):8080" `
        -v $mount `
        $image `
        -m $model -ngl 99 -c $($p.ctx) -np $($p.slots) --cont-batching --jinja `
        --host 0.0.0.0 --port 8080 --alias qwen3-8b | Out-Null
    $perSlot = [math]::Floor($p.ctx / $p.slots)
    "launched $($p.name) on :$($p.port) (GPU $($p.gpu), $($p.slots) slots, -c $($p.ctx) = $perSlot tokens/slot)"
}

"`nwaiting for the servers to serve..."
foreach ($p in $pools) {
    $ok = $false
    foreach ($n in 1..90) {
        try { Invoke-RestMethod "http://localhost:$($p.port)/v1/models" -TimeoutSec 2 | Out-Null; $ok = $true; break }
        catch {
            if ((docker inspect $p.name --format "{{.State.Status}}" 2>$null) -eq "exited") { break }
            Start-Sleep -Seconds 1
        }
    }
    "$($p.name): " + $(if ($ok) { "READY on :$($p.port)" } else { "FAILED"; docker logs $p.name --tail 12 })
}

"`n=== VRAM ==="
nvidia-smi --query-gpu=index,memory.used,memory.free,memory.total --format=csv,noheader

$urls = ($pools | ForEach-Object { "http://host.docker.internal:$($_.port)/v1" }) -join ","
"`nPoint the MCP at both pools with:"
"  DC_BASE_URLS=$urls"
