# Tasks

Work that is not done yet. One Markdown file per task, named
`NNN-short-name.md` with the next free number.

Each file starts with:

```markdown
# Title

- Status: under discussion | agreed | in progress | on hold
- Created: YYYY-MM-DD

## Goal
What the user wants, in their terms.

## Open questions
What the user still has to decide (empty once agreed).
```

and goes on with whatever helps whoever picks it up: the proposed design,
alternatives with their costs, what was already found out, how to test it.

- `under discussion`: a proposal. Nothing is implemented until the open
  questions are settled with the user; the answers are written into the file.
- `agreed`: ready to be implemented as written.
- `in progress`: someone is on it; note what is done and what is left.
- `on hold`: stopped until the user reports something (say what would
  bring it back); nobody works on it meanwhile.

A finished task is deleted in the commit that finishes it. By then what was
decided is in the docs.
