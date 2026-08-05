import { useEffect, useRef, useState } from "react";
import {
  admin,
  ApiError,
  type AccountView,
  type DaemonRecord,
  type OwnerRecord,
  type RepoRecord,
  type SettingsView,
} from "./api";
import { useStrings } from "./strings";

/// The administrative pages.
///
/// **One component per list, and they are almost the same shape**: read a list,
/// show it, offer one form to add and one button per row to revoke. Written out
/// rather than abstracted into a table generator, because the differences —
/// which fields, which warnings, what a revoke means — are the whole content and
/// a generic version would take them all as parameters anyway.

/// A list that has not loaded yet renders nothing rather than a spinner: these
/// are small files on local disk and the wait is imperceptible.
function useList<T>(
  load: () => Promise<T>,
): [T | null, (v: T) => void, string | null] {
  const [value, setValue] = useState<T | null>(null);
  const [problem, setProblem] = useState<string | null>(null);
  // **The loader in a ref, not in the dependency list.** Each caller defines it
  // inline, so it is a new function on every render — listing it would fetch in
  // a loop, and silencing that with a disable comment hides the reason.
  const loader = useRef(load);
  loader.current = load;
  useEffect(() => {
    loader
      .current()
      .then(setValue)
      .catch((e: ApiError) => setProblem(e.message));
  }, []);
  return [value, setValue, problem];
}

/// What this server does.
///
/// **Secrets are write-only.** The server sends whether a key is set and never
/// its value — there is no read path for one anywhere — so these fields are
/// always blank and submitting them blank keeps what is there.
export function Settings() {
  // `t` for the catalogue, because `s` is already the settings on this page. The
  // other components use `s` for the strings; here the collision would be worse
  // than the inconsistency.
  const t = useStrings();
  const [s, setS, problem] = useList(() => admin.settings());
  const [note, setNote] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const save = (patch: Record<string, unknown>) => {
    setNote(null);
    setError(null);
    admin
      .saveSettings(patch)
      .then((next) => {
        setS(next);
        setNote(t.admin_saved);
      })
      .catch((e: ApiError) => setError(e.message));
  };

  if (problem) return <p className="note">{problem}</p>;
  if (!s) return null;

  return (
    <>
      <h1>{t.settings_heading}</h1>
      {note && <p className="note">{note}</p>}
      {/* **Already translated by the server**, like every `ApiError.message`.
          The API answers a refusal in the negotiated locale, so re-translating
          it here would mean the client keeping a second copy of every refusal
          the server can produce. */}
      {error && <p className="note">{error}</p>}

      <h2>{t.settings_public_heading}</h2>
      <p className="meta">{t.settings_public_note}</p>
      <form
        onSubmit={(e) => {
          e.preventDefault();
          save({ public: !s.public });
        }}
      >
        <button type="submit">
          {s.public ? t.settings_public_off : t.settings_public_on}
        </button>
      </form>

      <h2>{t.settings_filers_heading}</h2>
      <SettingsForm
        fields={[]}
        extra={
          <label>
            <input
              type="checkbox"
              defaultChecked={s.show_spec ?? true}
              onChange={(e) => save({ show_spec: e.target.checked })}
            />{" "}
            {t.settings_show_spec}
          </label>
        }
        onSave={save}
      />

      <h2>{t.settings_stack_heading}</h2>
      <p className="meta">{t.settings_stack_note}</p>

      <h2>{t.settings_ceilings_heading}</h2>
      <p className="meta">{t.settings_ceilings_note}</p>
      <SettingsForm
        numeric
        fields={[
          { name: "max_daily_filings", label: t.settings_max_filings, value: String(s.max_daily_filings ?? "") },
          { name: "max_daily_drafts", label: t.settings_max_drafts, value: String(s.max_daily_drafts ?? "") },
          { name: "max_accounts", label: t.settings_max_accounts, value: String(s.max_accounts ?? "") },
          { name: "max_outstanding_links", label: t.settings_max_links, value: String(s.max_outstanding_links ?? "") },
        ]}
        onSave={save}
      />
    </>
  );
}

