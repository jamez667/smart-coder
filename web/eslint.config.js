import js from "@eslint/js";
import tseslint from "typescript-eslint";

// The lint that stands in for `esc()`.
//
// The server used to escape everything that did not come from the crate, and its
// module doc said why: *"There is no 'trusted' path: the request text was typed
// by a person on the internet and the spec was written by a model."* React
// escapes by default, so that property survives the move — **except** through
// the three doors below, each of which hands a string to the HTML parser.
//
// This is a weaker guarantee than the one it replaces. Escaping was applied by
// the type system at the point of rendering; this is a lint that a determined
// author can silence. It is the strongest thing available on this side of the
// wire, and spec 18 records the trade rather than pretending it is even.
export default tseslint.config(
  { ignores: ["dist", "../crates/sc-server/assets/**"] },
  js.configs.recommended,
  ...tseslint.configs.recommended,
  {
    files: ["**/*.{ts,tsx}"],
    rules: {
      // **The whole reason this file exists.** A drafted spec is model-authored
      // text and a request is text somebody typed on the internet; either
      // reaching the HTML parser is the class of bug the server's escaping
      // removed outright.
      "react/no-danger": "off", // not using the react plugin; the rules below do it
      "no-restricted-properties": [
        "error",
        {
          property: "innerHTML",
          message:
            "innerHTML parses HTML. Every string here is model-authored or typed by a stranger — use textContent, or let React render it.",
        },
        {
          property: "outerHTML",
          message: "outerHTML parses HTML. Use textContent.",
        },
      ],
      "no-restricted-syntax": [
        "error",
        {
          selector: "JSXAttribute[name.name='dangerouslySetInnerHTML']",
          message:
            "dangerouslySetInnerHTML parses HTML. The specs this renders were written by a model; render them as text.",
        },
        {
          selector: "CallExpression[callee.property.name='insertAdjacentHTML']",
          message: "insertAdjacentHTML parses HTML. Use textContent.",
        },
        {
          // `document.write` is the other parser door, and there is no reason
          // for it to appear in a bundled application at all.
          selector: "CallExpression[callee.property.name='write'][callee.object.name='document']",
          message: "document.write parses HTML.",
        },
      ],
    },
  },
);
