import { useEffect, useState } from "react";
import { api, ApiError, type FiledRequest, type Me } from "./api";
import { Masthead } from "./Masthead";
import { SignInDialog } from "./SignInDialog";

/// The public surface.
///
/// **What the server rendered as `landing_page`, `public_file_page` and
/// `public_detail`.** One `GET /` used to answer three different pages depending
/// on who asked; here that is one component reading `me.role`, which is the same
/// decision made in the same place — the server still decides, this just draws
/// the answer.
export function App() {
  const [me, setMe] = useState<Me | null>(null);
  const [mine, setMine] = useState<FiledRequest[]>([]);
  const [problem, setProblem] = useState<string | null>(null);
  const [signingIn, setSigningIn] = useState(false);

  useEffect(() => {
    // **`/me` first, always.** Nothing can be drawn before knowing whether
    // anybody is signed in — including the front door a stranger needs.
    api
      .me()
      .then((who) => {
        setMe(who);
        if (who.can.file) {
          return api
            .requests<FiledRequest[]>()
            .then(setMine)
            .catch((e: ApiError) => setProblem(e.message));
        }
      })
      .catch((e: ApiError) => setProblem(e.message));
  }, []);

  if (!me) {
    // Deliberately blank rather than a spinner: on the connection this surface
    // was designed for, a spinner that resolves in 40ms is a flash of noise.
    return <div className="bar-inner" />;
  }

  return (
    <>
      <Masthead me={me} onSignIn={() => setSigningIn(true)} />
      <main>
        {problem && <p className="note">{problem}</p>}
        {me.can.file ? <Filing mine={mine} /> : <Landing />}
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

/// A filer's own surface: the form, and what they have filed.
function Filing({ mine }: { mine: FiledRequest[] }) {
  return (
    <>
      <h1>What needs doing?</h1>
      {mine.length > 0 && (
        <>
          <h2>What you have filed</h2>
          {mine.map((r) => (
            <a className="item" key={r.id} href={`/public/request/${r.id}`}>
              {/* Rendered as a child, so React escapes it. The summary is text
                  somebody typed on the internet; the server used to escape it
                  and this is what replaces that. */}
              <span className="tag">{r.state}</span> {r.summary}
            </a>
          ))}
        </>
      )}
    </>
  );
}
