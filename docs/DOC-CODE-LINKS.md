# Linking documents to code

A project that indexes its documents (`infigraph index-docs`) can tie a section
of a Markdown file to the symbol it describes. An agent that calls
`get_doc_context` on that symbol then gets the section in the same call, next to
the source, the callers and the callees. This is for rationale that does not
belong in the code: the *why* of a function, the invariant a type keeps, the
history of a decision.

There are two kinds of link, and they differ in how far to trust them.

| Link | Written as | Trust |
|---|---|---|
| **entry** | a heading whose whole text is a symbol id | exact: the heading *is* the key |
| **mention** | a code span in running text that names one symbol | heuristic: it counts only if it names exactly one |

Nothing links by a bare name. `` `process` `` in a sentence is not a link; it
matches too many things.

## Entries: key headings

A **key heading** is a Markdown heading of level 2, 3 or 4 whose entire text is
one backtick span holding a symbol id, which contains `::` and no whitespace:

```markdown
### `src/billing/invoice.py::Invoice::total`

Sums the lines before tax, in cents, because ...

#### body

The loop is deliberate: ...
```

An id is the one `infigraph search` prints: the file path from the project root,
`::`, and the symbol's qualified name, with members joined by `::`
(`file.py::Class::method`). It is not the `Class.method` spelling a language
uses, and it is not a symbol with a `#N` suffix (see below).

The entry is everything under the heading up to the next heading that is itself
a key heading, or whose level is the heading's own or higher. So `####`
sub-entries under a `###` key belong to it, and two keys of one level never
swallow each other. Fenced code is skipped, so a heading inside a fence starts
and ends nothing. That is the whole format; nothing is configurable.

`<file>::module` is an entry for the file itself (`src/billing/invoice.py::module`).
`get_doc_context` shows it as a separate block, **File-level notes**, on every
symbol in that file.

A heading that starts like a key but is not exactly one span (`` ### `a.py::f` --
the first ``, say) links nothing; `doctor` counts those so a typo does not
silently drop an entry.

## Mentions

A code span in text becomes a mention candidate when it is shaped like

- a file and a name: `routes.ts::admittedKinds`, any path suffix of the file
  (`src/routes.ts::admittedKinds`), or the full id, or
- a qualified name: `Invoice.total` or `Invoice::total`, either spelling.

It is a link to a symbol only if the string resolves to *exactly that one*
symbol in the graph when `get_doc_context` is asked. `Cart.add` with two `Cart`
classes in the project links to neither. Mentions are listed after the callees,
labelled heuristic, at most five, with the line they were written on.

A mention inside the symbol's own entry is not listed back at it.

## What `get_doc_context` shows

```
Glossary (docs/glossary/billing.md:12-30) -- possibly stale: code changed 2026-10-11 49fb64ee, entry last touched 2026-09-25 d75b2d88:
<the entry without its heading, up to 4,000 characters>

File-level notes (docs/glossary/billing.md:3-9):
<the file::module entry>

... source, callers, callees ...

Mentioned in (heuristic: a code span that names only this symbol; 2 of 2):
  docs/design/billing.md:41 § Totals
    Totals come from `invoice.py::Invoice::total`, never from ...
```

A project that has not opted in to documents gets none of this and nothing is
created. A document store written before this feature has documents no build has
scanned; `get_doc_context` says so and the degraded mode `doc-links-unscanned`
is set until `infigraph index-docs` runs (it scans them even though their
content has not changed).

## Is the entry still true? (`possibly stale`)

The label compares *when the lines last changed*, from `git blame -w`: the
newest change to the symbol's lines against the newest change to the entry's
lines. If the code is newer, the entry is **possibly stale**. It is a prompt to
re-read the entry against the code, never an error.

- Nothing is stored: not an mtime, not an index time. A checkout resets mtimes
  and a rebuild resets index times; history is the same in every clone.
- The same commit on both is in sync. An uncommitted change counts as the
  newest. A moved block reads as changed by the move.
- `get_doc_context` asks git for the two ranges only, with a 3-second limit. If
  git is missing, the project is not a checkout, or the call times out, the entry
  is shown without a label and the degraded mode `doc-sync-unknown` says why.

## `infigraph doctor`

`doctor` always checks the keys: every documents key must resolve to a live
symbol, and the report lists the ones that do not, by reason:

- written with `.` where the graph uses `::` (`A.b` for `A::b`), with the
  spelling to use;
- several symbols of that name in the file (`name#N`: the id depends on source
  order, so it cannot be a key);
- the file is not in the graph (not indexed, or deleted);
- no such symbol in the file (renamed or removed).

A rename that leaves a glossary behind shows up here instead of as a silent gap.
The default `doctor` also says that the sync check was not run.

`infigraph doctor --docs-sync` runs the staleness comparison over every entry.
It costs one `git blame` per file, so it has a 120-second budget: within it the
answer is exact; after it the remaining files are judged by their last commit
only. That per-file comparison is cheap and **under-reports** (about 62% of the
exact answer on the repository it was measured on), so `doctor` counts coarse
results separately and says so. The MCP `doctor` tool checks keys only; the sync
check is a command-line run.

## Groups and multi-repo

Unchanged. Links belong to one project's document store and are read for that
project's symbols. A group's cross-repo document search (`group_search_docs`,
`group_link_docs`) does not see them, and an entry never links a symbol of
another project.

## Where it lives

The index stores what the text says (`CodeLink`, `CodeLinkScan` in `docs.kuzu`)
and never reads the code graph. Reading a symbol's context resolves it against
the graph at that moment, so a renamed symbol is a key that no longer resolves,
not a dangling reference in either store. A document's links are replaced as a
unit when the document is re-indexed, deleted with it, and recorded last so an
interrupted run leaves the document to be scanned again.
