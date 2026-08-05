import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import "./app.css";
import { App } from "./App";
import { StringsProvider } from "./strings";

const root = document.getElementById("root");
// **Not translated, deliberately.** This is a developer-facing throw for a
// broken build — the `<div id="root">` is in `index.html` and cannot be missing
// unless something upstream is wrong — and it is thrown before any catalogue
// could have been fetched. There is nobody to translate it for.
if (!root) throw new Error("no root element");
createRoot(root).render(
  <StrictMode>
    {/* **Above `App`, and it has to be.** Every component below reads its words
        out of this, including the masthead the wizard draws before `/me` has
        answered — so the catalogue must be in place before anything renders. */}
    <StringsProvider>
      <App />
    </StringsProvider>
  </StrictMode>,
);
