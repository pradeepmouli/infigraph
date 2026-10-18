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
and ends nothing. A fence that is opened and never closed is **not** a fence:
CommonMark would run it to the end of the file, which hides every entry below it
from the index, `doctor` and the edit guard, so its lines are read as prose and
`doctor` lists it (`unclosed code fence at file:line`). A page that documents
this format with fences inside a fence ends up this way, since the first bare
fence line closes the outer one. That is the whole format; nothing is
configurable.

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

## What needs updating (`stale_docs`)

`get_doc_context` labels one entry. To find the entries that drifted, and what
the code did, call the MCP tool `stale_docs` or run `infigraph stale-docs`; it
reads, it never writes (infigraph does not author prose, and the Markdown is the
source of truth), so the answer is where to edit and why (`doc_entry`, below,
makes the edit):

```
Stale docs: 842 of 4008 entries possibly stale (exact git blame of 4008 entries); showing the 20 ...

1. src/app.py::compute_total -- entry docs/glossary/src.md:3-5
   possibly stale: code changed 2026-10-11 49fb64ee, entry last touched 2026-09-25 d75b2d88 (code at src/app.py:1-5)
   what changed since the entry:
   49fb64ee 2026-10-11 tighten rounding
     @@ -3,1 +3,1 @@
     -        total += item
     +        total += item * 2
```

- **Arguments.** `path_prefix` keeps the entries whose symbol's file or whose
  document lies under it (whole path components, as in the other analysis
  tools: `PathScope`); `symbol` names one id and gets a fuller history; `limit`
  (default 20) cuts the list, ranked by the gap between the code's newest
  change and the entry's, biggest first. There is no `since`: the gap already
  orders the list and the entry's own commit is the baseline.
- **What changed** is `git log -L start,end:file <entry commit>..HEAD`: the
  commits after the entry that touched the symbol's lines, with their patches
  cut to those lines (changed lines and one around them; `...` marks the rest).
  Uncommitted edits to those lines are listed first. `git log -L` reads line
  numbers at HEAD, so a file with uncommitted edits is mapped back first
  (`git diff -U0 HEAD`). Measured on sittir: 0.03s per entry.
- **Caps.** A list shows 3 commits and 900 characters per entry; one `symbol`
  shows 10 and 6,000. What is cut says so: `(truncated: older commits not
  shown)`, `(truncated: patch cut at N chars)`.
- **Budget.** The whole-project call judges exactly for 30 seconds (`doctor
  --docs-sync` allows 120) and judges the rest per file, which under-reports and
  is said to; on sittir the whole project takes 17-27s, a `path_prefix` subtree
  about 6s and one `symbol` under 2s.
- **Degraded, not silent.** No document index: the report says so. Not a
  checkout, git missing or timing out: the entries are listed as not judged and
  the degraded mode `doc-sync-unknown` is set. A key whose symbol is gone is
  counted and left to `doctor`.
- **One judgement.** `doctor --docs-sync` and the tool both call
  `stale_docs::judge` (and `get_doc_context` the same `git_blame::compare`), so
  their counts agree. All git access is in `infigraph_core::git_blame`, the one
  place to replace if history is ever kept in the graph.

After editing an entry, the document watcher refreshes the index, or call
`reindex_docs`.

## Changing an entry (`doc_entry`)

`stale_docs` says where to edit; `doc_entry` (MCP tool, `infigraph doc-entry
<op>`) does the edit, so a glossary change is one call that cannot damage the
entries around it. Every op **previews by default** (a diff, nothing written) and
writes only with `apply: true` / `--apply`. An applied op writes the Markdown and
then refreshes the document index (skipped, with a note, when document indexing
is off), and repeating it is a no-op that says "already done".

| op | does |
| --- | --- |
| `edit` | replaces `old` with `new` once inside the entry (`replace_all` for every match inside it); refuses on no match or on several, with the count |
| `replace` | rewrites the entry's lead text (or one `section`) |
| `create` | adds an entry for a key; the file is the document that holds most entries of the same code file, else the nearest ancestor directory's majority (never the project root), else it refuses and asks for `doc_file` |
| `rekey` | changes an entry's key; refuses when the new key does not resolve unless `force`, and lists the prose mentions of the old key it did not edit |
| `delete` | removes an entry; refuses while its symbol still resolves unless `force` |
| `ack` | records that the entry was read against the symbol as it is now (below) |

- **Ranges are re-read.** The entry's lines come from parsing the file at call
  time (`code_links::entries`), never from stored rows, so an op after an
  unindexed edit still lands on the right lines. An op that would change the
  list of entries (an `edit` that adds or drops a key heading) is refused.
