#!/bin/sh
# What is `cargo test -p sc-eval` actually waiting on?
#
# **A script, not inline YAML.** Three attempts at embedding this in the
# workflow produced broken YAML or mangled line continuations; a file is read by
# the shell and by a person the same way, and `yaml.safe_load` cannot object to
# it.
#
# The problem it exists for: `cargo test -p sc-eval` finishes in ~18 seconds in
# the `reports` job and has never once finished in `check`. Running
# `cargo clippy --workspace --all-targets` first reproduces the hang in
# `reports` too, so the trigger is known -- but `timeout --signal=KILL` does not
# fire on the stuck process, and a separate `CARGO_TARGET_DIR` does not help. A
# process that cannot be SIGKILLed is blocked in the kernel, and the only thing
# that names such a wait is `/proc`.
#
# So: run the tests in the background, and if they are still going after the
# grace period, print what the kernel says about every process in the tree.
# Always exits 0 -- it is an observation, not a gate.

GRACE=${1:-180}

# **Print the machine first.** The full CI sequence -- web build, workspace
# clippy, then this exact command -- passes on a two-core Linux container with
# 7 GB in about ninety seconds. It has never finished on the hosted runner. So
# the difference is the runner, and these three lines are the cheapest way to
# see it: how much disk is left, how much memory, and how many cores.
echo "=== the machine, before anything runs ==="
df -h / /tmp 2>/dev/null
free -m 2>/dev/null
nproc 2>/dev/null
echo "=== target/ size (clippy --workspace --all-targets built into it) ==="
du -sh target 2>/dev/null || echo "no target/"
echo

cargo test -p sc-eval -- --test-threads=1 --nocapture >/tmp/eval.log 2>&1 &
TESTPID=$!

i=0
while [ "$i" -lt "$GRACE" ]; do
    if ! kill -0 "$TESTPID" 2>/dev/null; then
        wait "$TESTPID"
        echo "=== finished rc=$? after ${i}s ==="
        tail -25 /tmp/eval.log
        exit 0
    fi
    sleep 5
    i=$((i + 5))
done

echo "=== STILL RUNNING AFTER ${GRACE}s ==="

echo "--- partial test output (the last line is the test that never returned) ---"
tail -25 /tmp/eval.log

echo "--- every thread, with state and kernel wait channel ---"
ps -eLo pid,tid,ppid,stat,wchan:30,etimes,comm 2>/dev/null | head -60

echo "--- D state: uninterruptible, which is what SIGKILL cannot touch ---"
ps -eLo pid,tid,stat,wchan:30,comm 2>/dev/null | awk '$3 ~ /D/'

echo "--- kernel stacks and open files for the test tree ---"
for d in /proc/[0-9]*; do
    pid=${d#/proc/}
    comm=$(cat "$d/comm" 2>/dev/null)
    case "$comm" in
        sc_eval* | cargo | ladder* | rustc | sh | sleep)
            state=$(grep '^State:' "$d/status" 2>/dev/null)
            echo "== pid $pid comm=$comm $state"
            echo "   wchan: $(cat "$d/wchan" 2>/dev/null)"
            for t in "$d"/task/*; do
                [ -r "$t/stack" ] || continue
                echo "   tid ${t##*/} stack:"
                head -8 "$t/stack" 2>/dev/null | sed 's/^/     /'
            done
            echo "   open fds:"
            ls -l "$d/fd" 2>/dev/null | head -12 | sed 's/^/     /'
            ;;
    esac
done

echo "--- disk and memory, in case the wait is on either ---"
df -h 2>/dev/null | head -6
free -m 2>/dev/null

kill -9 "$TESTPID" 2>/dev/null
exit 0
