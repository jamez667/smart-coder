import { expect, test } from "@playwright/test";

// What the server answers, asserted through a real HTTP client.
//
// **Its own file so it runs on one project.** These are transport tests,
// not layout ones: a second identical pass on a phone-shaped viewport proves
// nothing and spends the anonymous rate-limit budget, and the 429 that
// follows gets blamed on the interface.

test.describe("what the API refuses", () => {

  test("a mutating call from a form content type is refused", async ({
    request,
  }) => {
    // The CSRF second line, asserted through a real HTTP client rather than the
    // in-process fixture: a `<form>` can only send three content types, and none
    // of them is `application/json`.
    const res = await request.post("/api/v1/ui/requests/r-1/discard", {
      form: { note: "x" },
    });
    expect(res.status()).toBe(415);
  });

  test("the administrative endpoints do not exist for a stranger", async ({
    request,
  }) => {
    // **404, never 403** — a 403 would confirm the address is real.
    for (const path of ["settings", "owners", "repos", "daemons", "accounts"]) {
      const res = await request.get(`/api/v1/ui/${path}`);
      expect(res.status(), path).toBe(404);
    }
  });
});
