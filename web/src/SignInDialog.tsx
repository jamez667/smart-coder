import { useEffect, useRef, useState } from "react";
import { api } from "./api";

/// The way in, and the only one.
///
/// **A real `<dialog>` opened with `showModal()`**, not a positioned div. The
/// element brings focus trapping, Escape-to-close, inertness of the page behind
/// it and a `::backdrop` — every one of which an overlay has to reimplement, and
/// the accessibility half of that list is what such overlays usually skip.
///
/// The layout is the one settled on the server surface: the magic link first,
/// because that is what almost everybody arriving here needs, and the password
/// behind a disclosure because the two named roles know they are doing something
/// different.
///
/// **Both forms fetch rather than navigate.** They used to be native POSTs that
/// returned a rendered page, which worked only because a rendered page existed
/// to return. Submitting in place also keeps the dialog open to show a refusal,
/// where a navigation threw the reader out of whatever they were doing to land
/// on a fresh document.
export function SignInDialog({ onClose }: { onClose: () => void }) {
  const ref = useRef<HTMLDialogElement>(null);
  const [sent, setSent] = useState(false);
  const [problem, setProblem] = useState("");
  const [busy, setBusy] = useState(false);

  /// **A full reload rather than a state update.** Signing in changes what every
  /// view may show, and the server decides that from a cookie it has just set;
  /// reloading asks it once instead of teaching each component to re-fetch.
  const reload = () => window.location.reload();

  useEffect(() => {
    const d = ref.current;
    if (!d) return;
    d.showModal();
    const closed = () => onClose();
    d.addEventListener("close", closed);
    return () => d.removeEventListener("close", closed);
  }, [onClose]);

  return (
    <dialog
      ref={ref}
      id="signin-dialog"
      // Clicking the backdrop closes it. The dialog fills its own box, so a
      // click whose target IS the dialog element landed outside the content.
      onClick={(e) => {
        if (e.target === ref.current) ref.current?.close();
      }}
    >
      <button
        className="close"
        type="button"
        aria-label="Close"
        onClick={() => ref.current?.close()}
      >
        ×
      </button>
      <h2>Sign in</h2>
      <p>
        Filing a request needs an email address — it is how you find your way
        back to what you filed, and it keeps this form from being a free-for-all.
      </p>
      {problem && (
        <p className="problem" role="alert">
          {problem}
        </p>
      )}
      {sent ? (
        /* **The same words whether or not an account existed.** The server
           answers identically on purpose, so that somebody probing addresses
           learns nothing; saying "check your email" only when there was one to
           send to would give it all back. */
        <p role="status">
          If that address can sign in here, a link is on its way. It works once,
          for fifteen minutes.
        </p>
      ) : (
        <>
          <form
            onSubmit={(e) => {
              e.preventDefault();
              const email = new FormData(e.currentTarget).get("email");
              setBusy(true);
              setProblem("");
              api
                .requestLink(String(email ?? ""))
                .then(() => setSent(true))
                .catch((err: Error) => setProblem(err.message))
                .finally(() => setBusy(false));
            }}
          >
            <label htmlFor="dlg-email">Email</label>
            <input
              id="dlg-email"
              name="email"
              type="email"
              required
              autoCapitalize="off"
              autoCorrect="off"
              spellCheck={false}
              placeholder="you@example.com"
            />
            <button type="submit" disabled={busy}>
              Email me a link
            </button>
          </form>
          <p className="meta">
            No password. We send a link that works once, for fifteen minutes.
            Filing for the first time creates the account.
          </p>
        </>
      )}
      <details className="pw">
        <summary>Admin login</summary>
        <form
          onSubmit={(e) => {
            e.preventDefault();
            const data = new FormData(e.currentTarget);
            setBusy(true);
            setProblem("");
            api
              .signInWithPassword(
                String(data.get("login") ?? ""),
                String(data.get("password") ?? ""),
              )
              .then(reload)
              .catch((err: Error) => setProblem(err.message))
              .finally(() => setBusy(false));
          }}
        >
          <label htmlFor="dlg-login">Email</label>
          <input
            id="dlg-login"
            name="login"
            type="email"
            required
            autoCapitalize="off"
            autoCorrect="off"
            spellCheck={false}
            placeholder="you@example.com"
          />
          <label htmlFor="dlg-password">Password</label>
          <input id="dlg-password" name="password" type="password" required />
          <button type="submit" disabled={busy}>
            Sign in
          </button>
        </form>
      </details>
    </dialog>
  );
}
