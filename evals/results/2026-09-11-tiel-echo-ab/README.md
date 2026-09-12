# A/B: does the after-edit echo shorten Tiel's runs?

Commit `d2cd8a2`, tiel-coder-35b @ 11436. 17 rungs x 3 repeats x 2 arms = 102
runs. Identical binary, identical seeds; the ONLY difference is
`SC_NO_EDIT_ECHO=1`.

Control verified before spending the GPU: on `engine-grid-scan`, OFF produced 0
echoes in 8 turns and ON produced 3 in 9. Across the full run, ON fired **81**
echoes and OFF fired **0** -- the arms genuinely differed.

## Result: the echo is exonerated

| metric | ON (echo) | OFF | prediction |
|---|---|---|---|
| median turns | **4.0** | **4.0** | wrong |
| mean turns | 4.90 | 5.02 | wrong |
| pass rate | 51/51 | 51/51 | held (no headroom) |
| faults | 2 truncated, 1 misfire | 3 truncated, 1 misfire | held |

I predicted, in writing before the run: *if the echo caused the 6.0 -> 4.0 turn
drop, the OFF arm climbs back toward 6.* **It did not move at all.** A 0.12-turn
difference in the means across 51 runs per arm is noise.

## So the turn improvement has NO identified cause

The tiel full-17 at this commit scored median 4.0 turns against a historical
baseline of median 6.0 over 488 runs. That looked like a win. It is not
attributable to anything shipped today:

* three of the four trace-analysis fixes -- the revisit clause, the
  already-applied anchor message, and the sole-whole-writer exemption -- fired
  **zero** times in 252 tiel turns;
* the fourth, the after-edit echo, fired often (85) and is now ruled out by this
  A/B;
* the identical-pair guard fired 3 times, far too few to move a median.

The remaining candidates are drift across the many commits between the baseline
rows (newest: `35c438b`) and now, or a difference in how those older rows were
collected. Attributing the drop without a bisect would be a guess.

## Second independent A/B agreeing

The mellum A/B (2026-09-11, `evals/results/2026-09-11-echo-ab/`) found the same
thing from the other direction: ON 30.5 vs OFF 34.0 median turns, also inside
noise, with the prediction wrong in the same way. Two models, two controlled
arms, no turn effect.

The echo does inflate the prompt -- +17% peak on mellum -- so its cost is real
and its benefit is unmeasured. It stays because it is not harmful and it targets
a real measured failure (13 of 48 missed anchors on `engine-diagonal-wired`
followed the model's own successful edits), but **it should not be cited as a
turn-count improvement.**
