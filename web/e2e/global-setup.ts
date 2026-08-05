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
  // Nothing seeds a request any more. The smoke test does not read one, and
  // `fileAndDraft` costs a daemon poll and a drafting round trip on every run —
  // `fixture.ts` keeps it because the next test that needs a request will want
  // it, and rebuilding it from scratch is the expensive part.
  return () => running?.stop();
}
