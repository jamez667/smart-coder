import { useCallback, useEffect, useState, type ReactNode } from "react";
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
import { Setup } from "./Setup";
import { useStrings } from "./strings";

/// The whole interface.
///
/// **One `GET /` answered three different pages depending on who asked**, and
/// that has not changed — the server still decides, by returning a different
/// `me` and a different shape of request. This draws the answer.
///
/// Routing is `history.pushState` over `window.location.pathname`, with no
/// router library: there are four addresses. A router would be a dependency and
/// a concept in service of a `switch`.
/// What the masthead is told before `/me` has answered.
///
/// The wizard renders before that request lands, and a masthead needs *some*
/// caller. A stranger is the safe one: it draws a sign-in button and names
/// nothing.
const ANONYMOUS: Me = {
  role: "anonymous",
  can: { file: false, review: false, accept: false, administer: false },
};

export function App() {
  const s = useStrings();
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

  // **An address this interface does not know says so.**
  //
  // The server answers its own 404 for anything outside the set it serves the
  // document for — but inside that set the client decides, and it used to fall
  // through to whatever the caller's role renders by default. So `/nonsense`
  // showed a signed-in reviewer their review list, with the address bar saying
  // something else entirely: the one failure mode worse than a 404, because it
  // looks like it worked.
  //
  // Listed rather than inferred. A regex over `/request/...` would also match
  // `/requests` — which is now a real address of its own, so that collision is
  // no longer hypothetical: the `^\/(public\/)?request\/[^/]+$` below requires
  // the slash and an id precisely so the two cannot be confused.
  //
  // **This must match `wants_document` on the server exactly.** The server
  // decides which addresses get this bundle at all; a path it serves and this
  // does not is a working address showing "Not found", and a path this claims
  // and it does not can never arrive. Note what is deliberately absent:
  // `/public/signin/{token}` is the magic-link landing and is still rendered by
  // the server, because it is a navigation out of an email rather than a route
  // within the application.
  const known =
    path === "/" ||
    path === "/public" ||
    path === "/public/signin" ||
    // **One address for "the requests I may see".** It resolves to the
    // reviewer's queue, the filer's own list, or a prompt to sign in — chosen
    // from what `/me` returned, exactly as `/` already was. The masthead can
    // therefore carry one link for every caller instead of an address that
    // changes with the reader's role.
    path === "/requests" ||
    path === "/review" ||
    path === "/setup" ||
    path === "/settings" ||
    path === "/owners" ||
    path === "/repos" ||
    path === "/daemons" ||
    path === "/accounts" ||
    /^\/(public\/)?request\/[^/]+$/.test(path);

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

  // **The wizard, before anything else.** An unclaimed server has no
  // administrator to sign in as, so this is the only thing a visitor to
  // `/setup` can usefully be shown.
  if (path === "/setup") {
    return (
      <>
        <Masthead me={me ?? ANONYMOUS} onSignIn={() => setSigningIn(true)} onGo={go} path={path} />
        <main>
          <Setup
            onClaimed={() => {
              // Claimed and signed in: reload rather than route, so `/me` is
              // asked again and the whole interface comes back as the
              // administrator.
              window.location.assign("/review");
            }}
          />
        </main>
      </>
    );
  }

  if (!me) {
    // Deliberately blank rather than a spinner: on the connection this surface
    // was designed for, a spinner that resolves in 40ms is a flash of noise.
    return <div className="bar-inner" />;
  }

  return (
    <>
      <Masthead me={me} onSignIn={() => setSigningIn(true)} onGo={go} path={path} />
      <main>
        {problem && <p className="note">{problem}</p>}
        {!known ? (
          <NotFound />
        ) : admin ? (
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
        ) : /* **`/` is the marketing page for everybody now**, and that is the
              change. It used to be the landing page *only* for a caller who
              could do nothing — a reviewer who typed `/` got their queue, so
              the page describing the product was unreachable the moment you
              had an account, and the one person most likely to send somebody a
              link to it could not see what they were sending.
              The surfaces it used to stand in for moved to `/requests`, which
              the masthead links to. */
        path === "/" ? (
          <Landing me={me} onGo={go} onSignIn={() => setSigningIn(true)} />
        ) : me.can.review ? (
          <ReviewList requests={review} onOpen={openRequest} />
        ) : me.can.file ? (
          <Filing mine={mine} me={me} onFiled={() => load().catch(() => undefined)} />
        ) : (
          /* `/requests` reached by somebody who can neither file nor review.
             Not a 404 — the address is real and what they are missing is a
             session, which is one click away. */
          <RequestsAnonymous onSignIn={() => setSigningIn(true)} />
        )}
      </main>
      <footer className="bar footer">
        <div className="bar-inner">
          {/* **The brand and the tagline, joined here.** This used to be one
              hardcoded sentence, which put the product name back inside a
              translatable string after the catalogue had deliberately split it
              out — so the footer and the masthead could come to call the same
              site two different things. The catalogue holds a name and a
              phrase; putting them together is the renderer's job. */}
          <p>
            {s.brand}
            {s.footer_tagline_app}
          </p>
        </div>
      </footer>
      {signingIn && <SignInDialog onClose={() => setSigningIn(false)} />}
    </>
  );
}

