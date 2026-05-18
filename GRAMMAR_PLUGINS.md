# Grammar Plugin Guide

Add support for a new language by writing an ANTLR grammar + Java extractor, then dropping them into a plugin directory.

## Overview

Each grammar plugin has two parts:

1. **Grammar files** (`.g4`) — ANTLR4 lexer and parser grammars that define how to parse the language
2. **Java extractor** — a `BaseExtractor` subclass that walks the parse tree and emits symbols/relations

Grammar files go in a plugin directory. The extractor lives in the driver jar and is loaded by class name at runtime.

## Directory Structure

```
grammars/
  your_language/
    YourLang_Lexer.g4
    YourLang_Parser.g4
    plugin.toml
```

Plugin directories can live in:
- `~/.infigraph/grammars/` — user-global plugins
- `<project>/grammars/` — project-local plugins

## plugin.toml Reference

```toml
[language]
name = "your_language"          # unique identifier
extensions = [".ext1", ".ext2"] # file extensions this grammar handles
entry_rule = "program"          # top-level parser rule name
lexer = "YourLang_Lexer.g4"    # lexer grammar filename
parser = "YourLang_Parser.g4"  # parser grammar filename
extractor = "YourLangExtractor" # Java extractor class name (in com.infigraph.driver.extractors)

# Optional: C-style preprocessor support (handles #ifdef, #include, #define)
# Use when source files contain C preprocessor directives
preprocessor = "c"

# Optional: emit Imports relations for cross-file form references
emit_referenced_form_imports = true
```

## Writing a Custom Extractor

Extractors live in `driver/src/main/java/com/infigraph/driver/extractors/`.

Extend `BaseExtractor` and implement `processRule()`. Return `true` if the rule creates a scope (function, section), `false` otherwise. The base class handles tree walking, scope stack management, and module symbol creation.

### Minimal Example

```java
package com.infigraph.driver.extractors;

import org.antlr.v4.runtime.*;
import org.antlr.v4.runtime.tree.*;

public class YourLangExtractor extends BaseExtractor {

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
                    return true; // scope — will auto-pop after children
                }
                return false;
            }
            case "functionCall": {
                String target = findChildRawText(tree, "identifier", ctx.ruleNames);
                if (target != null) {
                    int[] span = getSpan((RuleContext) tree, tokens);
                    ctx.pushRelation(target, "Calls",
                        span[0], span[1], span[2], span[3]);
                }
                return false;
            }
            default:
                return false;
        }
    }
}
```

The example above shows the minimal pattern. For languages with form-qualified fields, reads/writes tracking, or preprocessor integration, extend `processRule()` with additional cases and use the helpers below.

### Available Helpers

**Tree navigation:**
- `findChildRawText(tree, childRule, ruleNames)` — text of first matching child rule (no spaces)
- `findChildRawTextByIndex(tree, childRule, n, ruleNames)` — text of nth matching child rule
- `collectRawText(tree)` — concatenated text of all leaf tokens (no spaces)
- `hasChildRule(tree, ruleName, ruleNames)` — check if child rule exists
- `hasChildToken(tree, tokenText)` — check if child terminal with exact text exists
- `getSpan(ruleContext, tokens)` — `[startLine, startCol, endLine, endCol]`

**Context methods (on `ExtractContext`):**
- `ctx.pushSymbol(name, kind, sl, sc, el, ec, text, formQualified)` — emit a symbol. Set `formQualified=true` for fields that should get `FORMNAME::fieldName` IDs
- `ctx.pushRelation(targetName, kind, sl, sc, el, ec)` — emit a file-local relation
- `ctx.pushFormQualifiedRelation(formName, fieldName, kind, sl, sc, el, ec, trackRef)` — emit a cross-file relation targeting `FORMNAME::fieldName`. Set `trackRef=true` to track as referenced form for import generation
- `ctx.sourceId()` — current source symbol ID (scope-aware)
- `ctx.scopeStack` — push/pop to track nesting (functions, sections)
- `ctx.formNames` — list of form names declared in the file

**Pre-processing:**
- Override `init(ctx, source)` for source-level scanning before parse tree walk
- `parseFormNames(source, ctx.formNames)` — scan source for `FORM prefix.NAME;` declarations

### Symbol Kinds

`Module`, `Section`, `Function`, `Method`, `Class`, `Struct`, `Interface`, `Trait`, `Enum`, `Variable`, `Constant`, `Field`, `Test`, `Route`

