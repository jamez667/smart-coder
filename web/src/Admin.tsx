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
        setNote("Saved.");
      })
      .catch((e: ApiError) => setError(e.message));
  };

  if (problem) return <p className="note">{problem}</p>;
  if (!s) return null;

  return (
    <>
      <h1>Settings</h1>
      {note && <p className="note">{note}</p>}
      {error && <p className="note">{error}</p>}

      <h2>The public site</h2>
      <p className="meta">
        Whether strangers can file requests here at all. A freshly claimed server
        starts with this off.
      </p>
      <form
        onSubmit={(e) => {
          e.preventDefault();
          save({ public: !s.public });
        }}
      >
        <button type="submit">
          {s.public ? "Turn the public site off" : "Turn the public site on"}
        </button>
      </form>

      <h2>What this site is called</h2>
      <SettingsForm
        fields={[
          { name: "site_name", label: "Site name", value: s.site_name },
        ]}
        extra={
          <label>
            <input
              type="checkbox"
              defaultChecked={s.show_spec ?? true}
              onChange={(e) => save({ show_spec: e.target.checked })}
            />{" "}
            Let a filer read the spec drafted from their own request
          </label>
        }
        onSave={save}
      />

      <h2>Sending mail</h2>
      <p className="meta">
        Sign-in links are sent through this. Without it, nobody but the
        administrator and the owners can sign in.
      </p>
      <SettingsForm
        fields={[
          { name: "mail_provider", label: "Provider", value: s.mail_provider },
          { name: "mail_from", label: "From address", value: s.mail_from },
          { name: "mail_from_name", label: "From name", value: s.mail_from_name },
        ]}
        onSave={save}
      />

      <h2>Secrets</h2>
      <p className="meta">
        {s.mail_key_set ? "A mail key is set." : "No mail key is set."}{" "}
        {s.screen_key_set ? "A screening key is set." : "No screening key is set."}{" "}
        Values are never shown — leaving a field blank keeps what is there.
        Changing one needs a sign-in from the last five minutes.
      </p>
      <SettingsForm
        secret
        fields={[
          { name: "mail_key", label: "Mail key", value: "" },
          { name: "screen_key", label: "Screening key", value: "" },
          { name: "base_url", label: "This server's address", value: s.base_url },
        ]}
        onSave={save}
      />

      <h2>Ceilings</h2>
      <p className="meta">
        What this server will spend in a day. Blank means the built-in default,
        which is not the same as zero.
      </p>
      <SettingsForm
        numeric
        fields={[
          { name: "max_daily_filings", label: "Filings a day", value: String(s.max_daily_filings ?? "") },
          { name: "max_daily_drafts", label: "Drafts a day", value: String(s.max_daily_drafts ?? "") },
          { name: "max_accounts", label: "Accounts", value: String(s.max_accounts ?? "") },
          { name: "max_outstanding_links", label: "Outstanding sign-in links", value: String(s.max_outstanding_links ?? "") },
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
      <button type="submit">Save</button>
    </form>
  );
}

/// Who may review, and for what.
export function Owners() {
  const [list, setList, problem] = useList(() => admin.owners());
  const [error, setError] = useState<string | null>(null);
  if (problem) return <p className="note">{problem}</p>;
  if (!list) return null;
  return (
    <>
      <h1>Owners</h1>
      <p className="meta">
        An owner signs in with a username and password, and reviews requests for
        the repositories you name here. They can send work back, release it and
        discard it — they cannot accept it.
      </p>
      {error && <p className="note">{error}</p>}
      {list.map((o) => (
        <div className="item" key={o.login}>
          {o.login} <span className="meta">{o.repos.join(", ")}</span>
          {o.revoked ? (
            <span className="tag">revoked</span>
          ) : (
            <form
              onSubmit={(e) => {
                e.preventDefault();
                admin.revoke<OwnerRecord[]>("owners", o.login).then(setList).catch((x: ApiError) => setError(x.message));
              }}
            >
              <button type="submit">Revoke</button>
            </form>
          )}
        </div>
      ))}
      <h2>Add an owner</h2>
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
        <label htmlFor="login">Email</label>
        <input id="login" name="login" type="email" required autoComplete="off" />
        <label htmlFor="repos">Repositories, separated by commas</label>
        <input id="repos" name="repos" required autoComplete="off" />
        <button type="submit">Add</button>
      </form>
    </>
  );
}

/// What the public site collects for.
export function Repos() {
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
      <h1>Repositories</h1>
      {error && <p className="note">{error}</p>}
      {unserved && (
        <p className="note">
          No machine has offered {unserved}. Enabling it anyway means requests
          filed against it will wait until one does.{" "}
          <button type="button" onClick={() => add(unserved, true)}>
            Enable it anyway
          </button>
        </p>
      )}
      {list.map((r) => (
        <div className="item" key={r.name}>
          {r.name}{" "}
          <span className="meta">{r.served_by ?? "no machine has offered it"}</span>
          {r.disabled ? (
            <span className="tag">off</span>
          ) : (
            <form
              onSubmit={(e) => {
                e.preventDefault();
                admin.revoke<RepoRecord[]>("repos", r.name).then(setList).catch((x: ApiError) => setError(x.message));
              }}
            >
              <button type="submit">Turn it off</button>
            </form>
          )}
        </div>
      ))}
      <h2>Enable a repository</h2>
      <form
        onSubmit={(e) => {
          e.preventDefault();
          const data = new FormData(e.currentTarget);
          add(String(data.get("name") ?? ""), false);
        }}
      >
        <label htmlFor="name">Repository name</label>
        <input id="name" name="name" required autoComplete="off" />
        <button type="submit">Enable</button>
      </form>
    </>
  );
}

