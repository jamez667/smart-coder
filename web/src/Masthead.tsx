import { api, type Me } from "./api";
import { LANGUAGES, useLocale, useSetLanguage, useStrings } from "./strings";

/// The bar across the top, on every page.
///
/// **The account menu is still a `<details>`.** It needs no script now that
/// there is script everywhere, but the element brings its own keyboard and
/// screen-reader behaviour and a div with a click handler has to reimplement all
/// of it — the same argument that chose `<dialog>` over a positioned overlay.
export function Masthead({
  me,
  onSignIn,
  onGo,
  path,
}: {
  me: Me;
  onSignIn: () => void;
  onGo: (to: string) => void;
  /// The address being drawn, so the bar can mark its own entry as current.
  ///
  /// Passed in rather than read from `window.location` here: the application
  /// routes with `pushState`, so the global updates without re-rendering
  /// anything, and a bar reading it directly would highlight the previous page
  /// until something else happened to redraw it.
  path: string;
}) {
  const s = useStrings();
  return (
    <>
      <input type="checkbox" id="theme-invert" className="theme-in" />
      <header className="bar">
        <div className="bar-inner masthead">
          <a className="wordmark" href="/">
            {/* The monogram. Not translated and not translatable — it is two
                letters of a product name used as a mark, and it is already
                hidden from a screen reader for the same reason. */}
            <span className="logo" aria-hidden="true">
              SC
            </span>
            <span>{s.brand}</span>
          </a>
          <Nav path={path} onGo={onGo} />
          <div className="controls">
            <label className="theme to-dark" htmlFor="theme-invert" title={s.theme_to_dark}>
              <span className="theme-in">{s.theme_to_dark}</span>
              {/* Decorative glyphs, hidden from a screen reader — the label
                  beside them carries the meaning, which is why they are not
                  catalogue strings. */}
              <span className="knob" aria-hidden="true">
                <span className="sun">☀</span>
                <span className="moon">☾</span>
              </span>
            </label>
            <label className="theme to-light" htmlFor="theme-invert" title={s.theme_to_light}>
              <span className="theme-in">{s.theme_to_light}</span>
              <span className="knob" aria-hidden="true">
                <span className="sun">☀</span>
                <span className="moon">☾</span>
              </span>
            </label>
            <LanguagePicker />
            {me.role === "anonymous" ? (
              <button className="btn" type="button" onClick={onSignIn}>
                {s.nav_signin}
              </button>
            ) : (
              <AccountMenu me={me} onGo={onGo} />
            )}
          </div>
        </div>
      </header>
    </>
  );
}

/// A link that routes rather than reloads.
///
/// **A real `href`, with the click intercepted.** Middle-click, "open in new
/// tab" and a screen reader announcing "link" all depend on the attribute being
/// there; a `<span onClick>` looks identical and is none of those things.
///
/// Hoisted out of `AccountMenu`, which had this exact function inside it, when
/// the navigation bar needed the same behaviour. Two copies of the modifier-key
/// check is two places to get it wrong.
function Link({
  to,
  onGo,
  className,
  current,
  children,
}: {
  to: string;
  onGo: (to: string) => void;
  className?: string;
  /// Marks this as the page being shown. Renders `aria-current="page"`, which is
  /// what a screen reader announces — the visual underline alone says nothing to
  /// somebody who cannot see it.
  current?: boolean;
  children: string;
}) {
  return (
    <a
      href={to}
      className={className}
      aria-current={current ? "page" : undefined}
      onClick={(e) => {
        // Let the browser handle anything that is not a plain left click, so
        // opening in a new tab still works.
        if (e.metaKey || e.ctrlKey || e.shiftKey || e.button !== 0) return;
        e.preventDefault();
        onGo(to);
      }}
    >
      {children}
    </a>
  );
}

