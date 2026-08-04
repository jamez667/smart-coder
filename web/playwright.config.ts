import { defineConfig, devices } from "@playwright/test";

// The browser harness.
//
// **This carries the tests the move out of Rust would otherwise lose.** Roughly
// thirty in-process tests asserted policy against rendered markup — that the two
// review decisions weigh the same, that the controls sit after the artifact,
// that a signed-out stranger is never shown an administrative route. None of
// those can be asserted against JSON, and all of them are real properties.
//
// The honest caveat, which `scripts/layout-check.js` already states about
// itself: *"a gate that cannot run everywhere is a gate somebody disables."*
// This needs a browser and a running server, so it is not part of
// `scripts/check.sh` — it runs as its own CI step.
export default defineConfig({
  testDir: "./e2e",
  forbidOnly: !!process.env.CI,
  retries: 0,
  reporter: process.env.CI ? "github" : "list",
  globalSetup: "./e2e/global-setup.ts",
  use: {
    // **Set by the global setup, never defaulted.** A fallback here is how the
    // first version of this harness ended up testing the author's own container
    // rather than a server it stood up — green locally, ECONNREFUSED in CI.
    baseURL: process.env.SC_BASE_URL,
    trace: "retain-on-failure",
  },
  // **One project at a time.** The two share a rate-limit bucket on the server
  // — PublicRead, 600/min, keyed on nothing because an anonymous reader has no
  // credential to key on. Running them together exhausts it and the failures
  // read as bugs in the interface rather than as the limiter doing its job.
  workers: 1,
  fullyParallel: false,
  projects: [
    { name: "chromium", use: { ...devices["Desktop Chrome"] } },
    // The surface exists for a phone on a train; the layout properties are
    // asserted where they actually matter.
    { name: "mobile", use: { ...devices["Pixel 7"] }, testIgnore: "**/api.spec.ts" },
  ],
});