//// The page that says what the product is — **shown to everybody**.
///
/// It used to be what a caller with no capabilities fell through to, which made
/// it a page for strangers by accident rather than by design: anybody with an
/// account got their own surface at `/` and could not reach this at all. So the
/// person best placed to send a colleague a link to the product had never seen
/// the page they were linking to, and nothing on it acknowledged a reader who
/// was already signed in.
///
/// **It argues for the product, not for this queue.** The first version of this
/// page read "ask *us* for a change" — which described this deployment rather
/// than the thing being deployed, and left a reader thinking Smart Coder was a
/// suggestion box for somebody else's project. What it is for is a company
/// running their own: their repositories, their stakeholders, their machine
/// drafting the specs. This deployment is then a *demonstration* of that, and
/// says so in its own band rather than being silently mistaken for the point.
///
/// **What differs when signed in is the call to action and nothing else.** Two
/// versions of the argument would be two things to keep true; the argument is
/// the same either way, and only the next step changes — a stranger needs to
/// sign in, a filer needs the door to their own requests.
function Landing({
  me,
  onGo,
  onSignIn,
}: {
  me: Me;
  onGo: (to: string) => void;
  onSignIn: () => void;
}) {
  const s = useStrings();
  const known = me.role !== "anonymous";
  // The same control in three places, so it is built once. Written inline three
  // times in the first draft, which is three places to forget the modifier-key
  // check that keeps "open in a new tab" working.
  const cta = (
    <Cta known={known} onGo={onGo} onSignIn={onSignIn} label={known ? s.landing_cta_signed_in : s.landing_cta} />
  );
  return (
    <>
      {/* **The hero breaks the text column.** `main` is capped at 45rem, which
          is a measure chosen for reading prose — right for a spec, too narrow
          for a page whose first screen is the whole argument. `.wide` widens to
          the 60rem the masthead and footer already use, so the hero lines up
          with the bar above it instead of sitting inside it. */}
      <section className="hero wide">
        {/* **The licence above the headline, not below the buttons.** For
            something a reader is being asked to run on their own
            infrastructure, what it costs and whether they can see inside it is
            the question — and said *after* the call to action it arrives too
            late to remove the objection that stopped them clicking. Sitting it
            beside the eyebrow also spares the hero a third stacked line under
            the button, which was diluting the one thing that should be there. */}
        <p className="eyebrow-row">
          <span className="eyebrow">{s.landing_eyebrow}</span>
          <span className="badge">{s.landing_oss_badge}</span>
        </p>
        <h1>{s.landing_headline}</h1>
        <p className="lede">{s.landing_sub}</p>
        <p className="cta-row">{cta}</p>
        {/* The objection answered before it is raised — and only worth saying
            to somebody who has not already signed in. */}
        {!known && <p className="cta-note">{s.landing_cta_note}</p>}
      </section>

      {/* Who is being spoken to, named before the argument goes on. A reader who
          has mistaken the audience reads every following section wrongly. */}
      <section className="wide band">
        <h2 className="band-heading">{s.landing_for_heading}</h2>
        <p className="band-lede">{s.landing_for_body}</p>
        {/* **Two audiences, side by side.** The product has two and they want
            opposite things — the people filing want to be heard without
            learning a tracker, the team receiving wants the noise to arrive as
            something they can act on. Putting them in one column would make one
            of them read as an afterthought. */}
        <div className="sides">
          <Card title={s.landing_side_filers_title} body={s.landing_side_filers_body} />
          <Card title={s.landing_side_team_title} body={s.landing_side_team_body} />
        </div>
      </section>

      <section className="wide band">
        <h2 className="band-heading">{s.landing_how_heading}</h2>
        {/* **An ordered list, because the order is the content.** These are
            three things that happen one after another; a screen reader gets
            "1 of 3" from the element rather than from the numerals, which are
            drawn by CSS and hidden from it. */}
        <ol className="steps">
          <Step n={1} title={s.landing_step_1_title} body={s.landing_step_1_body} />
          <Step n={2} title={s.landing_step_2_title} body={s.landing_step_2_body} />
          <Step n={3} title={s.landing_step_3_title} body={s.landing_step_3_body} />
        </ol>
      </section>

      <section className="wide band">
        <h2 className="band-heading">{s.landing_points_heading}</h2>
        {/* The three points, rewritten for the new argument and kept as cards.
            The first of them — a spec and not a pull request — is the boundary
            the whole product is built around, so it leads. */}
        <div className="cards">
          <Card title={s.landing_point_1_title} body={s.landing_point_1_body} />
          <Card title={s.landing_point_2_title} body={s.landing_point_2_body} />
          <Card title={s.landing_point_3_title} body={s.landing_point_3_body} />
        </div>
      </section>

      {/* **Where it runs, in its own band.** The strongest honest claim the
          product has, and the first thing a buyer checks: the intake server
          links no repository, no filesystem and no model, and the machine that
          drafts dials out rather than listening. */}
      <section className="wide band">
        <h2 className="band-heading">{s.landing_host_heading}</h2>
        <p className="band-lede">{s.landing_host_body}</p>
        <div className="cards">
          <Card title={s.landing_host_1_title} body={s.landing_host_1_body} />
          <Card title={s.landing_host_2_title} body={s.landing_host_2_body} />
          <Card title={s.landing_host_3_title} body={s.landing_host_3_body} />
        </div>
      </section>

      {/* Free and open source, at length, for the reader who has decided to
          care. The badge in the hero is the half-second version. */}
      <section className="wide band">
        <h2 className="band-heading">{s.landing_oss_heading}</h2>
        <p className="band-lede">{s.landing_oss_body}</p>
      </section>

      {/* **The two ways to run it, and what separates them.** Open core: the
          code is MIT in both, and what a subscription buys is not a feature but
          the not-running-it. Presenting them as "free tier / pro tier" would be
          selling the software back to somebody who already has all of it, so
          the heading says *run* and the lede says so outright.

          The note under both is the load-bearing line: the daemon is on the
          customer's hardware either way, which is why a hosted inbox can be
          offered without retracting the section above it. */}
      <section className="wide band">
        <h2 className="band-heading">{s.landing_tiers_heading}</h2>
        <p className="band-lede">{s.landing_tiers_body}</p>
        <div className="sides">
          <Tier
            title={s.landing_tier_self_title}
            tag={s.landing_tier_self_tag}
            body={s.landing_tier_self_body}
            cta={
              // **A real navigation, so a real anchor** — this one leaves the
              // site entirely, which is exactly the case the routed links are
              // not. `rel="noreferrer"` because there is nothing the
              // destination needs to learn about where the reader came from.
              <a className="btn" href={SOURCE_URL} target="_blank" rel="noreferrer">
                {s.landing_tier_self_cta}
              </a>
            }
          />
          <Tier
            title={s.landing_tier_hosted_title}
            tag={s.landing_tier_hosted_tag}
            // **Marked as not yet available, in the same glance as the name.**
            // A reader who takes in the description first and the availability
            // second has been misled for the length of a paragraph — so the tag
            // is styled differently from the free one rather than identically.
            pending
            body={s.landing_tier_hosted_body}
            cta={
              // Registering interest is a GitHub issue, because that exists
              // today. A form here would need an endpoint, a store and a
              // mailer that this server deliberately does not have — and a
              // button that silently drops what somebody typed is worse than
              // one that hands them somewhere real.
              <a className="btn" href={INTEREST_URL} target="_blank" rel="noreferrer">
                {s.landing_tier_hosted_cta}
              </a>
            }
          />
        </div>
        <p className="tiers-note">{s.landing_tiers_note}</p>
      </section>

      {/* **The demonstration, named as one.** This deployment collects for the
          product's own repository, which makes it a working example and *not*
          how anybody else's requests would be handled. A reader who files here
          expecting support for their own project has been misled by the page,
          so the note says which it is before the button invites them in. */}
      <section className="demo wide">
        <h2>{s.landing_demo_heading}</h2>
        <p>{s.landing_demo_body}</p>
        <p className="cta-row">
          <Cta known={known} onGo={onGo} onSignIn={onSignIn} label={known ? s.landing_cta_signed_in : s.landing_demo_cta} />
        </p>
        <p className="cta-note">{s.landing_demo_note}</p>
      </section>

      <section className="closer wide">
        <h2>{s.landing_close_heading}</h2>
        <p>{s.landing_close_body}</p>
        <p className="cta-row">{cta}</p>
      </section>
    </>
  );
}

