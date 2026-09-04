# Investigate leads — A/B probe (spec 23 M7)

Question: *Can you investigate why on the jump screen the trail behind the stars it thin before it gets thick? it should be the other way around.*

Workspace: void-claim  ·  model: `tiel-coder-35b`  ·  the user's original wording, verbatim

Full transcripts (both arms) at `crates/sc-win/logs/leads-probe.md` (gitignored).
Reproduce with `cargo test -p sc-win --test leads_probe -- --ignored --nocapture`.

## Result

| | answered | steps | tool calls | faults | elapsed |
|---|---|---|---|---|---|
| leads OFF | yes | 10 | 9 | 0 | 59s |
| leads ON | **yes** | **5** | **5** | 0 | **18s** |

**Both arms reached the same correct answer.** Same file, same two lines, same fix:
`draw_trails` in `crates/void_engine/src/fx/starfield.rs` draws `head → mid` with
`width_head` (thin) and `mid → tail` with `width_tail` (thick), where `tail` is the
star itself — so the streak is thin at the back and thick at the star, the opposite of
the comment's stated intent. Swap the two widths.

Leads halved the run: 10 steps to 5, 59s to 18s.

## Why — the OFF arm's first four steps

```
step 1  read_file  crates/void_claim/src/hyperspace.rs          <- does not exist
step 2  read_file  crates/void_claim/src/starfield.rs           <- does not exist
step 3  read_file  crates/void_engine/src/fx/starfield.rs       <- the right file, by luck
step 4  read_file  crates/void_claim/src/starfield.rs@1:5       <- back to the one that does not exist
step 5  search_code draw_trails
step 6  read_file  crates/void_claim/src/ship_render.rs@130:60
step 7  read_file  crates/void_engine/src/fx/starfield.rs@114:73
step 8  read_file  crates/void_engine/src/fx/starfield.rs@114:73  <- identical re-read
step 9  (7455-char reply, no call)
step 10 finish
```

Four of the first eight calls were spent guessing filenames — two of them at paths
that do not exist — and step 8 re-read step 7 byte for byte. This is the behaviour
spec 23's principle describes: *it stumbles on the twenty turns before that*.

## Why — the ON arm

The leads block the model was handed, immediately after the sorted file map:

```
leads (indexed search over your question):
  crates/void_engine/src/fx/starfield.rs:114  fn draw_trails  matched: screen, trail, star, thin, get, thick, way
  crates/void_claim/src/ship_render.rs:25     fn draw_nav_arrow  matched: screen, thick, around
  crates/void_engine/src/fx/starfield.rs:5    (file)  matched: trail
  ...
```

`draw_trails` is the first line, and it was the first file the model opened. No
guessed paths, no re-reads.

## Verdict

**The bar spec 23 set is met**: both arms answered, and ON used fewer steps, with the
sorted map unchanged underneath. The default is nonetheless still **off**
(`SC_INVESTIGATE_LEADS=1` to enable) — flipping it is a human decision, and this is
one probe on one question. This file is the evidence for that decision.
