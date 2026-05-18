# `.bob/` — operator scratch

This directory is bob's local working space for the checkout. Nothing here is
canonical project state; it is operator scratch that gets rewritten on every
interaction with bob.

Layout:

- `.bob/notes/pending-notes.txt` — bob's working notes for the current checkout.
- `.bob/.bob-errors/errors-YYYY-MM-DD.log` — transient per-day error logs.

The whole subtree is gitignored except this README (see the `.bob/` block in
`.gitignore` at the repo root). If you find yourself wanting to commit
something from here, the right move is to promote it out of `.bob/` first.