function SettingsForm({
  fields,
  onSave,
  secret,
  numeric,
  extra,
}: {
  fields: { name: string; label: string; value: string }[];
  onSave: (patch: Record<string, unknown>) => void;
  secret?: boolean;
  numeric?: boolean;
  extra?: React.ReactNode;
}) {
  const t = useStrings();
  return (
    <form
      onSubmit={(e) => {
        e.preventDefault();
        const data = new FormData(e.currentTarget);
        const patch: Record<string, unknown> = {};
        for (const f of fields) {
          const v = String(data.get(f.name) ?? "");
          // A blank secret is "keep what is there" and must not be sent at all;
          // a blank number is "use the default" and must be sent as null.
          if (secret && !v) continue;
          patch[f.name] = numeric ? (v === "" ? null : Number(v)) : v;
        }
        onSave(patch);
      }}
    >
      {fields.map((f) => (
        <div key={f.name}>
          <label htmlFor={f.name}>{f.label}</label>
          <input
            id={f.name}
            name={f.name}
            type={secret && f.name.endsWith("_key") ? "password" : "text"}
            defaultValue={f.value}
            autoComplete="off"
          />
        </div>
      ))}
      {extra}
      <button type="submit">{t.admin_save}</button>
    </form>
  );
}

/// Who may review, and for what.
export function Owners() {
  const t = useStrings();
  const [list, setList, problem] = useList(() => admin.owners());
  const [error, setError] = useState<string | null>(null);
  if (problem) return <p className="note">{problem}</p>;
  if (!list) return null;
  return (
    <>
      <h1>{t.owners_heading}</h1>
      <p className="meta">{t.owners_note}</p>
      {error && <p className="note">{error}</p>}
      {list.map((o) => (
        // The login is an email address and the repositories are names. Neither
        // is translated: they are what the operator typed and what the server
        // matches on.
        <div className="item" key={o.login}>
          {o.login} <span className="meta">{o.repos.join(", ")}</span>
          {o.revoked ? (
            <span className="tag">{t.admin_revoked_tag}</span>
          ) : (
            <form
              onSubmit={(e) => {
                e.preventDefault();
                admin.revoke<OwnerRecord[]>("owners", o.login).then(setList).catch((x: ApiError) => setError(x.message));
              }}
            >
              <button type="submit">{t.admin_revoke}</button>
            </form>
          )}
        </div>
      ))}
      <h2>{t.owners_add_heading}</h2>
      <form
        onSubmit={(e) => {
          e.preventDefault();
          const data = new FormData(e.currentTarget);
          admin
            .addOwner(
              String(data.get("login") ?? ""),
              String(data.get("repos") ?? "")
                .split(",")
                .map((r) => r.trim())
                .filter(Boolean),
            )
            .then(setList)
            .catch((x: ApiError) => setError(x.message));
        }}
      >
        <label htmlFor="login">{t.owners_email_label}</label>
        <input id="login" name="login" type="email" required autoComplete="off" />
        <label htmlFor="repos">{t.owners_repos_label}</label>
        <input id="repos" name="repos" required autoComplete="off" />
        <button type="submit">{t.admin_add}</button>
      </form>
    </>
  );
}

/// What the public site collects for.
export function Repos() {
  const t = useStrings();
  const [list, setList, problem] = useList(() => admin.repos());
  const [error, setError] = useState<string | null>(null);
  const [unserved, setUnserved] = useState<string | null>(null);

  const add = (name: string, anyway: boolean) => {
    setError(null);
    admin
      .addRepo(name, anyway)
      .then((next) => {
        setList(next);
        setUnserved(null);
      })
      .catch((x: ApiError) => {
        // **409 is the "no machine offers this" refusal**, which is overridable —
        // the daemon may simply not have polled yet. Anything else is an error.
        if (x.status === 409) setUnserved(name);
        else setError(x.message);
      });
  };

  if (problem) return <p className="note">{problem}</p>;
  if (!list) return null;
  return (
    <>
      <h1>{t.repos_heading}</h1>
      {error && <p className="note">{error}</p>}
      {/* Split around the repository name rather than formatted with it: the
          catalogue carries no placeholders, and the name is an identifier the
          operator typed. */}
      {unserved && (
        <p className="note">
          {t.repos_unserved_before}
          {unserved}
          {t.repos_unserved_after}{" "}
          <button type="button" onClick={() => add(unserved, true)}>
            {t.repos_enable_anyway}
          </button>
        </p>
      )}
      {list.map((r) => (
        <div className="item" key={r.name}>
          {/* The repository name and the machine's label are both identifiers.
              Only the *absence* of a machine is a phrase, so only that
              translates. */}
          {r.name} <span className="meta">{r.served_by ?? t.repos_no_machine}</span>
          {r.disabled ? (
            <span className="tag">{t.repos_off_tag}</span>
          ) : (
            <form
              onSubmit={(e) => {
                e.preventDefault();
                admin.revoke<RepoRecord[]>("repos", r.name).then(setList).catch((x: ApiError) => setError(x.message));
              }}
            >
              <button type="submit">{t.repos_turn_off}</button>
            </form>
          )}
        </div>
      ))}
      <h2>{t.repos_add_heading}</h2>
      <form
        onSubmit={(e) => {
          e.preventDefault();
          const data = new FormData(e.currentTarget);
          add(String(data.get("name") ?? ""), false);
        }}
      >
        <label htmlFor="name">{t.repos_name_label}</label>
        <input id="name" name="name" required autoComplete="off" />
        <button type="submit">{t.repos_enable}</button>
      </form>
    </>
  );
}

