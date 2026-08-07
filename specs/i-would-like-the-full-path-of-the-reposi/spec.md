# Improvement Spec: Repository Path in Header

## Goals
- Replace the placeholder text "smart-coder" in the header with the actual repository path "jamez667/smart-coder".
- Maintain existing UI/UX behavior and layout.
- Ensure the change is minimal and focused.

## Non-Goals
- Modify any other part of the application UI or functionality.
- Introduce new dependencies or external crates.
- Change the project's build or deployment process.

## Constraints
- Only modify existing `.rs` files.
- Do not introduce new modules unless necessary for wiring.
- Match existing code style and error handling patterns.
- Ensure `cargo check` and `cargo test` pass after changes.

## Current Behavior
The header currently displays "smart-coder" as a placeholder or hardcoded string.

## Desired Behavior
The header displays "jamez667/smart-coder" as the repository path.

## Files to Touch
- Identify the `.rs` file(s) responsible for rendering the header.
- Update the relevant string literal or variable to reflect "jamez667/smart-coder".