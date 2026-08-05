import { useEffect, useRef, useState } from "react";
import { api } from "./api";
import { useStrings } from "./strings";

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
  const s = useStrings();
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
        aria-label={s.dialog_close}
        onClick={() => ref.current?.close()}
      >
        {/* Decorative, and the reason the button carries an `aria-label`: the
            glyph is a multiplication sign, not a word, and a screen reader
            reading it aloud says nothing useful. */}
        ×
      </button>
      <h2>{s.signin_title}</h2>
      <p>{s.signin_intro}</p>
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
        <p role="status">{s.signin_sent}</p>
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
            <label htmlFor="dlg-email">{s.signin_email_label}</label>
            <input
              id="dlg-email"
              name="email"
              type="email"
              required
              autoCapitalize="off"
              autoCorrect="off"
              spellCheck={false}
              placeholder={s.signin_email_placeholder}
            />
            <button type="submit" disabled={busy}>
              {s.signin_submit}
            </button>
          </form>
          <p className="meta">{s.signin_no_password_note}</p>
        </>
      )}
      <details className="pw">
        <summary>{s.signin_password_heading}</summary>
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
          <label htmlFor="dlg-login">{s.signin_email_label}</label>
          <input
            id="dlg-login"
            name="login"
            type="email"
            required
            autoCapitalize="off"
            autoCorrect="off"
            spellCheck={false}
            placeholder={s.signin_email_placeholder}
          />
          <label htmlFor="dlg-password">{s.signin_password_label}</label>
          <input id="dlg-password" name="password" type="password" required />
          <button type="submit" disabled={busy}>
            {s.signin_password_submit}
          </button>
        </form>
      </details>
    </dialog>
  );
}