/// Where the source lives.
///
/// **A constant rather than a catalogue string.** A URL is not prose: it is
/// identical in every language, and the catalogue forbids markup and is tested
/// for it — a translated link is an injection point maintained by whoever last
/// edited the translation. The same reasoning already keeps repository names and
/// email addresses out of it.
const SOURCE_URL = "https://github.com/jamez667/smart-coder";

/// Where "register interest" goes.
///
/// **A GitHub issue, because that exists today.** A form on this page would
/// need an endpoint, somewhere to keep what it collected and a way to mail
/// about it — none of which this server has, and adding them for a service that
/// does not exist yet is building the wrong thing first. A button that silently
/// drops what somebody typed is worse than one that hands them somewhere real.
const INTEREST_URL =
  "https://github.com/jamez667/smart-coder/issues/new?title=Hosted%20intake%3A%20interest&labels=hosted";

/// The page's call to action, wherever it appears.
///
/// **A button for a stranger and a link for a caller we know**, because they do
/// different things: signing in opens a `<dialog>` on this page, while a known
/// caller is being sent to another address. An anchor that goes nowhere would be
/// announcing a destination that does not exist, and a button that navigates
/// loses middle-click and "open in new tab".
function Cta({
  known,
  label,
  onGo,
  onSignIn,
}: {
  known: boolean;
  label: string;
  onGo: (to: string) => void;
  onSignIn: () => void;
}) {
  if (!known) {
    return (
      <button className="btn primary" type="button" onClick={onSignIn}>
        {label}
      </button>
    );
  }
  return (
    <a
      className="btn primary"
      href="/requests"
      onClick={(e) => {
        // Anything but a plain left click belongs to the browser, so opening in
        // a new tab still works.
        if (e.metaKey || e.ctrlKey || e.shiftKey || e.button !== 0) return;
        e.preventDefault();
        onGo("/requests");
      }}
    >
      {label}
    </a>
  );
}

