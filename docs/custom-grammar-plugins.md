# Creating Custom Grammar Plugins for Infigraph

Infigraph supports custom ANTLR4-based grammar plugins that let you parse any DSL or language, extract symbols and relations, and build a full code intelligence graph.

## Architecture Overview

A grammar plugin has 2 required components:

```
grammars/
  your-language/
    plugin.toml          # Plugin config + extraction rules (required)
    YourLexer.g4         # ANTLR4 lexer grammar (required)
    YourParser.g4        # ANTLR4 parser grammar (required)
```

No Java code is required. The `plugin.toml` file defines which grammar rules map to symbols and relations using the built-in `GenericExtractor`.

The **JVM driver** (`infigraph-driver.jar`) loads grammars at runtime, parses files using ANTLR4, and uses the extraction rules from `plugin.toml` to emit symbols and relations via JSON IPC.

## Quick Start

### 1. Create `plugin.toml`

```toml
[language]
name = "your-language"
extensions = [".ext1", ".ext2"]
entry_rule = "program"
lexer = "YourLexer.g4"
parser = "YourParser.g4"

# Optional: preprocessor (any command that reads a file and writes to stdout)
# [preprocessor]
# cmd = ["mcpp", "-W0"]
# define_flag = "-D"         # default
# include_flag = "-I"        # default
# line_markers = true        # strip #line markers from output (default true)
# pipe_strings = true        # collapse multi-line "|...|" strings before preprocessing

# --- Extraction Rules ---
# Map grammar rules to symbols and relations.
# No Java code needed — GenericExtractor handles this.

[[extract.symbols]]
rule = "functionDecl"
kind = "Function"
name = "identifier"       # child rule containing the name
scope = true              # creates a scope (auto-pops on exit)

[[extract.symbols]]
rule = "section"
kind = "Section"
name_path = ["sectionDecl", "identifier"]  # walk nested rules to find name
scope = true

[[extract.symbols]]
rule = "localVariableDecl"
kind = "Variable"
name = "identifierList"
split = ","               # split by comma for multiple names

[[extract.relations]]
rule = "assignmentStatement"
kind = "Writes"
target = "writableAddress"

[[extract.relations]]
rule = "functionCall"
kind = "Calls"
target = "identifier"
```

### 2. Write ANTLR4 Grammars

Place `.g4` files in `grammars/your-language/`.

- Lexer and parser must be **split grammars** (not combined). Use `lexer grammar` / `parser grammar`.
- The parser must set `options { tokenVocab=YourLexer; }`.
- If your grammar uses `import`, place imported grammar files in the same directory.

### 3. Build & Test

```bash
cd driver && bash build.sh
python3 tests/test_grammar_plugins.py
```

## plugin.toml Reference

### `[language]` section

| Field | Required | Description |
|-------|----------|-------------|
| `name` | Yes | Language identifier, matches the directory name |
| `extensions` | Yes | File extensions to match |
| `entry_rule` | Yes | The top-level rule in your parser grammar |
| `fallback_entry_rules` | No | Array of fallback rules to try if `entry_rule` produces parse errors (useful for fragment files) |
| `lexer` | Yes | Lexer grammar filename |
| `parser` | Yes | Parser grammar filename |
| `extractor` | No | Path to a custom `.java` extractor source file, e.g. `"MyExtractor.java"` (omit to use GenericExtractor) |
| `emit_referenced_form_imports` | No | If `true`, generates `Imports` relations for cross-file references |

### `[[extract.symbols]]` — Symbol extraction rules

Each entry maps an ANTLR parser rule to a symbol kind.

| Field | Required | Description |
|-------|----------|-------------|
| `rule` | Yes | ANTLR parser rule name to match |
| `kind` | Yes | Symbol kind to emit (see table below) |
| `name` | Yes* | Child rule containing the symbol name |
| `name_path` | Yes* | Array of rules to walk for nested names (e.g., `["sectionDecl", "identifier"]`) |
| `name_index` | No | 0-based index when multiple children match (e.g., `1` for the 2nd `identifier`) |
| `scope` | No | If `true`, pushes this symbol onto the scope stack (auto-pops on exit) |
| `split` | No | Split the extracted name by this delimiter to emit multiple symbols |
| `form_qualified` | No | If `true`, qualifies symbol with form names from source pre-scan |

