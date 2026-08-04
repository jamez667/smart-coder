import { expect, test } from "@playwright/test";
import { server, signInWithPassword, type Server } from "./fixture";

// The review gate, asserted in a browser.
//
// **These are the tests the move out of Rust would otherwise lose.** Each
// encodes a decision recorded in spec 20 as markup — the two decisions weighing
// the same, the controls sitting after the artifact, the bypass being visible.
// None can be asserted against JSON, and all of them are real properties of a
// gate whose entire purpose is that a human actually read something.
//
// The Rust original is named in each test, so the connection survives the file
// they used to live in being deleted.
//
// **A scratch server per file**, claimed and populated through the real paths.
// A review test that skipped when there was nothing to review would pass on a
// broken build, which is worse than not having it.

let sc: Server;
let requestId: string;

test.beforeAll(async () => {
  sc = await server();
  requestId = await sc.fileAndDraft("The thing is broken and should not be.");
});

test.afterAll(() => sc?.stop());

test.beforeEach(async ({ page, context }) => {
  const session = await signInWithPassword(sc.base, sc.login, sc.password);
  await context.addCookies([
    {
      name: "sc_device",
      value: session,
      url: sc.base,
      httpOnly: true,
      sameSite: "Strict",
    },
  ]);
  await page.goto(`${sc.base}/request/${requestId}`);
  await expect(page.locator("#decide")).toBeVisible();
});

test("approve and send back carry the same weight", async ({ page }) => {
  // **Replaces `approve_and_send_back_carry_the_same_weight`.** Spec 20: a
  // phone UI whose easiest action is a big green button, on an artifact too
  // long to read on that screen, produces rubber-stamp approval — and a
  // rubber-stamped gate is worse than no gate, because the system still
  // reports a human signed off.
  //
  // The Rust version compared the opening tags byte for byte, because that was
  // the strongest thing available to a string. A browser can be asked what the
  // buttons actually look like, which is what the property was always about.
  const buttons = page.locator("#decide button");
  const n = await buttons.count();
  expect(n, "send back, approve, discard").toBe(3);

  const looks: string[] = [];
  for (let i = 0; i < n; i++) {
    looks.push(
      await buttons.nth(i).evaluate((b) => {
        const s = getComputedStyle(b);
        return [s.backgroundColor, s.color, s.fontSize, s.fontWeight].join("|");
      }),
    );
  }
  expect(
    new Set(looks).size,
    `the decisions are styled differently: ${looks.join(" vs ")}`,
  ).toBe(1);
});

test("send back comes before approve", async ({ page }) => {
  // Also from the Rust original: the order is part of the same argument. The
  // easy action must not be the first one a thumb reaches.
  const labels = await page.locator("#decide button").allTextContents();
  expect(labels[0]).toContain("Send back");
  expect(labels[1]).toContain("Approve");
});

test("the decision comes after the whole artifact", async ({ page }) => {
  // **Replaces `the_decision_comes_after_the_whole_artifact`.** The one property
  // document order actually gives: on a phone the controls are physically below
  // the spec, so they cannot be reached without scrolling past what they decide
  // on.
  //
  // Rust compared string offsets. A browser compares positions, which is the
  // thing the offsets were standing in for.
  const spec = page.locator("pre").last();
  const specBox = await spec.boundingBox();
  const decideBox = await page.locator("#decide").boundingBox();
  expect(specBox).toBeTruthy();
  expect(decideBox).toBeTruthy();
  expect(
    decideBox!.y,
    "the decision sits below the artifact it decides on",
  ).toBeGreaterThan(specBox!.y + specBox!.height - 1);
});

test("the skip link is visible rather than hidden", async ({ page }) => {
  // **Replaces `the_skip_link_is_visible_rather_than_hidden`.** Hiding the
  // bypass does not remove it — it only lets the system believe nobody used one.
  const skip = page.locator('a[href="#decide"]');
  await expect(skip).toBeVisible();
});

test("a send back demands a reason in the form itself", async ({ page }) => {
  // **Replaces `a_send_back_demands_a_reason_in_the_form_itself`.** `required`
  // in the markup, not a check in JavaScript: a send-back with no reason
  // produces a redraft that repeats itself.
  await expect(page.locator("#notes")).toHaveAttribute("required", "");
});

test("the drafted spec is rendered as text, never as markup", async ({
  page,
}) => {
  // **Replaces `a_drafted_spec_is_escaped_rather_than_rendered`.** A model wrote
  // it and it may contain anything. The server used to escape it; React renders
  // it as a text node, and the eslint ban on innerHTML is what keeps it that
  // way. This asserts the outcome rather than the mechanism.
  const spec = page.locator("pre").last();
  await expect(spec).toContainText("What this changes");
  // The heading in the spec is Markdown, and must appear as characters rather
  // than as an element.
  expect(await spec.locator("h1").count()).toBe(0);
});