- **One key, several entries.** The same heading twice in a document, or in two
  documents, makes `edit`, `replace`, `rekey` and `delete` refuse with every
  `file:line` listed. Choose with `nth` (1-based, in file order within one
  document), `line` (the entry's key heading line) or `doc_file`. A `delete` of an
  entry that is already gone ("no 2nd entry (1 found)") is "already done"; any other
  op says how many entries there are. `nth` and `line` are positions, so after a
  delete the survivor is the only one: preview before repeating. `doctor` lists a
  duplicated key with `file:line` of each entry.
- **The index may be behind.** The stored index names the file that holds a key;
  whether an entry is there is the file's answer. If the refresh after a write
  fails (the daemon is down, the growth breaker is tripped), the file is still
  written, the output says `document index NOT refreshed: <reason>` and the exit
  code is 0, and asking for the same change again says "already done" for every
  op (a `rekey` or `delete` whose old key the file no longer has, a `create`
  whose entry the file already has). `infigraph index-docs` brings the index up to
  date.
- **Worktrees.** A linked worktree with its own index (`infigraph worktree init`)
  is edited and refreshed on its own. One without an index would resolve to the
  main checkout around it and change its glossary instead, so `doc_entry` refuses,
  naming the command.
  The MCP tool without `path` means the project its server started in, so an
  `apply` with no `path` is refused when that project has linked worktrees
  (pass `path`); a dry run is allowed. Every MCP report's first line is
  `project: <root>`, the root it resolved.
- **Acknowledging.** Read the changed lines (`stale_docs` shows them) against the
  entry first, and `ack` only an entry that is still true; never ack in bulk to
  clear a list. Most "possibly stale" entries are still true. `ack` stores
  `DocAck(target, commit, commit_time, acked_at)` in `docs.kuzu`, one newest row
  per target, and nothing in the Markdown. `stale_docs::verdict` then treats the
  entry as in sync for code up to that commit's time, so a later change to the
  symbol makes it stale again; `get_doc_context` shows no label for it and
  `stale_docs` reports "N acknowledged". An `ack` is refused when the symbol's
  lines have uncommitted edits (a review names a commit). `rekey` does not carry
  an ack to the new key (`ack: true` records a fresh one); `delete` drops it. A
  document reindex exports the acks before it wipes the store and writes them back.
- **One write path.** The ack write is `WriteRequest::AckDocEntries` on the
  daemon socket, taken under the same exclusive docs lock as indexing; a client
  never opens `docs.kuzu` for it. Under `INFIGRAPH_BACKEND=kuzu` it runs in the
  calling process.

### The edit guard (Claude Code)

`infigraph install` adds a second PreToolUse hook (`infigraph-doc-guard.sh`,
matcher `Write|Edit|MultiEdit`) that runs `infigraph doc-guard`. It denies an
`Edit`/`MultiEdit` whose `old_string` lies inside a key-heading entry of a
Markdown file, and a `Write` over a file that has entries, and the message names
the op to use (`edit` for a small fix, `replace` for a rewrite, `rekey`,
`delete`, `create`, `ack`). Limits, on purpose:

- It parses the file and reads one setting (`[docs] enabled`); it opens no
  database and starts no daemon. Any doubt (no project, no `jq`, unparsable
  input, docs off) allows the edit.
- It guards Claude Code's file tools only. A shell redirect, `sed -i` or another
  editor is not seen; the guard is a nudge toward the tool, not access control.
  Another agent integration would need its own hook calling `infigraph doc-guard`
  with the same JSON.
- The ops' own writes never go through the file tools, so the guard cannot block
  them; there is no setting to turn the guard off short of uninstalling it.
- The hook script ships with the CLI. An existing install gets it, and the new
  skill text, only after `infigraph install` is run again.

## `infigraph doctor`

`doctor` always checks the keys: every documents key must resolve to a live
symbol, and the report lists the ones that do not, by reason:

- written with `.` where the graph uses `::` (`A.b` for `A::b`), with the
  spelling to use;
- several symbols of that name in the file (`name#N`: the id depends on source
  order, so it cannot be a key);
- the file is not in the graph (not indexed, or deleted);
- no such symbol in the file (renamed or removed).

It also lists a key that has more than one entry (with `file:line` of each) and a
code fence that is never closed.

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
interrupted run leaves the document to be scanned again. `DocAck` is the one
table that does not belong to a document: it outlives a re-index (carried over a
`reindex_docs`) and is dropped only by `doc_entry delete`, `clean_docs`, or when
a newer ack for the same target replaces it.