*One of `name` or `name_path` is required.

### `[[extract.relations]]` — Relation extraction rules

Each entry maps an ANTLR parser rule to a relation kind.

| Field | Required | Description |
|-------|----------|-------------|
| `rule` | Yes | ANTLR parser rule name to match |
| `kind` | Yes | Relation kind to emit (see table below) |
| `target` | Yes* | Child rule containing the relation target |
| `target_path` | Yes* | Array of rules to walk for nested targets |
| `target_index` | No | 0-based index when multiple children match |
| `target_fallback` | No | Fallback child rule if primary target not found |

*One of `target` or `target_path` is required.

### `[extract]` options

| Field | Description |
|-------|-------------|
| `scan_form_names` | Pre-scan source for `FORM` declarations (for form-qualified symbols) |

### Symbol Kinds

| Kind | Use for |
|------|---------|
| `Module` | Top-level compilation unit (file, form, class) |
| `Section` | Named sections, blocks, topics |
| `Function` | Functions, methods, procedures |
| `Variable` | Variables, local declarations |
| `Constant` | Constants, defines |
| `Field` | Struct/record fields, global declarations |
| `Class` | Classes, types |

### Relation Kinds

| Kind | Use for |
|------|---------|
| `Calls` | Function/procedure calls |
| `Writes` | Assignment to a variable/field |
| `Reads` | Reading a variable/field |
| `Imports` | Cross-file references |

## Preprocessor

The `[preprocessor]` section in `plugin.toml` lets you run any external preprocessor before parsing. The preprocessor command receives the source file as its last argument and must write processed output to stdout.

### `[preprocessor]` section

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `cmd` | Yes | — | Command and default args (e.g., `["mcpp", "-W0"]`) |
| `define_flag` | No | `"-D"` | Flag prefix for `-D` defines |
| `include_flag` | No | `"-I"` | Flag prefix for `-I` include paths |
| `line_markers` | No | `true` | Strip `#line N "file"` markers and build source map |
| `pipe_strings` | No | `false` | Collapse multi-line `"\|...\|"` strings before preprocessing |

### Example: mcpp (MSVC-compatible)

```toml
[preprocessor]
cmd = ["mcpp", "-W0"]
pipe_strings = true
```

### Example: GCC cpp

```toml
[preprocessor]
cmd = ["cpp", "-traditional-cpp", "-w"]
```

### Example: MSVC cl.exe

```toml
[preprocessor]
cmd = ["cl", "/E", "/nologo"]
define_flag = "/D"
include_flag = "/I"
```

### Preprocessor Configuration (per project)

Configure defines and include paths per project in `.infigraph.toml`:

```toml
[grammar_plugins.your-language]
defines = ["DEFINE1", "DEFINE2=value", "PLATFORM_X"]
include_paths = ["includes/", "shared/"]
```

## Advanced: Custom Java Extractors

For complex extraction logic that can't be expressed in TOML (synthetic names, cross-form field reads, custom string processing, conditional logic), write a custom Java extractor.

**No build step required.** Write a plain `.java` source file, put it next to your `plugin.toml`, and set `extractor` to its filename. The driver compiles it on load (via the JDK compiler API) and loads the result — you never run `javac` yourself, and there's no jar to build or ship. This requires a **JDK** on the machine running the driver, not just a JRE (`java -version` alone isn't enough — you also need `javac`).

Set `extractor = "YourExtractor.java"` in `[language]` and omit the `[extract]` section. The path resolves relative to `plugin.toml`'s directory.

Create `YourExtractor.java` next to `plugin.toml`:

```java
import com.infigraph.driver.extractors.BaseExtractor;
import org.antlr.v4.runtime.*;
import org.antlr.v4.runtime.tree.*;

public class YourExtractor extends BaseExtractor {

    @Override
    protected boolean processRule(String ruleName, ParseTree tree,
            CommonTokenStream tokens, ExtractContext ctx) {
        switch (ruleName) {
            case "functionDecl": {
                String name = findChildRawText(tree, "identifier", ctx.ruleNames);
                if (name != null) {
                    int[] span = getSpan((RuleContext) tree, tokens);
                    ctx.pushSymbol(name, "Function",
                        span[0], span[1], span[2], span[3],
                        collectRawText(tree), false);
                    ctx.scopeStack.push(name);
                    return true;
                }
                return false;
            }
            default:
                return false;
        }
    }
}
```

No `package` declaration — the file is compiled standalone against the driver's own classpath (`BaseExtractor` and the ANTLR runtime are resolvable via `import`, but your class itself stays in the default package). The class name must match the filename, same as any Java source file.

### Class name resolution

The compiled class name is derived from the filename: `MyExtractor.java` → class `MyExtractor`. If your source declares a different public class name than the filename, compilation will fail — keep them matching.

### Compiled-class caching

The driver compiles a given `.java` path once per process and reuses the compiled class for every subsequent `set_extractor` call or batch-mode file — editing the `.java` file requires restarting the driver process to pick up the change (there's no file-watching/hot-reload).

### BaseExtractor Helpers

| Method | Description |
|--------|-------------|
| `getSpan(tree, tokens)` | Returns `int[]{startLine, startCol, endLine, endCol}` |
| `findChildRawText(tree, ruleName, ruleNames)` | First child matching rule name → text |
| `findChildRawTextByIndex(tree, ruleName, index, ruleNames)` | Nth child matching rule name → text |
| `collectRawText(tree)` | Full text of tree node (used for signature hashing) |
| `ctx.pushSymbol(name, kind, sL, sC, eL, eC, signature, formQualified)` | Emit a symbol. If `formQualified` is true and form names were scanned via `parseFormNames`, emits one symbol per known form instead of a single scoped symbol |
| `ctx.pushRelation(targetName, kind, sL, sC, eL, eC)` | Emit a relation targeting a name in the current file/scope |
| `ctx.pushFormQualifiedRelation(formName, fieldName, kind, sL, sC, eL, eC, trackRef)` | Emit a relation targeting `FORMNAME::fieldName` in another form — use for `Form.Field` cross-form reads. Set `trackRef=true` to also register the form in `ctx.referencedForms` (feeds `emit_referenced_form_imports`) |
| `ctx.scopeStack.push(name)` / `ctx.currentScope()` | Manage nested scope |

`BaseExtractor` only provides helpers that return **text** from a matched child (`findChildRawText` and friends) — there's no built-in helper that returns the child **tree node** itself. If you need to walk further into a specific child (e.g. to call `collectRawText` on just that subtree, or to look for a grandchild), write a small local helper that mirrors `findChildRawText`'s loop but returns the `ParseTree` instead of calling `collectRawText`/`collectText` on it.

### processRule return value

- **`return true`** — this rule creates a scope. Children are visited, scope auto-pops on exit. Only return `true` if you actually pushed something onto `ctx.scopeStack` — returning `true` without a matching push will throw when the walker tries to pop an empty stack.
- **`return false`** — children are still visited, but no scope management.

## JVM Driver JSON IPC Protocol

The driver accepts JSON commands on stdin and responds on stdout. One JSON object per line.

### Commands

**`load`** — Load a grammar
```json
{"cmd": "load", "id": "mygrammar", "lexer": "path/Lexer.g4", "parser": "path/Parser.g4", "entry_rule": "program", "preprocessor_cmd": "mcpp -W0", "preprocessor_pipe_strings": "true"}
```

**`set_extractor`** — Set custom Java extractor (compiled on load from source)
```json
{"cmd": "set_extractor", "id": "mygrammar", "class": "/absolute/path/to/MyExtractor.java"}
```
The `class` field must be an absolute path ending in `.java`, or the literal string `"GenericExtractor"`. Bare class names (e.g. `"MyExtractor"` with no `.java` suffix and no path) are rejected — this is the only accepted syntax.

**`set_extractor` (generic)** — Set GenericExtractor with pipe-delimited mappings
```json
{"cmd": "set_extractor", "id": "mygrammar", "class": "GenericExtractor", "mappings": "S:functionDecl:Function:identifier:scope|R:assignmentStatement:Writes:writableAddress"}
```

Mapping format: `TYPE:rule:kind:spec[:flags]` separated by `|`
- `S:rule:kind:nameSpec[:scope][:split=X][:fq]` — Symbol mapping
- `R:rule:kind:targetSpec[:fallback=X]` — Relation mapping
- `O:scan_form_names` — Options

Name/target spec formats:
- `identifier` — direct child rule
- `sectionDecl>identifier` — nested path
- `identifier#1` — Nth occurrence (0-based index)

**`extract`** — Parse and extract
```json
{"cmd": "extract", "id": "mygrammar", "file": "path/file.ext", "source": "...", "defines": "A,B=1", "include_paths": "/dir1,/dir2"}
```

**`shutdown`** — Stop the driver
```json
{"cmd": "shutdown"}
```

## Fragment Files and Fallback Entry Rules

Some languages compile via a single main file that `#include`s many fragment files. For example, a main file might contain the full program structure while individual fragment files contain only topics, functions, or sections.

When parsing fragment files individually, the primary `entry_rule` (e.g., `program`) may fail because fragments lack the top-level declarations (like `Formset`, `FORM`, or `CONVERSION`). Use `fallback_entry_rules` to handle this:

```toml
[language]
name = "your-language"
entry_rule = "program"
fallback_entry_rules = ["routineList", "topicList"]
```

The driver tries `entry_rule` first. If parse errors occur, it retries with each fallback rule and keeps the result with fewest errors. This allows extracting symbols from fragment files even when they can't parse as complete programs.

**Fragment files will typically have some parse errors** — this is expected and by design. The extractor still captures symbols and relations from the successfully parsed portions.

### Project-Level Preprocessor Configuration

Configure defines and include paths per project in `.infigraph.toml`:

```toml
[grammar_plugins.your-language]
defines = ["DEFINE1", "DEFINE2=value"]
include_paths = ["includes/", "shared/common/"]
```

The include paths must cover all directories referenced by `#include` directives. For languages with deep include chains (e.g., macros defined in shared libraries), ensure all directories in the chain are listed.

## CLI Batch Mode

The driver supports a batch mode for processing many files directly:

```bash
java -jar infigraph-driver.jar batch <grammar-dir> <source-dir> [options]
```

Options:
- `--ext .clc,.cmp` — Override file extensions (default: read from plugin.toml)
- `--defines X,Y=1` — Preprocessor defines
- `--include-paths /dir1,/dir2` — Preprocessor include paths
- `--preprocessor "mcpp -W0"` — Override preprocessor command
- `--pipe-strings` — Collapse multi-line pipe strings before preprocessing
- `--force-include path1,path2` — Force-include headers before preprocessing

Batch mode runs files in parallel across available CPU cores. Extensions are read from `plugin.toml` automatically.

## Plugin Discovery

Infigraph discovers grammar plugins in this order:
1. **Bundled** — `<binary_dir>/grammars/`
2. **Global** — `~/.infigraph/grammars/`
3. **Project** — `<project_root>/grammars/`

Project grammars override global, global overrides bundled.

## Checklist

- [ ] `plugin.toml` with correct `entry_rule`, `lexer`, `parser`
- [ ] `.g4` files parse clean in ANTLR4
- [ ] `[extract]` section defines symbol and relation mappings, or `extractor = "YourExtractor.java"` for custom Java (no separate build step — the driver compiles it on load, but a JDK must be present)
- [ ] `python3 tests/test_grammar_plugins.py` passes
- [ ] Smoke test: `infigraph index` picks up files and extracts symbols