### Relation Kinds

`Calls`, `Reads`, `Writes`, `Imports`, `Inherits`, `Implements`, `Contains`

### Building After Adding an Extractor

After writing the extractor:

1. Add the `.java` file to the `javac` line in `driver/build.sh` (lines 42-46)
2. Rebuild the driver jar: `cd driver && ./build.sh`
3. Copy `infigraph-driver.jar` next to the `infigraph` binary (or set `INFIGRAPH_DRIVER_JAR`)

The grammar `.g4` files and `plugin.toml` are hot-loaded at runtime — no rebuild needed for grammar-only changes. But new/modified extractors require a jar rebuild.

## Preprocessor Configuration

When `preprocessor = "c"` is set, infigraph uses JCPP to evaluate C preprocessor
directives (`#ifdef`, `#ifndef`, `#else`, `#endif`, `#include`, `#define`) before parsing.

This requires a `.infigraph.toml` file at the root of the project being analyzed:

```toml
[preprocessor]
defines = ["FS1040"]
include_paths = [
    "common-comind/comps",
    "common-comall/comps",
    "comps",
]
```

- `defines` — preprocessor symbols to define (equivalent to `-DSYMBOL` compiler flags)
- `include_paths` — directories to search for `#include` files (relative to project root)

### How to Discover Preprocessor Settings for a New Project

**Step 1: Find include paths**

Scan source files for `#include` directives to see what directories are referenced:

```bash
grep -roh '#include "[^"]*"' comps/ | sed 's/#include "//;s/"//' | sort -u
```

Common patterns:
- `"../common-comall/comps/fed.h"` -> include path: `common-comall/comps`
- `"someutil.inc"` -> include path: same directory as source (usually `comps/`)

List the directories that contain the included files. Use paths relative to project root.

**Step 2: Find preprocessor defines**

Count `#ifdef`/`#ifndef` symbols by frequency:

```bash
grep -roh '#ifdef [A-Za-z_0-9]*\|#ifndef [A-Za-z_0-9]*' comps/ \
  | sed 's/#ifn\?def //' | sort | uniq -c | sort -rn | head -20
```

Example output:
```
1647 PER
1415 PRO
1145 FS1040
1138 FS1040NR
 148 kFK1PW
 131 FS540
```

These are typically **product/build variants** — mutually exclusive configurations
set by the build system. Common patterns:

- Product variants: `FS1040` (federal 1040), `PRO` (professional), `PER` (personal)
- Feature flags: `WEB`, `IRS`, `INTVIEW`
- Debug toggles: `ExchDebug`

**Step 3: Determine which defines to use**

Pick the defines that match the build variant you want to analyze. Usually:
- Check build scripts, Makefiles, or CI configs for `-D` flags
- Ask the build team which defines are active for a given product
- If unsure, start with the most common define and test

Defines are often mutually exclusive (e.g., `FS1040` vs `FS540` — different tax form years).
Setting conflicting defines will include code from both branches, which may cause parse errors.

**Step 4: Validate**

Run infigraph on a few source files and check:
- Parse error count should decrease compared to no preprocessing
- Symbol/relation counts should increase (more code visible after `#ifdef` resolution)
- No regressions (files that parsed cleanly before should still parse cleanly)

### Fallback Behavior

If JCPP fails on a file (e.g., macros using unsupported C operators like `##` token
paste or `#@` charizing), infigraph automatically falls back to parsing the raw source.
This means every file gets processed — JCPP-compatible files get accurate preprocessing,
others get best-effort parsing.

## Example: Adding a New Grammar

1. Get ANTLR4 `.g4` files for your language (lexer + parser)
2. Create `grammars/my_lang/` directory, copy `.g4` files into it
3. Open the parser grammar, identify:
   - Entry rule (usually `program`, `compilationUnit`, `file`)
   - Rules for declarations (functions, classes, variables)
   - Rules for references (function calls, variable reads)
4. Write a Java extractor (`MyLangExtractor.java`) mapping those rules to symbols/relations
5. Add it to `driver/build.sh` and rebuild the jar
6. Write `plugin.toml` with `extractor = "MyLangExtractor"`
7. If source files use C preprocessor directives, add `preprocessor = "c"` and
   create `.infigraph.toml` in the target project
8. Run infigraph and check symbol/relation counts and parse errors