/// The navigation bar's own entries, between the wordmark and the controls.
///
/// **Two entries, and the same two for everybody** — including a stranger. The
/// account menu is where a surface appears or disappears with a capability;
/// this bar is the fixed frame around it, and a frame whose shape changes when
/// you sign in is one a returning reader has to re-learn.
///
/// That is safe because `/requests` serves everybody: it draws the reviewer's
/// queue, the filer's own requests, or a prompt to sign in, and the server
/// decides which by answering `/me`. A stranger following it is not refused —
/// they are shown the door.
function Nav({ path, onGo }: { path: string; onGo: (to: string) => void }) {
  const s = useStrings();
  return (
    <nav className="nav" aria-label={s.nav_requests}>
      <Link to="/" onGo={onGo} current={path === "/"}>
        {s.nav_overview}
      </Link>
      {/* `/public` and `/review` are the two surfaces this stands in front of,
          and both still work — a bookmark to either is not broken by this. What
          `current` covers is the reader arriving at one of them from somewhere
          other than this bar, who should still see where they are. */}
      <Link
        to="/requests"
        onGo={onGo}
        current={path === "/requests" || path === "/public" || path === "/review"}
      >
        {s.nav_requests}
      </Link>
    </nav>
  );
}

/// Choosing a language.
///
/// **In the masthead, on every page, and reachable signed out.** Somebody who
/// cannot read the current page is exactly the person who needs this, so putting
/// it behind a sign-in or an account page would mean navigating a language they
/// do not have to reach the control that fixes it.
///
/// The options are listed by **endonym** — "Français", never "French" — for the
/// same reason: naming the languages in the language the reader cannot read
/// defeats the control.
///
/// **Submits on change**, with no confirm button. Requiring a second click to
/// apply a language is a step nobody expects, and the `.lang` rules in
/// `app.css` were written for exactly this control: they style a bare `<select>`
/// with a CSS-drawn chevron, and carry a button rule that only applies inside a
/// `<noscript>`. There is no `<noscript>` fallback here — the whole interface is
/// script — so that rule is simply unused, and the styling it was written
/// alongside is what this reuses.
function LanguagePicker() {
  const s = useStrings();
  const locale = useLocale();
  const setLanguage = useSetLanguage();
  return (
    <label className="lang">
      {/* The label is for a screen reader rather than for the eye: the control
          is a two-item select in a crowded bar, and a visible label would cost
          more room than it buys. `sr-only` is not in this stylesheet, so the
          text rides on the element instead. */}
      <select
        aria-label={s.language_label}
        value={locale}
        onChange={(e) => setLanguage(e.target.value)}
      >
        {LANGUAGES.map((l) => (
          <option key={l.code} value={l.code}>
            {l.endonym}
          </option>
        ))}
      </select>
    </label>
  );
}

/// The signed-in menu.
///
/// **Nothing here is drawn from a role string.** Each entry is gated on the
/// matching capability the server sent, so an interface that guesses wrong shows
/// a door the server would refuse — which is exactly the failure the server-built
/// menu could not have.
function AccountMenu({ me, onGo }: { me: Me; onGo: (to: string) => void }) {
  const s = useStrings();
  // `Link` lives at module scope now — the navigation bar needed the same
  // modifier-key handling, and it was declared inside this function, which also
  // meant a fresh component type on every render.
  const To = (p: { to: string; children: string }) => <Link {...p} onGo={onGo} />;

  return (
    <details className="acct">
      <summary className="btn">{s.nav_account}</summary>
      <div className="menu">
        <To to="/public">{s.nav_mine}</To>
        {me.can.administer && (
          <>
            <hr />
            {/* **Translated, unlike the route slugs they point at** — and now so
                are the pages themselves. The one administrator per server may
                not read English, and a French menu whose every entry opens an
                English page is worse than an English menu. */}
            <p className="grp">{s.nav_admin_heading}</p>
            <To to="/review">{s.nav_admin_review}</To>
            <To to="/settings">{s.nav_admin_settings}</To>
            <To to="/repos">{s.nav_admin_repos}</To>
            <To to="/owners">{s.nav_admin_owners}</To>
            <To to="/daemons">{s.nav_admin_daemons}</To>
            <To to="/accounts">{s.nav_admin_accounts}</To>
          </>
        )}
        {me.can.review && !me.can.administer && (
          <>
            <hr />
            <To to="/">{s.nav_review}</To>
          </>
        )}
        <hr />
        {/* Signing out revokes the session server-side and clears the cookie,
            then reloads — the server decides what every view may show, so
            asking it once beats teaching each component to forget. This was a
            native form POST, which worked only while a rendered page existed to
            return to. */}
        <button
          type="button"
          onClick={() => {
            api.signOut().finally(() => window.location.reload());
          }}
        >
          {s.nav_signout}
        </button>
      </div>
    </details>
  );
}