// One numbered step.
///
/// The numeral is an attribute rather than text in the markup: CSS draws it, so
/// a screen reader hears the list's own position instead of hearing the number
/// twice.
function Step({ n, title, body }: { n: number; title: string; body: string }) {
  return (
    <li className="step">
      <span className="step-n" aria-hidden="true">
        {n}
      </span>
      <div>
        <h3>{title}</h3>
        <p>{body}</p>
      </div>
    </li>
  );
}

/// One of the two ways to run it.
///
/// A card with a tag and an action. **`pending` is a separate prop rather than
/// a second component** — the two tiers are the same shape and differ in one
/// fact, and a copy of this whose only change is a class name is a copy that
/// drifts.
function Tier({
  title,
  tag,
  body,
  cta,
  pending,
}: {
  title: string;
  tag: string;
  body: string;
  cta: ReactNode;
  /// Not available yet. Styles the tag as a caution rather than a fact, and is
  /// the only difference between the two cards.
  pending?: boolean;
}) {
  return (
    <section className={pending ? "card tier pending" : "card tier"}>
      <header className="tier-head">
        <h3>{title}</h3>
        <span className="tier-tag">{tag}</span>
      </header>
      <p>{body}</p>
      <p className="cta-row">{cta}</p>
    </section>
  );
}

/// One of the three points, as a card.
function Card({ title, body }: { title: string; body: string }) {
  return (
    <section className="card">
      <h3>{title}</h3>
      <p>{body}</p>
    </section>
  );
}

