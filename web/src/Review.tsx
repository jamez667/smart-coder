import { useState } from "react";
import { api, type Me, type ReviewRequest } from "./api";

/// The review list — every request a reviewer may act on.
///
/// The administrator sees all of them; an owner sees their repositories, and the
/// server has already narrowed the list before it arrives here. **The client
/// never filters by repository**: the intersection is done once at identify time
/// precisely so no call site re-derives it and gets it subtly different.
export function ReviewList({
  requests,
  onOpen,
}: {
  requests: ReviewRequest[];
  onOpen: (id: string) => void;
}) {
  if (requests.length === 0) {
    return (
      <>
        <h1>Nothing to review</h1>
        <p>When something is filed and drafted, it appears here.</p>
      </>
    );
  }
  return (
    <>
      <h1>Requests</h1>
      {requests.map((r) => (
        <a
          className="item"
          key={r.id}
          href={`/request/${r.id}`}
          onClick={(e) => {
            e.preventDefault();
            onOpen(r.id);
          }}
        >
          <span className="tag">{r.state}</span> {r.summary}
          <span className="meta"> · {r.repo}</span>
        </a>
      ))}
    </>
  );
}

/// One request, and the decision about it.
///
/// **The document order here is a policy, not a layout preference.** Spec 20:
/// the decision controls sit after the *whole* artifact, so on a phone they are
/// physically below it and cannot be reached without scrolling past what they
/// decide on. `the_decision_comes_after_the_whole_artifact` asserted this in
/// Rust by comparing string offsets; it is asserted in the browser now.
export function ReviewDetail({
  request,
  me,
  onDone,
}: {
  request: ReviewRequest;
  me: Me;
  onDone: (r: ReviewRequest) => void;
}) {
  const [problem, setProblem] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const act = (run: () => Promise<ReviewRequest>) => {
    setBusy(true);
    setProblem(null);
    run()
      .then(onDone)
      .catch((e: Error) => setProblem(e.message))
      .finally(() => setBusy(false));
  };

  return (
    <>
      <h1>{request.summary}</h1>
      <p className="meta">
        <span className="tag">{request.state}</span> {request.kind} ·{" "}
        {request.repo}
      </p>

      {/* **Why nothing has happened yet**, shown only while it is still waiting
          — once something has drafted it, coverage is history rather than a
          diagnosis. The two unserved cases name different fixes on purpose:
          starting a daemon and correcting a repository name are not the same
          problem, and one message for both sends half the operators wrong. */}
      {request.state === "queued" && request.coverage !== "served" && (
        <p className="meta" role="status">
          {request.coverage === "no-daemon-seen"
            ? "Nothing has polled this server recently, so nothing will pick this up. Start a daemon."
            : "Daemons are polling, but none offers this repository. Check the name matches what `queue add-repo` was given."}
        </p>
      )}

      {/* **The bypass is visible rather than hidden.** Hiding it does not remove
          it — it only lets the system believe nobody used one. Replaces
          `the_skip_link_is_visible_rather_than_hidden`. */}
      <a className="skip" href="#decide">
        Skip to the decision
      </a>

      <h2>What was asked for</h2>
      {/* A child, so React renders it as a text node. This is text somebody
          typed on the internet; the server used to escape it and the ban on
          innerHTML is what replaces that. */}
      <pre>{request.text}</pre>

      {request.spec && (
        <>
          <h2>The drafted spec</h2>
          {/* Model-authored. Same rule, and this is the field it exists for. */}
          <pre>{request.spec}</pre>
        </>
      )}

      {request.note && (
        <>
          <h2>Note</h2>
          <pre>{request.note}</pre>
        </>
      )}

      {request.artifact_dir && (
        <p className="meta">It landed in {request.artifact_dir}</p>
      )}

      {problem && <p className="note">{problem}</p>}

      <Decision request={request} me={me} busy={busy} act={act} />
    </>
  );
}

/// The decision controls.
///
/// **Every button is a plain `<button type="submit">` with no class.** That is
/// the property `approve_and_send_back_carry_the_same_weight` asserted by
/// comparing the opening tags byte for byte: a phone UI whose easiest action is
/// a big green button, on an artifact too long to read on that screen, produces
/// rubber-stamp approval — and a rubber-stamped gate is worse than no gate,
/// because the system still reports that a human signed off.
///
/// **Send back comes first in document order**, before accept. Also asserted.
function Decision({
  request,
  me,
  busy,
  act,
}: {
  request: ReviewRequest;
  me: Me;
  busy: boolean;
  act: (run: () => Promise<ReviewRequest>) => void;
}) {
  const [note, setNote] = useState("");

  // A held request is a different decision: release it or discard it. The
  // screener held it for a reason, so this is deliberately not framed as
  // clearing a nag.
  if (request.state === "quarantined") {
    return (
      <div className="decide" id="decide">
        <h2>Your call</h2>
        <form
          onSubmit={(e) => {
            e.preventDefault();
            act(() => api.release(request.id));
          }}
        >
          <button type="submit" disabled={busy}>
            Release it — this is not spam
          </button>
        </form>
        <form
          onSubmit={(e) => {
            e.preventDefault();
            act(() => api.discard(request.id));
          }}
        >
          <button type="submit" disabled={busy}>
            Discard
          </button>
        </form>
        <p className="meta">Leaving it here decides nothing.</p>
      </div>
    );
  }

  return (
    <div className="decide" id="decide">
      <h2>Your call</h2>
      <form
        onSubmit={(e) => {
          e.preventDefault();
          act(() => api.sendBack(request.id, note));
        }}
      >
        <label htmlFor="notes">Send it back — what should change?</label>
        {/* `required` in the markup, not a check in JavaScript. A send-back with
            no reason produces a redraft that repeats itself, and the browser
            refusing an empty field is one fewer thing to remember. */}
        <textarea
          id="notes"
          name="notes"
          required
          value={note}
          onChange={(e) => setNote(e.target.value)}
          placeholder="The redraft grounds on this, so be specific."
        />
        <button type="submit" disabled={busy}>
          Send back
        </button>
      </form>

      {/* **Only the administrator may accept**, and the server enforces it by
          the caller's variant rather than by this flag — an owner reaching the
          endpoint gets a 404 because the verb does not exist for them. This
          only stops the interface drawing a button that would fail. */}
      {me.can.accept && request.spec_digest && (
        <form
          onSubmit={(e) => {
            e.preventDefault();
            // The digest of the spec *as read*. If a redraft landed while this
            // page was open, the server refuses rather than accepting text
            // nobody saw.
            act(() => api.accept(request.id, request.spec_digest!));
          }}
        >
          <button type="submit" disabled={busy}>
            Approve this spec
          </button>
        </form>
      )}

      <form
        onSubmit={(e) => {
          e.preventDefault();
          act(() => api.discard(request.id));
        }}
      >
        <button type="submit" disabled={busy}>
          Discard
        </button>
      </form>
      <p className="meta">
        Leaving this page decides nothing — it will still be here.
      </p>
    </div>
  );
}
