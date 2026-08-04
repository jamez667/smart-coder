import { server, type Server } from "./fixture";

// One server for the whole run.
//
// **Every spec asks for this address rather than assuming one.** The first
// version of this harness defaulted `baseURL` to the container the author
// happened to have running on `127.0.0.1:8791`, which passed locally and failed
// every test in CI with `ECONNREFUSED` — the tests were not testing a server,
// they were testing *that author's laptop*.
//
// Started once rather than per file because claiming a server costs an argon2
// hash and a container start, and nothing here mutates state another spec reads.

let running: Server | undefined;

export default async function globalSetup() {
  running = await server();
  // Playwright's config cannot await, so the address is handed on through the
  // environment — the one channel a config file, a spec and a teardown all see.
  process.env.SC_BASE_URL = running.base;
  process.env.SC_ADMIN_LOGIN = running.login;
  process.env.SC_ADMIN_PASSWORD = running.password;
  process.env.SC_REQUEST_ID = await running.fileAndDraft(
    "The search is broken on a phone and should not be.",
  );
  return () => running?.stop();
}
