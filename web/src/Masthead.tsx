import type { Me } from "./api";

/// The bar across the top, on every page.
///
/// **The account menu is still a `<details>`.** It needs no script now that
/// there is script everywhere, but the element brings its own keyboard and
/// screen-reader behaviour and a div with a click handler has to reimplement all
/// of it — the same argument that chose `<dialog>` over a positioned overlay.
export function Masthead({ me, onSignIn }: { me: Me; onSignIn: () => void }) {
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
              <AccountMenu me={me} />
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
function AccountMenu({ me }: { me: Me }) {
  return (
    <details className="acct">
      <summary className="btn">Account</summary>
      <div className="menu">
        <a href="/public">What you have filed</a>
        {me.can.administer && (
          <>
            <hr />
            <p className="grp">Admin</p>
            <a href="/review">Requests</a>
            <a href="/settings">Settings</a>
            <a href="/repos">Repositories</a>
            <a href="/owners">Owners</a>
            <a href="/daemons">Machines</a>
            <a href="/accounts">Who can file</a>
          </>
        )}
        {me.can.review && !me.can.administer && (
          <>
            <hr />
            <a href="/">Requests to review</a>
          </>
        )}
        <hr />
        <form method="post" action="/public/signout">
          <button type="submit">Sign out</button>
        </form>
      </div>
    </details>
  );
}
