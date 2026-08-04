import { useEffect, useRef } from "react";

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
/// different. Both forms post to the server's own routes rather than the JSON
/// API — signing in sets a cookie and returns a page, and it is the one flow
/// that predates the API and has no reason to move.
export function SignInDialog({ onClose }: { onClose: () => void }) {
  const ref = useRef<HTMLDialogElement>(null);

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
      <form method="post" action="/public/signin">
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
        <button type="submit">Email me a link</button>
        <button type="submit" className="ghost">
          Create an account
        </button>
      </form>
      <p className="meta">
        No password. We send a link that works once, for fifteen minutes.
      </p>
      <details className="pw">
        <summary>Admin login</summary>
        <form method="post" action="/public/signin/password">
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
          <button type="submit">Sign in</button>
        </form>
      </details>
    </dialog>
  );
}
