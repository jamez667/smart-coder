# The partial-line anchor fix, measured on the rung it was found in

`rust-symptomatic`, Mellum2-12B on the 3080, three repeats each.

**CORRECTION (later the same day): this rung now PASSES.** On the full 17-rung
run it went green in 32 steps / 189s. I called it a capability wall three times
on the strength of four failures and said no harness change would reach it.
That was wrong — the anchor fix got it over the line, it just needed more steps
than the three-repeat runs below were giving it. The model could find the bug
all along; it could not land the edit.

The cost figures below still stand as a measure of what the fix changed:

| | median secs | steps | median tokens |
| --- | --- | --- | --- |
| 16k ctx, guards only | 379 | 28-33 | 288k |
| 24k ctx, guards only | 286 | 23-29 | 292k |
| **24k + anchor fix** | **172** | **19-20** | **155k** |

Steps collapse from ~30 to a tight 19-20 and tokens nearly halve. The tightness
matters as much as the number: the run now fails for one consistent reason
rather than thrashing.

## What the fix was

`edit_file` rejected an anchor that identified exactly one span, purely over
leading whitespace — the model writes 8 spaces, the file has 12. Verified
against the fixture: 0 exact substring matches, exactly 1 for `old_str.trim()`.
The existing `count == 0` fallback only compared whole-line signatures, so a
sub-expression anchor never matched. **24 of 43 anchor failures across the
transcripts were this one class.**

## And the context theory, which was mine and was wrong

I attributed the earlier 7x Ti-vs-3080 gap to the 16k window. The forensics
disagreed: peak prompt was 9,275 tokens against a 16,384 window, so the window
was never binding. Raising it to 24k confirmed that — 286s against 379s, still
nowhere near the Ti's 31-107s. The gap is generation running to the 3072-token
cap, roughly 19s a turn, not context pressure.

Raising the context to 24k was reasonable housekeeping. It was not the fix, and
I should not have claimed it was before measuring.