/// The machines that draft specs.
export function Daemons() {
  const [list, setList, problem] = useList(() => admin.daemons());
  const [minted, setMinted] = useState<{ label: string; key: string } | null>(null);
  const [error, setError] = useState<string | null>(null);
  if (problem) return <p className="note">{problem}</p>;
  if (!list) return null;
  return (
    <>
      <h1>Machines</h1>
      {error && <p className="note">{error}</p>}
      {minted && (
        <div className="note">
          <p>
            <strong>{minted.label}</strong> — this key is shown once and cannot be
            recovered. Put it in that machine&apos;s configuration now.
          </p>
          {/* A text node, and the only copy: the volume holds a hash. */}
          <pre>{minted.key}</pre>
        </div>
      )}
      {list.map((d) => (
        <div className="item" key={d.label}>
          {d.label}
          {d.revoked ? (
            <span className="tag">revoked</span>
          ) : (
            <form
              onSubmit={(e) => {
                e.preventDefault();
                admin.revoke<DaemonRecord[]>("daemons", d.label).then(setList).catch((x: ApiError) => setError(x.message));
              }}
            >
              <button type="submit">Revoke</button>
            </form>
          )}
        </div>
      ))}
      <h2>Add a machine</h2>
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
        <label htmlFor="label">A name for it</label>
        <input id="label" name="label" required autoComplete="off" />
        <button type="submit">Mint a key</button>
      </form>
    </>
  );
}

/// Who can file, and the switch that stops them.
export function Accounts() {
  const [list, setList, problem] = useList(() => admin.accounts());
  const [error, setError] = useState<string | null>(null);
  if (problem) return <p className="note">{problem}</p>;
  if (!list) return null;
  return (
    <>
      <h1>Who can file</h1>
      <p className="meta">
        Revoked accounts stay listed. A list that silently shrinks cannot answer
        &quot;did I already deal with that?&quot;.
      </p>
      {error && <p className="note">{error}</p>}
      {list.map((a) => (
        <div className="item" key={a.id}>
          {a.email_hint}
          {a.has_password && <span className="tag">password</span>}
          {a.revoked ? (
            <span className="tag">revoked</span>
          ) : (
            <form
              onSubmit={(e) => {
                e.preventDefault();
                admin.revoke<AccountView[]>("accounts", a.id).then(setList).catch((x: ApiError) => setError(x.message));
              }}
            >
              <button type="submit">Revoke</button>
            </form>
          )}
        </div>
      ))}
    </>
  );
}

export type { SettingsView };
