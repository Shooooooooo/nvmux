# nvmux

## Comments

Keep comments short, and write them only where they are needed. This holds
for the Rust, the shell scripts in `scripts/`, the Lua embedded in
`src/client/`, and the Python in `tools/`.

- Comment only what the code can't say: why, an invariant, an ordering or
  race constraint, a Neovim, terminal, ssh or OS quirk, a magic number's
  reason, a `SAFETY` argument.
- Never restate the code, an item's name or a test's name. A test's comment
  says what its name leaves out, such as the bug it guards or a timing
  reason, or there is none.
- No history or measurement stories in comments; put them in the commit
  message. Keep only the conclusion and the number that matters.
- Plain short sentences, usually one to three lines. Doc comments open with
  a one-line summary.
- When you change code, fix or delete the comments it makes wrong.
