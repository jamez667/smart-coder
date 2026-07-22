# Plan — the plan-first conversational IDE (sc-win pivot)

## The shift

`sc-win` today is a **task-runner GUI**: type intent → `⚒ iterate` → the agent works
autonomously → stops. That's a CLI/batch model wearing a window.

The new model is a **plan-first conversational IDE**: you and the agent *talk*, fast and
dynamic, to build up the **plan and architecture** — and **the plan is real files**
(`README.md`, `TODO.md`), never ephemeral. It is neither fire-and-forget agentic nor
code-first; it's *active participation in the plan*. Code editing (the existing iterate
loop) is the back half you invoke deliberately once the plan is agreed.

## Two modes, auto-detected when a folder opens

1. **Start from scratch** — the folder has no `README.md` and no `TODO.md` (an empty or
   near-empty project). The agent opens by asking **"what do you want to build?"** and you
   converse back and forth; the agent drafts and iteratively refines **`README.md`** (the
   what/architecture) and **`TODO.md`** (the backlog) *as files on disk*.
2. **Existing project** — a `README.md` and/or `TODO.md` exists. The agent **auto-reads
   them**, surfaces the TODO (where you left off), and you continue the conversation from
   there — refining the plan, picking the next thing, or asking questions about the code.

Detection is a cheap filesystem check on open (reuse `find_readme` / `find_todo_file`).

## The conversation engine (the real new primitive)

`sc-core`'s agent loop is one-shot; it is NOT a chat session. But `sc-model` already has
the primitive we need:

```
GenerateRequest::new(vec![Message::system(..), Message::user(..), Message::assistant(..)])
  → backend.generate(req) → GenerateResponse { content }
```

So a **multi-turn chat is just a growing `Vec<Message>`**. New host-testable type in
`sc-win`:

- `crates/sc-win/src/chat.rs` — `Conversation`:
  - holds `Vec<sc_model::Message>` (system + running user/assistant turns),
  - a **mode-shaped system prompt** (scratch vs existing; injects the current README/TODO
    contents so the model always plans against the real files),
  - `fn user_turn(&mut self, text: &str)` appends a user message,
  - `fn request(&self) -> GenerateRequest` builds the request (bounded history — keep the
    system prompt + last N turns so a small model's window doesn't overflow),
  - `fn record_reply(&mut self, content: &str)` appends the assistant turn.
  - Pure/host-testable: no backend call inside (the worker does that), no iced types.

The **system prompt** is the design lever. It tells the model: you are a planning partner;
keep replies short and fast; when the plan changes, output the new `README.md` / `TODO.md`
content in a fenced block tagged with the filename; do NOT write source code yet — we are
planning. Plan-file blocks are extracted and offered to apply.

## Plans are files (not ether)

When the assistant proposes plan content, it emits a fenced block:

    ```file:TODO.md
    - [ ] add lakes
    - [ ] rail + stations
    ```

`chat.rs` parses these `file:<name>` blocks out of a reply (host-tested). The app shows the
proposed file in the **right-hand code view** (auto-open the relevant file — matches "code
view auto-opens the relevant file") and applies it to disk on the user's confirm (a light,
fast **Apply** — this is a plan file, low-stakes, but still explicit). Committing is the
user's choice (we never auto-commit; maybe a later "commit plan" affordance).

This keeps README/TODO as the single source of truth, and the existing welcome/TODO readout
keeps working because it reads those same files.

## Worker threading (non-blocking, like `Session`)

`backend.generate` is blocking + slow; it must not run on the UI thread. Mirror the existing
`Session` pattern:

- `ChatSession::spawn(cfg, conversation_snapshot)` runs `generate` on a worker thread and
  streams the reply back over an `mpsc` channel (whole reply for v1; token streaming is a
  later nicety). The UI drains it each tick exactly like `Session`.
- While a reply is in flight the composer shows "thinking…"; the app stays responsive.

## Layout changes

- **Middle column → the chat window**: a message thread (you ⟷ agent), the composer docked
  at the bottom (already there). Assistant plan-file proposals render with an **Apply** button.
- **Right column → code view**: unchanged, but now also **auto-opens the plan file** the
  conversation is touching (README/TODO), and still follows the agent during an iterate run.
- **Bottom area → tabbed**: **Activity · Verification · Build** become **three tabs** in one
  bottom panel (not stacked). Activity = the live agent/tool log (from an iterate run);
  Verification = verify output; Build = last run outcome. Default tab: Activity.
- The **`⚒ iterate`** action stays but becomes the *"now do the work"* button — invoked once
  the plan is agreed, not the only thing you can do. The primary interaction is the chat.

## Sequencing (each step compiles + gates green; you review between)

1. **`chat.rs` — `Conversation`** (mode-shaped system prompt, bounded history, `file:` block
   extraction). TDD: mode selection, prompt contains README/TODO, block parsing, history cap.
2. **`ChatSession`** worker (spawn/stream/drain), mirroring `Session`. Host-test the
   spawn-yields-terminal-event path against an unreachable backend (as `Session` does).
3. **Chat in the middle column** + composer routes to the chat (not straight to iterate).
   Assistant messages render; `file:` proposals get an **Apply** button that writes the file.
4. **Two-mode open**: on folder open, detect scratch vs existing, seed the conversation +
   opening assistant message; auto-open README/TODO in the code view.
5. **Tab the bottom** (Activity/Verification/Build) — small state + a tab bar.
6. **Keep iterate** as the explicit execute action; wire "apply this plan / do this TODO
   item" from chat into the existing `RunKind::Iterate`.
7. **Gate + live-test** the conversation against the real game (existing) and an empty temp
   dir (scratch) on the running 8B backend.

## Honest risks

- **Small-model chat coherence.** An 8B holding a multi-turn planning conversation is the
  real unknown. Bounded history + a tight system prompt + plans-as-files (so state lives on
  disk, not only in the context window) are the mitigations. If it drifts, the plan files are
  the anchor we can always re-inject.
- **Scope.** This is a genuine restructure of the app's interaction model, not a tweak. It's
  the right direction for "replace Claude for iterating," but it's the biggest single piece
  so far — sequenced above so each step is usable and gated.

## Out of scope for this pass

- Token-by-token streaming (whole-reply first; stream later).
- Auto-commit (user commits; maybe a later button).
- The PR-style diff view + steer-comments (still queued; lands after the conversation loop).
