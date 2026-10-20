# Clone detection

`detect_clones` (MCP), `infigraph clones` (CLI) and the web API find
near-duplicate functions by comparing their **bodies**, not their names.

One function, `infigraph_core::clone_detect::find::find_clones`, does the work;
the three surfaces only render its report.

## What counts as the same code

Each body is reduced to a stream of tokens before it is compared:

| Dropped (a rename or reformat cannot change it) | Kept (it cannot be hidden) |
|---|---|
| comments, whitespace and layout | keywords, operators, punctuation |
| local variables and parameters (one placeholder) | callee names (`validate(x)`), member and path names (`a.save`, `fs::read`) |
| numbers and strings (one placeholder each) | type names (a capitalised word) |

So a copy with renamed locals and other literals is the same code, and
`validate(a)` against `remove(a)` is not. The lexer does not parse: "local" is
decided by that rule of thumb (a word before `(`, after `.`/`->`/`::`, or
capitalised is kept) and it is wrong in both directions now and then. A
function's own name is normalized, so a copy under a new name is still a copy.

## Which signal scored a pair

Every pair says which signal scored it:

| Signal | Score | When |
|---|---|---|
| `exact-body` | 1.0 | the two bodies hash alike |
| `body-embedding` | cosine of the two body embeddings | both functions have a body embedding, and share at least `[clones] min_overlap` of their tokens |
| `token-fallback` | MinHash Jaccard of the token shingles | a body has no embedding (no model, or it failed) |
| `signature` | cosine of the signature-text vectors in `embeddings.bin` | a function has no body data yet (not indexed since the upgrade, the file changed since it was indexed) |

`token-fallback` is scored by Jaccard against the same threshold as a cosine,
and Jaccard of shingles falls faster than a cosine as two bodies diverge, so
it is the stricter signal: a missing embedding costs recall, not precision.

`signature` is the signal this feature replaced: it compares `kind name:
docstring`, so two functions with one name and docstring match whatever their
bodies hold, and a copy under a new name does not. It survives only as a
labeled fallback, and the report counts the functions that used it.

## Settings

`[clones]` in `.infigraph/config.toml` or `~/.infigraph/config.toml`, or
`INFIGRAPH_CLONES_<KEY>`:

| Key | Default | |
|---|---|---|
| `min_tokens` | 30 | bodies of fewer normalized tokens are not clones of anything (getters, stubs). 30 keeps 84% of this repository's functions. |
| `min_overlap` | 0.5 | a `body-embedding` pair must share this fraction of its tokens (0.0 to 1.0; 0 turns the gate off: the embedding alone, expect noise). |
| `exact_below` | 10000 | up to this many functions every pair is scored; above it candidates come from LSH over the body sketches. The report says which mode ran. |

**The trade-off in `min_overlap`.** Embedding cosines are compressed, and the
compression belongs to the model: on this repository, with the default 0.92
threshold, 62% of all pairs passed when every identifier was normalized and
0.7% with callees kept, and the 0.92 to 0.96 band was mostly unrelated code.
Token overlap does not depend on the model. The price is that a clone whose
tokens diverge below `min_overlap` is not reported even if the embedder rates it
similar. Lower `min_overlap` to see more; the report says how many pairs the
gate withheld.

## The sidecar

`.infigraph/clone-bodies.bin` holds, per function and method, the token count,
a 64-slot MinHash sketch and the body embedding. It is derived data:
rebuilt by every `infigraph index`, listed by `doctor`, checked by `verify`,
copied by `infigraph clone` like `embeddings.bin`, and removed by
`worktree clean`. Its text is read from the source file once, and only if that
file still hashes to what the graph was written from: a file edited since it
was indexed yields no entries until the next index.

Degraded modes: `clone-bodies-unavailable` (no sidecar) and
`clone-bodies-partial` (some functions lack an entry, or have a sketch but no
embedding); the remedy for both is `infigraph index`. The counts cover the
functions that have a body to describe (`bodies::has_body_span`): a symbol only
a SCIP indexer found, which has its identifier's line and no span (one per
file, named after it), can never get an entry, so it is in neither number and
`find_clones` leaves it out of the candidates.

A local graph never embeds anything when you ask for clones. The one exception
is a **remote** graph (Neo4j and Postgres), which has no local sidecar: it
embeds the signature text of its functions at query time, labels every pair
`signature`, and the report says so.
