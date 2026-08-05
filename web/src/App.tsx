import { useCallback, useEffect, useState } from "react";
import {
  api,
  ApiError,
  type FiledRequest,
  type Me,
  type ReviewRequest,
} from "./api";
import { Masthead } from "./Masthead";
import { SignInDialog } from "./SignInDialog";
import { ReviewDetail, ReviewList } from "./Review";
import { Accounts, Daemons, Owners, Repos, Settings } from "./Admin";

/// The whole interface.
///
/// **One `GET /` answered three different pages depending on who asked**, and
/// that has not changed — the server still decides, by returning a different
/// `me` and a different shape of request. This draws the answer.
///
/// Routing is `history.pushState` over `window.location.pathname`, with no
/// router library: there are four addresses. A router would be a dependency and
/// a concept in service of a `switch`.
export function App() {
  const [me, setMe] = useState<Me | null>(null);
  const [mine, setMine] = useState<FiledRequest[]>([]);
  const [review, setReview] = useState<ReviewRequest[]>([]);
  const [open, setOpen] = useState<ReviewRequest | null>(null);
  const [problem, setProblem] = useState<string | null>(null);
  const [signingIn, setSigningIn] = useState(false);
  const [path, setPath] = useState(window.location.pathname);

  // The back button has to work. Without this it silently does nothing, which
  // is the failure people notice first and complain about last.
  useEffect(() => {
    const pop = () => setPath(window.location.pathname);
    window.addEventListener("popstate", pop);
    return () => window.removeEventListener("popstate", pop);
  }, []);

  const go = useCallback((to: string) => {
    window.history.pushState({}, "", to);
    setPath(to);
  }, []);

  const load = useCallback(async () => {
    const who = await api.me();
    setMe(who);
    if (who.can.review) {
      setReview(await api.requests<ReviewRequest[]>());
    } else if (who.can.file) {
      setMine(await api.requests<FiledRequest[]>());
    }
  }, []);

  useEffect(() => {
    // **`/me` first, always.** Nothing can be drawn before knowing whether
    // anybody is signed in — including the front door a stranger needs.
    load().catch((e: ApiError) => setProblem(e.message));
  }, [load]);

  // A request opened by id: fetch it rather than reusing the list entry, because
  // the list carries a summary and the detail carries the spec and its digest.
  const openRequest = useCallback(
    (id: string) => {
      go(`/request/${id}`);
      api
        .request<ReviewRequest>(id)
        .then(setOpen)
        .catch((e: ApiError) => setProblem(e.message));
    },
    [go],
  );

  useEffect(() => {
    const m = path.match(/^\/request\/([^/]+)$/);
    if (m && !open) {
      api
        .request<ReviewRequest>(m[1])
        .then(setOpen)
        .catch((e: ApiError) => setProblem(e.message));
    }
    if (!m && open) setOpen(null);
  }, [path, open]);

  // The administrative pages, chosen by path. **Gated on the capability the
  // server sent**, not on the path alone: a filer who types `/settings` gets the
  // landing page, and the endpoints behind it would 404 for them anyway.
  const admin = me?.can.administer
    ? {
        "/settings": <Settings />,
        "/owners": <Owners />,
        "/repos": <Repos />,
        "/daemons": <Daemons />,
        "/accounts": <Accounts />,
      }[path]
    : undefined;

  // **What the interface decided, readable from a test.** The admin pages
  // failed in CI drawing the review list, and nothing on the page could say
  // whether that was the path, the capability, or the order they were evaluated
  // in. Three CI round trips went on guessing between them.
  //
  // Cheap enough to leave: two strings on the document element.
  if (typeof document !== "undefined") {
    document.documentElement.dataset.scPath = path;
    document.documentElement.dataset.scRole = me?.role ?? "loading";
  }

  if (!me) {
    // Deliberately blank rather than a spinner: on the connection this surface
    // was designed for, a spinner that resolves in 40ms is a flash of noise.
    return <div className="bar-inner" />;
  }

  return (
    <>
      <Masthead me={me} onSignIn={() => setSigningIn(true)} onGo={go} />
      <main>
        {problem && <p className="note">{problem}</p>}
        {admin ? (
          admin
        ) : open ? (
          <ReviewDetail
            request={open}
            me={me}
            onDone={(r) => {
              setOpen(r);
              load().catch(() => undefined);
            }}
          />
        ) : me.can.review ? (
          <ReviewList requests={review} onOpen={openRequest} />
        ) : me.can.file ? (
          <Filing mine={mine} />
        ) : (
          <Landing />
        )}
      </main>
      <footer className="bar footer">
        <div className="bar-inner">
          <p>Smart Coder — ask for a change, get a spec back.</p>
        </div>
      </footer>
      {signingIn && <SignInDialog onClose={() => setSigningIn(false)} />}
    </>
  );
}

/// What a stranger sees.
function Landing() {
  return (
    <>
      <h1>Ask for a change — get a spec back.</h1>
      <p>
        Describe what needs doing in your own words. It comes back as a written
        specification for the developer to read, approve, or send back for
        another pass.
      </p>
      <section className="point">
        <h2>Say it plainly</h2>
        <p>
          No issue templates and no jargon. A sentence or two about what is wrong
          or what you want is enough to start.
        </p>
      </section>
      <section className="point">
        <h2>A spec, not a ticket</h2>
        <p>
          What comes back is a written specification grounded in the actual code,
          not a restatement of what you asked for.
        </p>
      </section>
      <section className="point">
        <h2>Somebody reads it</h2>
        <p>
          Nothing is built until a person approves the spec. The gate is a human
          one, and it stays that way.
        </p>
      </section>
    </>
  );
}

/// A filer's own surface: what they have filed.
function Filing({ mine }: { mine: FiledRequest[] }) {
  return (
    <>
      <h1>What needs doing?</h1>
      {mine.length > 0 && (
        <>
          <h2>What you have filed</h2>
          {mine.map((r) => (
            <a className="item" key={r.id} href={`/public/request/${r.id}`}>
              <span className="tag">{r.state}</span> {r.summary}
            </a>
          ))}
        </>
      )}
    </>
  );
}