/// The machines that draft specs.
export function Daemons() {
  const t = useStrings();
  const [list, setList, problem] = useList(() => admin.daemons());
  const [minted, setMinted] = useState<{ label: string; key: string } | null>(null);
  const [error, setError] = useState<string | null>(null);
  if (problem) return <p className="note">{problem}</p>;
  if (!list) return null;
  return (
    <>
      <h1>{t.daemons_heading}</h1>
      {error && <p className="note">{error}</p>}
      {minted && (
        <div className="note">
          {/* The label leads the sentence, so the catalogue holds only what
              follows it — the same split-not-format rule, with nothing before
              the value to hold. The label is the operator's own word. */}
          <p>
            <strong>{minted.label}</strong>
            {t.daemons_minted_after}
          </p>
          {/* A text node, and the only copy: the volume holds a hash. Never
              translated for the obvious reason — it is a secret, not prose. */}
          <pre>{minted.key}</pre>
        </div>
      )}
      {list.map((d) => (
        <div className="item" key={d.label}>
          {d.label}
          {d.revoked ? (
            <span className="tag">{t.admin_revoked_tag}</span>
          ) : (
            <form
              onSubmit={(e) => {
                e.preventDefault();
                admin.revoke<DaemonRecord[]>("daemons", d.label).then(setList).catch((x: ApiError) => setError(x.message));
              }}
            >
              <button type="submit">{t.admin_revoke}</button>
            </form>
          )}
        </div>
      ))}
      <h2>{t.daemons_add_heading}</h2>
      <form
        onSubmit={(e) => {
          e.preventDefault();
          const data = new FormData(e.currentTarget);
          const label = String(data.get("label") ?? "");
          admin
            .mintDaemon(label)
            .then((m) => {
              setMinted(m);
              return admin.daemons().then(setList);
            })
            .catch((x: ApiError) => setError(x.message));
        }}
      >
        <label htmlFor="label">{t.daemons_label_label}</label>
        <input id="label" name="label" required autoComplete="off" />
        <button type="submit">{t.daemons_mint}</button>
      </form>
    </>
  );
}

/// Who can file, and the switch that stops them.
export function Accounts() {
  const t = useStrings();
  const [list, setList, problem] = useList(() => admin.accounts());
  const [error, setError] = useState<string | null>(null);
  if (problem) return <p className="note">{problem}</p>;
  if (!list) return null;
  return (
    <>
      <h1>{t.accounts_heading}</h1>
      <p className="meta">{t.accounts_note}</p>
      {error && <p className="note">{error}</p>}
      {list.map((a) => (
        <div className="item" key={a.id}>
          {/* A masked address — `j***@example.com`. Data, not prose. */}
          {a.email_hint}
          {a.has_password && <span className="tag">{t.accounts_password_tag}</span>}
          {a.revoked ? (
            <span className="tag">{t.admin_revoked_tag}</span>
          ) : (
            <form
              onSubmit={(e) => {
                e.preventDefault();
                admin.revoke<AccountView[]>("accounts", a.id).then(setList).catch((x: ApiError) => setError(x.message));
              }}
            >
              <button type="submit">{t.admin_revoke}</button>
            </form>
          )}
        </div>
      ))}
    </>
  );
}

export type { SettingsView };