/// `/requests`, reached by somebody the server does not know.
///
/// **Not a 404 and not a redirect.** The address is real, the masthead links to
/// it from every page, and what is missing is a session — so it says which of
/// those it is and puts the fix on the same screen. A 404 here would read as a
/// broken link in the site's own navigation bar.
function RequestsAnonymous({ onSignIn }: { onSignIn: () => void }) {
  const s = useStrings();
  return (
    <>
      <h1>{s.requests_anon_title}</h1>
      <p>{s.requests_anon_body}</p>
      <p>
        <button className="btn primary" type="button" onClick={onSignIn}>
          {s.nav_signin}
        </button>
      </p>
    </>
  );
}

/// An address the interface does not serve.
///
/// Says the same thing the server's own 404 says, because a reader who followed
/// a stale link should not be able to tell which of the two answered — the
/// distinction is about who routed, and that is not their problem.
function NotFound() {
  const s = useStrings();
  return (
    <>
      <h1>{s.app_not_found_title}</h1>
      <p>{s.app_not_found_body}</p>
      <p className="meta">
        <a href="/">{s.app_not_found_link}</a>
      </p>
    </>
  );
}

/// A filer's own surface: somewhere to say what they need, and what they said.
///
/// **The repository list comes from `/me`.** A client cannot invent it — the
/// server refuses a name it does not serve — and with exactly one configured
/// there is no field at all, because a choice of one is not a choice.
function Filing({
  mine,
  me,
  onFiled,
}: {
  mine: FiledRequest[];
  me: Me;
  onFiled: () => void;
}) {
  const s = useStrings();
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState("");
  const [filed, setFiled] = useState(false);
  const repos = me.repos ?? [];

  if (repos.length === 0) {
    // Not a failure — a surface with nothing enabled. Saying so beats a form
    // whose every submission would be refused.
    return (
      <>
        <h1>{s.filing_none_title}</h1>
        <p className="meta">{s.filing_none_body}</p>
      </>
    );
  }

  return (
    <>
      <h1>{s.filing_heading}</h1>
      {problem && (
        <p className="problem" role="alert">
          {problem}
        </p>
      )}
      {filed && <p role="status">{s.filing_done}</p>}
      <form
        onSubmit={(e) => {
          e.preventDefault();
          const form = e.currentTarget;
          const data = new FormData(form);
          setBusy(true);
          setProblem("");
          setFiled(false);
          api
            .file(
              String(data.get("text") ?? ""),
              String(data.get("kind") ?? "feature"),
              // Omitted when there is only one, which is what the server
              // expects: it takes the only one rather than demanding a name
              // nobody was asked for.
              repos.length > 1 ? String(data.get("repo") ?? "") : undefined,
            )
            .then(() => {
              setFiled(true);
              form.reset();
              onFiled();
            })
            .catch((err: Error) => setProblem(err.message))
            .finally(() => setBusy(false));
        }}
      >
        <label htmlFor="file-text">{s.filing_text_label}</label>
        <textarea
          id="file-text"
          name="text"
          required
          rows={6}
          placeholder={s.filing_text_placeholder}
        />

        <label htmlFor="file-kind">{s.filing_kind_label}</label>
        {/* **The labels translate; the values do not.** `feature` and `bug` are
            what the server matches on and what the developer reads on the review
            page, so a translated value would have a filer and a reviewer naming
            the same kind differently. */}
        <select id="file-kind" name="kind" defaultValue="feature">
          <option value="feature">{s.kind_feature}</option>
          <option value="bug">{s.kind_bug}</option>
        </select>

        {repos.length > 1 && (
          <>
            <label htmlFor="file-repo">{s.filing_repo_label}</label>
            {/* The repository names themselves are never translated — they are
                identifiers the server refuses if they do not match. */}
            <select id="file-repo" name="repo" required defaultValue={repos[0]}>
              {repos.map((r) => (
                <option key={r} value={r}>
                  {r}
                </option>
              ))}
            </select>
          </>
        )}

        <button type="submit" disabled={busy}>
          {s.filing_submit}
        </button>
      </form>

      {mine.length > 0 && (
        <>
          <h2>{s.file_mine_heading}</h2>
          {mine.map((r) => (
            <a className="item" key={r.id} href={`/public/request/${r.id}`}>
              {/* **Already translated by the server**, and deliberately not
                  re-translated here. `FiledRequest.state` is the coarse label a
                  filer is allowed to see, chosen server-side in the negotiated
                  locale — passing it through `stateLabel` would look it up as a
                  wire value, miss, and render the French text unchanged while
                  suggesting the client had a say in it. */}
              <span className="tag">{r.state}</span> {r.summary}
            </a>
          ))}
        </>
      )}
    </>
  );
}
