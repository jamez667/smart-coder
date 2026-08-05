import { api, type Me } from "./api";

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
}: {
  me: Me;
  onSignIn: () => void;
  onGo: (to: string) => void;
}) {
  return (
    <>
      <input type="checkbox" id="theme-invert" className="theme-in" />
      <header className="bar">
        <div className="bar-inner masthead">
          <a className="wordmark" href="/">
            <span className="logo" aria-hidden="true">
              SC
            </span>
            <span>Smart Coder</span>
          </a>
          <div className="controls">
            <label className="theme to-dark" htmlFor="theme-invert" title="Dark">
              <span className="theme-in">Dark</span>
              <span className="knob" aria-hidden="true">
                <span className="sun">☀</span>
                <span className="moon">☾</span>
              </span>
            </label>
            <label className="theme to-light" htmlFor="theme-invert" title="Light">
              <span className="theme-in">Light</span>
              <span className="knob" aria-hidden="true">
                <span className="sun">☀</span>
                <span className="moon">☾</span>
              </span>
            </label>
            {me.role === "anonymous" ? (
              <button className="btn" type="button" onClick={onSignIn}>
                Sign in
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

/// The signed-in menu.
///
/// **Nothing here is drawn from a role string.** Each entry is gated on the
/// matching capability the server sent, so an interface that guesses wrong shows
/// a door the server would refuse — which is exactly the failure the server-built
/// menu could not have.
function AccountMenu({ me, onGo }: { me: Me; onGo: (to: string) => void }) {
  // **A real `href`, with the click intercepted.** Middle-click, "open in new
  // tab" and a screen reader announcing "link" all depend on the attribute being
  // there; a `<span onClick>` looks identical and is none of those things.
  const Link = ({ to, children }: { to: string; children: string }) => (
    <a
      href={to}
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

  return (
    <details className="acct">
      <summary className="btn">Account</summary>
      <div className="menu">
        <Link to="/public">What you have filed</Link>
        {me.can.administer && (
          <>
            <hr />
            <p className="grp">Admin</p>
            <Link to="/review">Requests</Link>
            <Link to="/settings">Settings</Link>
            <Link to="/repos">Repositories</Link>
            <Link to="/owners">Owners</Link>
            <Link to="/daemons">Machines</Link>
            <Link to="/accounts">Who can file</Link>
          </>
        )}
        {me.can.review && !me.can.administer && (
          <>
            <hr />
            <Link to="/">Requests to review</Link>
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
          Sign out
        </button>
      </div>
    </details>
  );
}
