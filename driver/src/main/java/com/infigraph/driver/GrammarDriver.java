package com.infigraph.driver;

import org.antlr.v4.Tool;
import org.antlr.v4.tool.Grammar;
import org.antlr.v4.tool.LexerGrammar;
import org.antlr.v4.runtime.*;
import org.antlr.v4.runtime.tree.*;

import com.infigraph.driver.extractors.BaseExtractor;

import java.io.*;
import java.net.URLClassLoader;
import java.nio.file.*;
import java.util.*;
import java.util.concurrent.*;
import java.util.regex.*;
import javax.tools.JavaCompiler;
import javax.tools.JavaFileObject;
import javax.tools.StandardJavaFileManager;
import javax.tools.ToolProvider;

public class GrammarDriver {

    static class PreprocessorConfig {
        String[] cmd;
        String defineFlag = "-D";
        String includeFlag = "-I";
        boolean lineMarkers = true;
        boolean pipeStrings = false;
        String forceInclude;
    }

    static class LoadedGrammar {
        LexerGrammar lexer;
        Grammar parser;
        String entryRule;
        String[] fallbackEntryRules;
        String[] ruleNames;
        PreprocessorConfig preprocessor;
        boolean emitReferencedFormImports;
        BaseExtractor extractor;
    }

    static class SourceMapping {
        final String file;
        final int originalLine;
        SourceMapping(String file, int originalLine) {
            this.file = file;
            this.originalLine = originalLine;
        }
    }

    // Cache compiled extractor classes by absolute .java source path, so
    // batch mode's per-file newInstance() reuse doesn't trigger a recompile.
    private static final Map<String, Class<?>> extractorClassCache = new ConcurrentHashMap<>();

    /**
     * Resolves the extractor spec (either "GenericExtractor" or an absolute
     * path to a .java source file) into a BaseExtractor instance.
     * .java files are compiled on the fly via the JDK compiler API and
     * loaded via URLClassLoader — no build step required from the caller.
     */
    private static BaseExtractor resolveExtractor(String extractorSpec) throws Exception {
        if ("GenericExtractor".equals(extractorSpec)) {
            return new com.infigraph.driver.extractors.GenericExtractor();
        }
        if (!extractorSpec.endsWith(".java")) {
            throw new IllegalArgumentException(
                "Unsupported extractor '" + extractorSpec + "': must be 'GenericExtractor' "
                + "or an absolute path to a .java source file");
        }
        Class<?> cls = extractorClassCache.get(extractorSpec);
        if (cls == null) {
            cls = compileExtractor(extractorSpec);
            extractorClassCache.put(extractorSpec, cls);
        }
        return (BaseExtractor) cls.getDeclaredConstructor().newInstance();
    }

    private static Class<?> compileExtractor(String javaSourcePath) throws Exception {
        Path sourcePath = Paths.get(javaSourcePath);
        if (!Files.exists(sourcePath)) {
            throw new FileNotFoundException("Extractor source not found: " + javaSourcePath);
        }

        JavaCompiler compiler = ToolProvider.getSystemJavaCompiler();
        if (compiler == null) {
            throw new IllegalStateException(
                "No Java compiler available (running on a JRE, not a JDK). "
                + "Custom .java extractors require a JDK to compile on load: " + javaSourcePath);
        }

        Path outDir = Files.createTempDirectory("infigraph-extractor-");
        String driverClasspath = System.getProperty("java.class.path");

        StringWriter diagnostics = new StringWriter();
        try (StandardJavaFileManager fm = compiler.getStandardFileManager(null, null, null)) {
            Iterable<? extends JavaFileObject> units =
                fm.getJavaFileObjectsFromStrings(List.of(javaSourcePath));
            List<String> options = List.of(
                "-cp", driverClasspath,
                "-d", outDir.toString());
            boolean ok = compiler.getTask(
                new PrintWriter(diagnostics), fm, null, options, null, units).call();
            if (!ok) {
                throw new IllegalStateException(
                    "Failed to compile extractor '" + javaSourcePath + "':\n" + diagnostics);
            }
        }

        String className = sourcePath.getFileName().toString().replace(".java", "");
        URLClassLoader loader = new URLClassLoader(
            new java.net.URL[]{outDir.toUri().toURL()},
            GrammarDriver.class.getClassLoader());
        return Class.forName(className, true, loader);
    }

    /**
     * Reads a source file as UTF-8, falling back to ISO-8859-1 (Latin-1) if
     * the bytes aren't valid UTF-8. Legacy DSL source trees mix encodings
     * file-by-file; Latin-1 never throws (every byte sequence is valid), so
     * it's a safe universal fallback rather than crashing on the first
     * non-ASCII byte a file happens to contain.
     */
    private static String readSourceFile(Path path) throws IOException {
        byte[] bytes = Files.readAllBytes(path);
        try {
            java.nio.CharBuffer decoded = java.nio.charset.StandardCharsets.UTF_8.newDecoder()
                .onMalformedInput(java.nio.charset.CodingErrorAction.REPORT)
                .onUnmappableCharacter(java.nio.charset.CodingErrorAction.REPORT)
                .decode(java.nio.ByteBuffer.wrap(bytes));
            return decoded.toString();
        } catch (java.nio.charset.CharacterCodingException e) {
            return new String(bytes, java.nio.charset.StandardCharsets.ISO_8859_1);
        }
    }

    /**
     * Describes an exception for user-facing error messages. Falls back to
     * the exception's class name + originating stack frame when getMessage()
     * is null (common for NullPointerException), so a bug in a customer's
     * extractor is diagnosable instead of reporting an empty error.
     */
    private static String describeException(Exception e) {
        String msg = e.getMessage();
        if (msg != null && !msg.isEmpty()) return msg;
        StackTraceElement[] trace = e.getStackTrace();
        String at = trace.length > 0 ? " at " + trace[0] : "";
        return e.getClass().getName() + at;
    }

    private static void applySourceMap(BaseExtractor.ExtractContext ctx, Map<Integer, SourceMapping> sourceMap) {
        if (sourceMap == null || sourceMap.isEmpty()) return;
        Map<String, Integer> fileIndex = new HashMap<>();
        List<String> fileList = new ArrayList<>();
        TreeMap<Integer, int[]> compact = new TreeMap<>();
        for (Map.Entry<Integer, SourceMapping> e : sourceMap.entrySet()) {
            String f = e.getValue().file;
            Integer idx = fileIndex.get(f);
            if (idx == null) { idx = fileList.size(); fileIndex.put(f, idx); fileList.add(f); }
            compact.put(e.getKey(), new int[]{idx, e.getValue().originalLine});
        }
        ctx.sourceMap = compact;
        ctx.sourceMapFiles = fileList.toArray(new String[0]);
    }

    private final Map<String, LoadedGrammar> grammars = new HashMap<>();
    private final PrintWriter out;

    public GrammarDriver(PrintWriter out) {
        this.out = out;
    }

    public static void main(String[] args) throws Exception {
        PrintWriter out = new PrintWriter(new BufferedOutputStream(System.out), true);
        GrammarDriver driver = new GrammarDriver(out);

        // CLI batch mode: java -jar driver.jar batch <grammar-dir> <source-dir> [--ext .clc] [--defines X,Y] [--include-paths p1,p2]
        if (args.length >= 3 && "batch".equals(args[0])) {
            driver.runBatchMode(args);
            return;
        }

        out.println("{\"ok\":true,\"ready\":true,\"version\":5}");
        out.flush();

        BufferedReader in = new BufferedReader(new InputStreamReader(System.in));
        String line;
        while ((line = in.readLine()) != null) {
            line = line.trim();
            if (line.isEmpty()) continue;
            try {
                driver.handleRequest(line);
            } catch (Exception e) {
                driver.sendError(describeException(e));
            }
        }
    }

    private void runBatchMode(String[] args) throws Exception {
        String grammarDir = args[1];
        String sourceDir = args[2];

        String extensions = null;
        String definesStr = null;
        String includePathsStr = null;
        String preprocessorCmd = null;
        boolean pipeStrings = false;
        String forceInclude = null;

        for (int i = 3; i < args.length; i++) {
            switch (args[i]) {
                case "--ext": extensions = args[++i]; break;
                case "--defines": definesStr = args[++i]; break;
                case "--include-paths": includePathsStr = args[++i]; break;
                case "--preprocessor": preprocessorCmd = args[++i]; break;
                case "--pipe-strings": pipeStrings = true; break;
                case "--force-include": forceInclude = args[++i]; break;
            }
        }

        Path gDir = Paths.get(grammarDir).toAbsolutePath();
        Path lexerFile = null, parserFile = null;
        String entryRule = "program";
        String[] fallbackRules = null;
        String extractorClass = null;

        Path tomlFile = gDir.resolve("plugin.toml");
        if (!Files.exists(tomlFile)) {
            System.err.println("ERROR: plugin.toml not found in " + grammarDir);
            System.exit(1);
        }
        for (String line : Files.readAllLines(tomlFile)) {
            line = line.trim();
            if (line.startsWith("lexer")) {
                String val = line.split("=", 2)[1].trim().replace("\"", "");
                lexerFile = gDir.resolve(val);
            } else if (line.startsWith("parser")) {
                String val = line.split("=", 2)[1].trim().replace("\"", "");
                parserFile = gDir.resolve(val);
            } else if (line.startsWith("entry_rule")) {
                entryRule = line.split("=", 2)[1].trim().replace("\"", "");
            } else if (line.startsWith("fallback_entry_rules")) {
                String val = line.split("=", 2)[1].trim();
                val = val.replaceAll("[\\[\\]\"]", "");
                fallbackRules = val.split(",\\s*");
            } else if (line.startsWith("extensions") && extensions == null) {
                String val = line.split("=", 2)[1].trim();
                val = val.replaceAll("[\\[\\]\"]", "").trim();
                extensions = val;
            } else if (line.startsWith("extractor")) {
                extractorClass = line.split("=", 2)[1].trim().replace("\"", "");
                if (extractorClass.endsWith(".java")) {
                    extractorClass = gDir.resolve(extractorClass).toString();
                }
            } else if (line.startsWith("preprocessor") && !line.contains("_")) {
                String val = line.split("=", 2)[1].trim().replace("\"", "");
                if ("c".equals(val) && preprocessorCmd == null) preprocessorCmd = "mcpp -W0";
            } else if (line.startsWith("pipe_strings")) {
                String val = line.split("=", 2)[1].trim();
                pipeStrings = pipeStrings || "true".equals(val);
            }
        }
        if (lexerFile == null || parserFile == null) {
            System.err.println("ERROR: plugin.toml missing 'lexer' or 'parser' fields in " + grammarDir);
            System.exit(1);
        }
        if (extensions == null || extensions.isEmpty()) {
            System.err.println("ERROR: plugin.toml missing 'extensions' field in " + grammarDir);
            System.exit(1);
        }

        // Load grammar directly
        Path tokensFile = lexerFile.getParent().resolve(
            lexerFile.getFileName().toString().replace(".g4", ".tokens"));
        if (!Files.exists(tokensFile)) {
            org.antlr.v4.Tool tool = new org.antlr.v4.Tool(
                new String[]{"-o", lexerFile.getParent().toString(), lexerFile.toString()});
            tool.processGrammarsOnCommandLine();
        }

        LexerGrammar lexerGrammar = (LexerGrammar) Grammar.load(lexerFile.toString());
        if (lexerGrammar == null || lexerGrammar.atn == null) {
            System.err.println("ERROR: Failed to load lexer grammar: " + lexerFile);
            System.exit(1);
        }
        Grammar parserGrammar = Grammar.load(parserFile.toString());
        if (parserGrammar == null || parserGrammar.atn == null) {
            System.err.println("ERROR: Failed to load parser grammar (ATN null): " + parserFile);
            System.exit(1);
        }

        LoadedGrammar loaded = new LoadedGrammar();
        loaded.lexer = lexerGrammar;
        loaded.parser = parserGrammar;
        loaded.entryRule = entryRule;
        loaded.fallbackEntryRules = fallbackRules;
        loaded.ruleNames = parserGrammar.getRuleNames();

        if (preprocessorCmd != null) {
            PreprocessorConfig pp = new PreprocessorConfig();
            pp.cmd = preprocessorCmd.split("\\s+");
            pp.pipeStrings = pipeStrings;
            pp.forceInclude = forceInclude;
            loaded.preprocessor = pp;
        }

        if (extractorClass != null) {
            loaded.extractor = resolveExtractor(extractorClass);
        }

        grammars.put("batch", loaded);

        // Find source files
        Set<String> extSet = new HashSet<>();
        for (String e : extensions.split(",")) {
            e = e.trim();
            if (!e.isEmpty()) extSet.add(e);
        }
        List<Path> files = new ArrayList<>();
        Files.walk(Paths.get(sourceDir)).forEach(p -> {
            String name = p.getFileName().toString();
            int dot = name.lastIndexOf('.');
            if (dot >= 0 && extSet.contains(name.substring(dot))) files.add(p);
        });
        Collections.sort(files);

        long t0 = System.currentTimeMillis();
        java.util.concurrent.atomic.AtomicInteger totalSyms = new java.util.concurrent.atomic.AtomicInteger();
        java.util.concurrent.atomic.AtomicInteger totalRels = new java.util.concurrent.atomic.AtomicInteger();
        java.util.concurrent.atomic.AtomicInteger cleanCount = new java.util.concurrent.atomic.AtomicInteger();

        int nThreads = Math.min(Runtime.getRuntime().availableProcessors(), files.size());
        java.util.concurrent.ExecutorService pool =
            java.util.concurrent.Executors.newFixedThreadPool(nThreads);
        final String ppDefines = definesStr;
        final String ppIncludes = includePathsStr;
        final PreprocessorConfig pp = loaded.preprocessor;
        final int startRule = loaded.parser.getRule(loaded.entryRule).index;
        final int[] fallbackStartRules;
        if (loaded.fallbackEntryRules != null) {
            fallbackStartRules = new int[loaded.fallbackEntryRules.length];
            for (int fi = 0; fi < loaded.fallbackEntryRules.length; fi++) {
                fallbackStartRules[fi] = loaded.parser.getRule(loaded.fallbackEntryRules[fi]).index;
            }
        } else {
            fallbackStartRules = null;
        }
        final LexerGrammar lexerG = loaded.lexer;
        final Grammar parserG = loaded.parser;
        final String[] ruleNames = loaded.ruleNames;
        final BaseExtractor ext = loaded.extractor;

        String[][] results = new String[files.size()][];
        java.util.concurrent.CountDownLatch latch =
            new java.util.concurrent.CountDownLatch(files.size());
        for (int i = 0; i < files.size(); i++) {
            final int idx = i;
            pool.submit(() -> {
                String filePath = files.get(idx).toString();
                try {
                    String source = readSourceFile(files.get(idx));
                    Map<Integer, SourceMapping> sourceMap = null;
                    if (pp != null) {
                        sourceMap = new HashMap<>();
                        source = runPreprocessor(pp, source, filePath,
                            ppDefines, ppIncludes, sourceMap);
                    }

                    LexerInterpreter lexer = lexerG.createLexerInterpreter(
                        CharStreams.fromString(source));
                    lexer.removeErrorListeners();
                    CountingErrorListener lexerErrors = new CountingErrorListener();
                    lexer.addErrorListener(lexerErrors);

                    CommonTokenStream tokens = new CommonTokenStream(lexer);
                    ParserInterpreter parser = parserG.createParserInterpreter(tokens);
                    parser.removeErrorListeners();
                    CountingErrorListener parserErrors = new CountingErrorListener();
                    parser.addErrorListener(parserErrors);
                    ParseTree tree = parser.parse(startRule);

                    // If primary rule failed and fallbacks exist, try them
                    if (parserErrors.count > 0 && fallbackStartRules != null) {
                        for (int fb : fallbackStartRules) {
                            lexer = lexerG.createLexerInterpreter(CharStreams.fromString(source));
                            lexer.removeErrorListeners();
                            CountingErrorListener fbLexerErrors = new CountingErrorListener();
                            lexer.addErrorListener(fbLexerErrors);
                            tokens = new CommonTokenStream(lexer);
                            parser = parserG.createParserInterpreter(tokens);
                            parser.removeErrorListeners();
                            CountingErrorListener fbParserErrors = new CountingErrorListener();
                            parser.addErrorListener(fbParserErrors);
                            ParseTree fbTree = parser.parse(fb);
                            if (fbParserErrors.count < parserErrors.count) {
                                tree = fbTree;
                                parserErrors = fbParserErrors;
                                lexerErrors = fbLexerErrors;
                                break;
                            }
                        }
                    }

                    BaseExtractor threadExt;
                    synchronized (ext) {
                        threadExt = ext.getClass().getDeclaredConstructor().newInstance();
                    }
                    BaseExtractor.ExtractContext ctx = new BaseExtractor.ExtractContext(
                        filePath, ruleNames, source);
                    applySourceMap(ctx, sourceMap);
                    threadExt.init(ctx, source);
                    threadExt.extract(tree, tokens, ctx);

                    totalSyms.addAndGet(ctx.symbols.size());
                    totalRels.addAndGet(ctx.relations.size());
                    if (parserErrors.count == 0) cleanCount.incrementAndGet();

                    StringBuilder rb = new StringBuilder(256);
                    rb.append("{\"file\":\"").append(escapeJson(filePath))
                      .append("\",\"symbols\":").append(ctx.symbols.size())
                      .append(",\"relations\":").append(ctx.relations.size())
                      .append(",\"lexer_errors\":").append(lexerErrors.count)
                      .append(",\"parser_errors\":").append(parserErrors.count);
                    if (!parserErrors.messages.isEmpty()) {
                        rb.append(",\"parser_error_msgs\":[");
                        for (int m = 0; m < parserErrors.messages.size(); m++) {
                            if (m > 0) rb.append(",");
                            rb.append("\"").append(escapeJson(parserErrors.messages.get(m))).append("\"");
                        }
                        rb.append("]");
                    }
                    rb.append("}");
                    results[idx] = new String[]{ rb.toString() };
                } catch (Exception e) {
                    results[idx] = new String[]{
                        "{\"file\":\"" + escapeJson(filePath) +
                        "\",\"error\":\"" + escapeJson(describeException(e)) + "\"}"
                    };
                } finally {
                    latch.countDown();
                }
            });
        }
        latch.await();
        pool.shutdown();

        for (String[] r : results) {
            if (r != null) { out.println(r[0]); out.flush(); }
        }
        long elapsed = System.currentTimeMillis() - t0;
        out.println("{\"batch_done\":true,\"files\":" + files.size() +
            ",\"clean\":" + cleanCount.get() +
            ",\"symbols\":" + totalSyms.get() +
            ",\"relations\":" + totalRels.get() +
            ",\"elapsed_ms\":" + elapsed + "}");
        out.flush();
    }

    private void handleRequest(String json) throws Exception {
        JsonObject req = JsonObject.parse(json);
        String cmd = req.getString("cmd");

        switch (cmd) {
            case "load": handleLoad(req); break;
            case "set_extractor": handleSetExtractor(req); break;
            case "extract": handleExtract(req); break;
            case "extract_batch": handleExtractBatch(req); break;
            case "parse": handleParse(req); break;
            case "shutdown":
                out.println("{\"ok\":true,\"shutdown\":true}");
                out.flush();
                System.exit(0);
                break;
            default:
                sendError("Unknown command: " + cmd);
        }
    }

    private void handleLoad(JsonObject req) throws Exception {
        String id = req.getString("id");
        String lexerPath = req.getString("lexer");
        String parserPath = req.getString("parser");
        String entryRule = req.getString("entry_rule");

        Path lexerFile = Paths.get(lexerPath);
        Path tokensFile = lexerFile.getParent().resolve(
            lexerFile.getFileName().toString().replace(".g4", ".tokens"));

        if (!Files.exists(tokensFile)) {
            Tool tool = new Tool(new String[]{"-o", lexerFile.getParent().toString(), lexerPath});
            tool.processGrammarsOnCommandLine();
        }

        LexerGrammar lexerGrammar = (LexerGrammar) Grammar.load(lexerPath);
        if (lexerGrammar == null || lexerGrammar.atn == null) {
            sendError("Failed to load lexer grammar: " + lexerPath);
            return;
        }

        Grammar parserGrammar = Grammar.load(parserPath);
        if (parserGrammar == null || parserGrammar.atn == null) {
            sendError("Failed to load parser grammar (ATN null): " + parserPath);
            return;
        }

        if (parserGrammar.getRule(entryRule) == null) {
            sendError("Entry rule '" + entryRule + "' not found in parser grammar");
            return;
        }

        LoadedGrammar loaded = new LoadedGrammar();
        loaded.lexer = lexerGrammar;
        loaded.parser = parserGrammar;
        loaded.entryRule = entryRule;
        loaded.ruleNames = parserGrammar.getRuleNames();

        String fallbackStr = req.getString("fallback_entry_rules");
        if (fallbackStr != null && !fallbackStr.isEmpty()) {
            loaded.fallbackEntryRules = fallbackStr.split(",\\s*");
        }

        String ppCmd = req.getString("preprocessor_cmd");
        if (ppCmd == null || ppCmd.isEmpty()) {
            String pp = req.getString("preprocessor");
            if ("c".equals(pp)) ppCmd = "mcpp -W0";
        }
        if (ppCmd != null && !ppCmd.isEmpty()) {
            PreprocessorConfig pp = new PreprocessorConfig();
            pp.cmd = ppCmd.split("\\s+");
            String df = req.getString("preprocessor_define_flag");
            if (df != null && !df.isEmpty()) pp.defineFlag = df;
            String inf = req.getString("preprocessor_include_flag");
            if (inf != null && !inf.isEmpty()) pp.includeFlag = inf;
            pp.lineMarkers = !"false".equals(req.getString("preprocessor_line_markers"));
            pp.pipeStrings = "true".equals(req.getString("preprocessor_pipe_strings"));
            String fi = req.getString("preprocessor_force_include");
            if (fi != null && !fi.isEmpty()) pp.forceInclude = fi;
            loaded.preprocessor = pp;
        }
        loaded.emitReferencedFormImports = "true".equals(req.getString("emit_referenced_form_imports"));
        grammars.put(id, loaded);

        out.println("{\"ok\":true,\"id\":\"" + escapeJson(id) +
            "\",\"lexer_rules\":" + lexerGrammar.rules.size() +
            ",\"parser_rules\":" + parserGrammar.rules.size() + "}");
        out.flush();
    }

    private void handleSetExtractor(JsonObject req) {
        String id = req.getString("id");
        String className = req.getString("class");
        LoadedGrammar loaded = grammars.get(id);
        if (loaded == null) { sendError("Grammar not loaded: " + id); return; }

        if ("GenericExtractor".equals(className)) {
            String mappings = req.getString("mappings");
            if (mappings == null || mappings.isEmpty()) {
                sendError("GenericExtractor requires 'mappings' field");
                return;
            }
            loaded.extractor = parseGenericExtractor(mappings);
        } else {
            try {
                loaded.extractor = resolveExtractor(className);
            } catch (Exception e) {
                sendError("Failed to load extractor '" + className + "': " + e.getMessage());
                return;
            }
        }

        out.println("{\"ok\":true}");
        out.flush();
    }

    private BaseExtractor parseGenericExtractor(String mappings) {
        com.infigraph.driver.extractors.GenericExtractor ext =
            new com.infigraph.driver.extractors.GenericExtractor();

        for (String line : mappings.split("\\|")) {
            line = line.trim();
            if (line.isEmpty()) continue;

            String[] parts = line.split(":", -1);
            if (parts.length < 4) continue;

            String type = parts[0];
            String rule = parts[1];

            if ("S".equals(type)) {
                // S:rule:kind:nameSpec[:flags]
                // nameSpec: "identifier" or "sectionDecl>identifier" (path) or "identifier#1" (index)
                com.infigraph.driver.extractors.GenericExtractor.SymbolMapping m = new com.infigraph.driver.extractors.GenericExtractor.SymbolMapping();
                m.rule = rule;
                m.kind = parts[2];
                parseNameSpec(parts[3], m);

                for (int i = 4; i < parts.length; i++) {
                    String flag = parts[i];
                    if ("scope".equals(flag)) m.scope = true;
                    else if ("fq".equals(flag)) m.formQualified = true;
                    else if (flag.startsWith("split=")) m.split = flag.substring(6);
                }
                ext.addSymbolMapping(m);

            } else if ("R".equals(type)) {
                // R:rule:kind:targetSpec[:fallback=X]
                com.infigraph.driver.extractors.GenericExtractor.RelationMapping m = new com.infigraph.driver.extractors.GenericExtractor.RelationMapping();
                m.rule = rule;
                m.kind = parts[2];
                parseTargetSpec(parts[3], m);

                for (int i = 4; i < parts.length; i++) {
                    String flag = parts[i];
                    if (flag.startsWith("fallback=")) m.targetFallback = flag.substring(9);
                }
                ext.addRelationMapping(m);

            } else if ("O".equals(type)) {
                // O:scan_form_names
                if ("scan_form_names".equals(rule)) {
                    ext.setScanFormNames(true);
                }
            }
        }

        return ext;
    }

    private void parseNameSpec(String spec, com.infigraph.driver.extractors.GenericExtractor.SymbolMapping m) {
        if (spec.contains(">")) {
            m.namePath = spec.split(">");
        } else if (spec.contains("#")) {
            int hashIdx = spec.lastIndexOf('#');
            m.nameFrom = spec.substring(0, hashIdx);
            m.nameIndex = Integer.parseInt(spec.substring(hashIdx + 1));
        } else {
            m.nameFrom = spec;
        }
    }

    private void parseTargetSpec(String spec, com.infigraph.driver.extractors.GenericExtractor.RelationMapping m) {
        if (spec.contains(">")) {
            m.targetPath = spec.split(">");
        } else if (spec.contains("#")) {
            int hashIdx = spec.lastIndexOf('#');
            m.targetFrom = spec.substring(0, hashIdx);
            m.targetIndex = Integer.parseInt(spec.substring(hashIdx + 1));
        } else {
            m.targetFrom = spec;
        }
    }

    // --- extract: preprocess + parse + extractor ---

    private void handleExtract(JsonObject req) throws Exception {
        String id = req.getString("id");
        String file = req.getString("file");
        String source = req.getString("source");
        String definesStr = req.getString("defines");
        String includePathsStr = req.getString("include_paths");

        LoadedGrammar loaded = grammars.get(id);
        if (loaded == null) { sendError("Grammar not loaded: " + id); return; }
        if (loaded.extractor == null) { sendError("No extractor set for grammar: " + id); return; }

        Map<Integer, SourceMapping> sourceMap = null;
        if (loaded.preprocessor != null) {
            try {
                sourceMap = new HashMap<>();
                source = runPreprocessor(loaded.preprocessor, source, file, definesStr, includePathsStr, sourceMap);
            } catch (Exception e) {
                System.err.println("[PREPROC] Preprocessor failed for " + file + ": " + e.getMessage());
                sourceMap = null;
            }
        }

        LexerInterpreter lexer = loaded.lexer.createLexerInterpreter(
            CharStreams.fromString(source));
        lexer.removeErrorListeners();
        CountingErrorListener lexerErrors = new CountingErrorListener();
        lexer.addErrorListener(lexerErrors);

        CommonTokenStream tokens = new CommonTokenStream(lexer);

        ParserInterpreter parser = loaded.parser.createParserInterpreter(tokens);
        parser.removeErrorListeners();
        CountingErrorListener parserErrors = new CountingErrorListener();
        parser.addErrorListener(parserErrors);

        int startRule = loaded.parser.getRule(loaded.entryRule).index;
        ParseTree tree = parser.parse(startRule);

        if (parserErrors.count > 0 && loaded.fallbackEntryRules != null) {
            for (String fbRule : loaded.fallbackEntryRules) {
                int fb = loaded.parser.getRule(fbRule).index;
                LexerInterpreter fbLexer = loaded.lexer.createLexerInterpreter(
                    CharStreams.fromString(source));
                fbLexer.removeErrorListeners();
                CountingErrorListener fbLexerErrors = new CountingErrorListener();
                fbLexer.addErrorListener(fbLexerErrors);
                CommonTokenStream fbTokens = new CommonTokenStream(fbLexer);
                ParserInterpreter fbParser = loaded.parser.createParserInterpreter(fbTokens);
                fbParser.removeErrorListeners();
                CountingErrorListener fbParserErrors = new CountingErrorListener();
                fbParser.addErrorListener(fbParserErrors);
                ParseTree fbTree = fbParser.parse(fb);
                if (fbParserErrors.count < parserErrors.count) {
                    tree = fbTree;
                    tokens = fbTokens;
                    parserErrors = fbParserErrors;
                    lexerErrors = fbLexerErrors;
                    break;
                }
            }
        }

        BaseExtractor.ExtractContext ctx = new BaseExtractor.ExtractContext(file, loaded.ruleNames, source);
        applySourceMap(ctx, sourceMap);
        try {
            loaded.extractor.init(ctx, source);
            loaded.extractor.extract(tree, tokens, ctx);
        } catch (Exception e) {
            sendError("Extractor '" + loaded.extractor.getClass().getSimpleName()
                + "' failed on file '" + file + "': " + describeException(e));
            return;
        }

        if (loaded.emitReferencedFormImports) {
            for (String formName : ctx.referencedForms) {
                BaseExtractor.RelationOut r = new BaseExtractor.RelationOut();
                r.sourceId = file;
                r.targetId = formName.toLowerCase();
                r.kind = "Imports";
                r.file = file;
                r.startLine = 0; r.startCol = 0;
                r.endLine = 0; r.endCol = 0;
                ctx.relations.add(r);
            }
        }

        StringBuilder sb = new StringBuilder(2048);
        sb.append("{\"ok\":true,\"id\":\"").append(escapeJson(id))
          .append("\",\"file\":\"").append(escapeJson(file))
          .append("\",\"lexer_errors\":").append(lexerErrors.count)
          .append(",\"parser_errors\":").append(parserErrors.count)
          .append(",\"symbols\":[");

        for (int i = 0; i < ctx.symbols.size(); i++) {
            if (i > 0) sb.append(",");
            ctx.symbols.get(i).toJson(sb);
        }
        sb.append("],\"relations\":[");
        for (int i = 0; i < ctx.relations.size(); i++) {
            if (i > 0) sb.append(",");
            ctx.relations.get(i).toJson(sb);
        }
        sb.append("],\"errors\":[");
        List<String> allErrors = new ArrayList<>();
        allErrors.addAll(lexerErrors.messages);
        allErrors.addAll(parserErrors.messages);
        for (int i = 0; i < allErrors.size(); i++) {
            if (i > 0) sb.append(",");
            sb.append("\"").append(escapeJson(allErrors.get(i))).append("\"");
        }
        sb.append("]}");

        out.println(sb.toString());
        out.flush();
    }

    // --- extract_batch: preprocess in parallel, stream results one per line ---

    private void handleExtractBatch(JsonObject req) throws Exception {
        String id = req.getString("id");
        String filesStr = req.getString("files");
        String definesStr = req.getString("defines");
        String includePathsStr = req.getString("include_paths");

        LoadedGrammar loaded = grammars.get(id);
        if (loaded == null) { sendError("Grammar not loaded: " + id); return; }
        if (loaded.extractor == null) { sendError("No extractor set for grammar: " + id); return; }

        String[] files = filesStr.split("\\|");
        int nThreads = Math.min(Runtime.getRuntime().availableProcessors(), files.length);
        ExecutorService pool = Executors.newFixedThreadPool(nThreads);

        // Phase 1: read + preprocess in parallel
        Map<String, Future<Object[]>> futures = new LinkedHashMap<>();
        for (String file : files) {
            file = file.trim();
            if (file.isEmpty()) continue;
            String f = file;
            futures.put(f, pool.submit(() -> {
                String source = readSourceFile(Paths.get(f));
                Map<Integer, SourceMapping> sm = null;
                if (loaded.preprocessor != null) {
                    sm = new HashMap<>();
                    source = runPreprocessor(loaded.preprocessor, source, f, definesStr, includePathsStr, sm);
                }
                return new Object[]{source, sm};
            }));
        }
        pool.shutdown();
        pool.awaitTermination(5, TimeUnit.MINUTES);

        // Phase 2: parse + extract sequentially, stream one JSON line per file
        for (Map.Entry<String, Future<Object[]>> entry : futures.entrySet()) {
            String file = entry.getKey();
            try {
                Object[] result = entry.getValue().get();
                String source = (String) result[0];
                @SuppressWarnings("unchecked")
                Map<Integer, SourceMapping> sourceMap = (Map<Integer, SourceMapping>) result[1];

                LexerInterpreter lexer = loaded.lexer.createLexerInterpreter(
                    CharStreams.fromString(source));
                lexer.removeErrorListeners();
                CountingErrorListener lexerErrors = new CountingErrorListener();
                lexer.addErrorListener(lexerErrors);

                CommonTokenStream tokens = new CommonTokenStream(lexer);
                ParserInterpreter parser = loaded.parser.createParserInterpreter(tokens);
                parser.removeErrorListeners();
                CountingErrorListener parserErrors = new CountingErrorListener();
                parser.addErrorListener(parserErrors);

                int startRule = loaded.parser.getRule(loaded.entryRule).index;
                ParseTree tree = parser.parse(startRule);

                BaseExtractor.ExtractContext ctx = new BaseExtractor.ExtractContext(file, loaded.ruleNames, source);
                applySourceMap(ctx, sourceMap);
                loaded.extractor.init(ctx, source);
                loaded.extractor.extract(tree, tokens, ctx);

                if (loaded.emitReferencedFormImports) {
                    for (String formName : ctx.referencedForms) {
                        BaseExtractor.RelationOut r = new BaseExtractor.RelationOut();
                        r.sourceId = file;
                        r.targetId = formName.toLowerCase();
                        r.kind = "Imports"; r.file = file;
                        r.startLine = 0; r.startCol = 0; r.endLine = 0; r.endCol = 0;
                        ctx.relations.add(r);
                    }
                }

                StringBuilder sb = new StringBuilder(2048);
                sb.append("{\"ok\":true,\"batch\":true,\"file\":\"").append(escapeJson(file))
                  .append("\",\"lexer_errors\":").append(lexerErrors.count)
                  .append(",\"parser_errors\":").append(parserErrors.count)
                  .append(",\"symbols\":[");
                for (int i = 0; i < ctx.symbols.size(); i++) {
                    if (i > 0) sb.append(",");
                    ctx.symbols.get(i).toJson(sb);
                }
                sb.append("],\"relations\":[");
                for (int i = 0; i < ctx.relations.size(); i++) {
                    if (i > 0) sb.append(",");
                    ctx.relations.get(i).toJson(sb);
                }
                sb.append("]}");
                out.println(sb.toString());
                out.flush();
            } catch (Exception e) {
                out.println("{\"ok\":false,\"batch\":true,\"file\":\"" + escapeJson(file) +
                    "\",\"error\":\"" + escapeJson(e.getMessage()) + "\"}");
                out.flush();
            }
        }
        out.println("{\"ok\":true,\"batch_done\":true,\"id\":\"" + escapeJson(id) +
            "\",\"count\":" + futures.size() + "}");
        out.flush();
    }

    // --- Configurable preprocessor ---

    private String runPreprocessor(PreprocessorConfig pp, String source, String filePath, String definesStr, String includePathsStr, Map<Integer, SourceMapping> sourceMap) throws Exception {
        String input = pp.pipeStrings ? joinMultiLinePipeStrings(source) : source;
        if (pp.forceInclude != null && !pp.forceInclude.isEmpty()) {
            StringBuilder fi = new StringBuilder();
            for (String inc : pp.forceInclude.split(",")) {
                inc = inc.trim();
                if (!inc.isEmpty()) fi.append("#include \"").append(inc).append("\"\n");
            }
            input = fi + input;
        }

        List<String> includePaths = new ArrayList<>();
        Path sourceDir = Paths.get(filePath).getParent();
        if (sourceDir != null) includePaths.add(sourceDir.toString());
        if (includePathsStr != null && !includePathsStr.isEmpty()) {
            for (String p : includePathsStr.split(",")) {
                p = p.trim();
                if (!p.isEmpty()) includePaths.add(p);
            }
        }

        Map<String, String> defines = new LinkedHashMap<>();
        if (definesStr != null && !definesStr.isEmpty()) {
            for (String def : definesStr.split(",")) {
                def = def.trim();
                if (def.isEmpty()) continue;
                int eq = def.indexOf('=');
                if (eq >= 0) defines.put(def.substring(0, eq), def.substring(eq + 1));
                else defines.put(def, "1");
            }
        }

        if (pp.cmd.length == 1 && "builtin".equals(pp.cmd[0])) {
            return preprocessBuiltin(input, filePath, defines, includePaths, sourceMap);
        }

        return runExternalPreprocessor(pp, input, defines, includePaths, sourceMap, filePath);
    }

    private String runExternalPreprocessor(PreprocessorConfig pp, String input,
            Map<String, String> defines, List<String> includePaths,
            Map<Integer, SourceMapping> sourceMap, String filePath) throws Exception {
        List<String> cmd = new ArrayList<>();
        Collections.addAll(cmd, pp.cmd);
        for (Map.Entry<String, String> d : defines.entrySet()) {
            cmd.add(pp.defineFlag + d.getKey() + "=" + d.getValue());
        }
        for (String p : includePaths) {
            cmd.add(pp.includeFlag + p);
        }

        ProcessBuilder pb = new ProcessBuilder(cmd);
        pb.redirectErrorStream(false);
        Process proc = pb.start();

        byte[] inputBytes = input.getBytes();
        final byte[][] results = new byte[2][];
        Thread stdinThread = new Thread(() -> {
            try {
                proc.getOutputStream().write(inputBytes);
                proc.getOutputStream().close();
            } catch (IOException e) { }
        });
        Thread stdoutThread = new Thread(() -> {
            try { results[0] = proc.getInputStream().readAllBytes(); }
            catch (IOException e) { results[0] = new byte[0]; }
        });
        Thread stderrThread = new Thread(() -> {
            try { results[1] = proc.getErrorStream().readAllBytes(); }
            catch (IOException e) { results[1] = new byte[0]; }
        });
        stdinThread.start();
        stdoutThread.start();
        stderrThread.start();

        boolean finished = proc.waitFor(30, java.util.concurrent.TimeUnit.SECONDS);
        if (!finished) {
            proc.destroyForcibly();
            stdinThread.join(1000);
            stdoutThread.join(1000);
            stderrThread.join(1000);
            throw new Exception("Preprocessor timed out after 30s: " + pp.cmd[0]);
        }
        stdinThread.join(5000);
        stdoutThread.join(5000);
        stderrThread.join(5000);

        String output = new String(results[0] != null ? results[0] : new byte[0]);
        String errors = new String(results[1] != null ? results[1] : new byte[0]);
        if (!errors.isEmpty()) {
            System.err.println("[PREPROC] stderr for " + filePath + ": " +
                errors.lines().limit(5).reduce("", (a, b) -> a + "\n" + b).trim());
        }
        if (pp.lineMarkers) return stripLineMarkers(output, sourceMap);
        return output;
    }

    // --- Built-in C preprocessor (in-process, no external process spawn) ---

    private String preprocessBuiltin(String source, String filePath,
            Map<String, String> defines, List<String> includePaths,
            Map<Integer, SourceMapping> sourceMap) throws Exception {
        Map<String, String> defs = new LinkedHashMap<>(defines);
        StringBuilder out = new StringBuilder(source.length());
        outputLineCounter = 0;
        processLines(source.split("\n", -1), filePath, defs, includePaths, sourceMap, out,
            new ArrayDeque<>(), 1, new HashSet<>());
        if (out.length() > 0 && out.charAt(out.length() - 1) == '\n') {
            out.setLength(out.length() - 1);
        }
        return out.toString();
    }

    private int outputLineCounter;

    private void processLines(String[] lines, String filePath,
            Map<String, String> defs, List<String> includePaths,
            Map<Integer, SourceMapping> sourceMap, StringBuilder out,
            Deque<Boolean> ifStack, int startSourceLine, Set<String> includedFiles) {
        boolean active = ifStack.stream().allMatch(b -> b);

        for (int i = 0; i < lines.length; i++) {
            String line = lines[i];
            String trimmed = line.trim();
            int sourceLine = startSourceLine + i;

            // Strip // comments from directive lines
            String directiveLine = trimmed;
            if (trimmed.startsWith("#")) {
                int commentIdx = findDirectiveComment(trimmed);
                if (commentIdx >= 0) directiveLine = trimmed.substring(0, commentIdx).trim();
            }

            if (directiveLine.startsWith("#")) {
                String directive = directiveLine.substring(1).trim();

                if (directive.startsWith("ifdef ")) {
                    String name = directive.substring(6).trim();
                    active = ifStack.stream().allMatch(b -> b);
                    ifStack.push(active && defs.containsKey(name));
                    active = ifStack.stream().allMatch(b -> b);
                    continue;
                } else if (directive.startsWith("ifndef ")) {
                    String name = directive.substring(7).trim();
                    active = ifStack.stream().allMatch(b -> b);
                    ifStack.push(active && !defs.containsKey(name));
                    active = ifStack.stream().allMatch(b -> b);
                    continue;
                } else if (directive.startsWith("if ")) {
                    String expr = directive.substring(3).trim();
                    active = ifStack.stream().allMatch(b -> b);
                    ifStack.push(active && evalIfExpr(expr, defs));
                    active = ifStack.stream().allMatch(b -> b);
                    continue;
                } else if (directive.startsWith("elif ")) {
                    if (!ifStack.isEmpty()) {
                        boolean prev = ifStack.pop();
                        boolean parentActive = ifStack.stream().allMatch(b -> b);
                        String expr = directive.substring(5).trim();
                        ifStack.push(parentActive && !prev && evalIfExpr(expr, defs));
                        active = ifStack.stream().allMatch(b -> b);
                    }
                    continue;
                } else if (directive.equals("else")) {
                    if (!ifStack.isEmpty()) {
                        boolean prev = ifStack.pop();
                        boolean parentActive = ifStack.stream().allMatch(b -> b);
                        ifStack.push(parentActive && !prev);
                        active = ifStack.stream().allMatch(b -> b);
                    }
                    continue;
                } else if (directive.startsWith("endif")) {
                    if (!ifStack.isEmpty()) ifStack.pop();
                    active = ifStack.stream().allMatch(b -> b);
                    continue;
                }

                if (!active) continue;

                if (directive.startsWith("define ")) {
                    String rest = directive.substring(7).trim();
                    // Skip function-like macros — pass through as-is
                    int paren = rest.indexOf('(');
                    int space = rest.indexOf(' ');
                    int tab = rest.indexOf('\t');
                    if (paren >= 0 && (space < 0 || paren < space) && (tab < 0 || paren < tab)) {
                        // Function-like macro — don't store, emit as blank line
                        // Handle backslash continuations
                        while (i < lines.length - 1 && lines[i].endsWith("\\")) i++;
                        continue;
                    }
                    if (space >= 0) {
                        defs.put(rest.substring(0, space), rest.substring(space + 1).trim());
                    } else if (tab >= 0) {
                        defs.put(rest.substring(0, tab), rest.substring(tab + 1).trim());
                    } else {
                        defs.put(rest, "1");
                    }
                    continue;
                } else if (directive.startsWith("undef ")) {
                    defs.remove(directive.substring(6).trim());
                    continue;
                } else if (directive.startsWith("include ")) {
                    String inc = directive.substring(8).trim();
                    if (inc.startsWith("\"") && inc.endsWith("\"")) {
                        inc = inc.substring(1, inc.length() - 1);
                    }
                    String resolved = resolveInclude(inc, filePath, includePaths);
                    if (resolved != null && !includedFiles.contains(resolved)) {
                        includedFiles.add(resolved);
                        try {
                            String content = readSourceFile(Paths.get(resolved));
                            String[] incLines = content.split("\n", -1);
                            processLines(incLines, resolved, defs, includePaths,
                                sourceMap, out, ifStack, 1, includedFiles);
                        } catch (IOException e) {
                            System.err.println("[PREPROC] Include not readable: " + resolved);
                        }
                    }
                    continue;
                }
                // Other directives (#error, #pragma, etc.) — skip
                continue;
            }

            if (!active) continue;

            // Emit line with simple macro substitution
            String expanded = expandSimpleMacros(line, defs);
            outputLineCounter++;
            if (sourceMap != null) {
                sourceMap.put(outputLineCounter, new SourceMapping(filePath, sourceLine));
            }
            out.append(expanded).append('\n');
        }
    }

    private static int findDirectiveComment(String line) {
        boolean inString = false;
        char strChar = 0;
        for (int i = 0; i < line.length() - 1; i++) {
            char c = line.charAt(i);
            if (inString) {
                if (c == strChar && (i == 0 || line.charAt(i - 1) != '\\')) inString = false;
            } else {
                if (c == '"' || c == '\'') { inString = true; strChar = c; }
                else if (c == '/' && line.charAt(i + 1) == '/') return i;
            }
        }
        return -1;
    }

    private static boolean evalIfExpr(String expr, Map<String, String> defs) {
        expr = expr.trim();
        // Handle || (OR)
        int orIdx = findOperator(expr, "||");
        if (orIdx >= 0) {
            return evalIfExpr(expr.substring(0, orIdx), defs) ||
                   evalIfExpr(expr.substring(orIdx + 2), defs);
        }
        // Handle && (AND)
        int andIdx = findOperator(expr, "&&");
        if (andIdx >= 0) {
            return evalIfExpr(expr.substring(0, andIdx), defs) &&
                   evalIfExpr(expr.substring(andIdx + 2), defs);
        }
        // Handle !
        if (expr.startsWith("!")) {
            return !evalIfExpr(expr.substring(1).trim(), defs);
        }
        // Handle defined(X) or defined X
        if (expr.startsWith("defined")) {
            String arg = expr.substring(7).trim();
            if (arg.startsWith("(") && arg.endsWith(")")) {
                arg = arg.substring(1, arg.length() - 1).trim();
            }
            return defs.containsKey(arg);
        }
        // Handle comparison: NAME == VALUE, NAME != VALUE
        int eqIdx = expr.indexOf("==");
        if (eqIdx >= 0) {
            String left = resolveValue(expr.substring(0, eqIdx).trim(), defs);
            String right = resolveValue(expr.substring(eqIdx + 2).trim(), defs);
            return left.equals(right);
        }
        int neIdx = expr.indexOf("!=");
        if (neIdx >= 0) {
            String left = resolveValue(expr.substring(0, neIdx).trim(), defs);
            String right = resolveValue(expr.substring(neIdx + 2).trim(), defs);
            return !left.equals(right);
        }
        // Bare identifier — true if defined and non-zero
        String val = defs.getOrDefault(expr, "0");
        return !"0".equals(val) && !val.isEmpty();
    }

    private static int findOperator(String expr, String op) {
        int depth = 0;
        for (int i = 0; i < expr.length() - op.length() + 1; i++) {
            char c = expr.charAt(i);
            if (c == '(') depth++;
            else if (c == ')') depth--;
            else if (depth == 0 && expr.startsWith(op, i)) return i;
        }
        return -1;
    }

    private static String resolveValue(String v, Map<String, String> defs) {
        return defs.getOrDefault(v, v);
    }

    private static String expandSimpleMacros(String line, Map<String, String> defs) {
        if (defs.isEmpty()) return line;
        for (Map.Entry<String, String> e : defs.entrySet()) {
            String name = e.getKey();
            String val = e.getValue();
            if ("1".equals(val) || name.equals(val)) continue;
            int idx = 0;
            while ((idx = line.indexOf(name, idx)) >= 0) {
                if (idx > 0 && Character.isLetterOrDigit(line.charAt(idx - 1))) { idx++; continue; }
                int end = idx + name.length();
                if (end < line.length() && Character.isLetterOrDigit(line.charAt(end))) { idx++; continue; }
                line = line.substring(0, idx) + val + line.substring(end);
                idx += val.length();
            }
        }
        return line;
    }

    private static String resolveInclude(String name, String currentFile, List<String> includePaths) {
        // Try relative to current file first
        Path dir = Paths.get(currentFile).getParent();
        if (dir != null) {
            Path p = dir.resolve(name);
            if (Files.exists(p)) return p.toString();
        }
        for (String incDir : includePaths) {
            Path p = Paths.get(incDir).resolve(name);
            if (Files.exists(p)) return p.toString();
        }
        return null;
    }

    private static final Pattern LINE_MARKER = Pattern.compile(
        "^#(?:line)?\\s+(\\d+)\\s+\"(.+?)\"");

    private static String stripLineMarkers(String output, Map<Integer, SourceMapping> sourceMap) {
        StringBuilder sb = new StringBuilder(output.length());
        String currentFile = null;
        int currentSourceLine = 1;
        int outputLine = 1;

        for (String line : output.split("\n", -1)) {
            Matcher m = LINE_MARKER.matcher(line);
            if (m.find()) {
                currentSourceLine = Integer.parseInt(m.group(1));
                currentFile = m.group(2);
                continue;
            }
            if (sourceMap != null && currentFile != null) {
                sourceMap.put(outputLine, new SourceMapping(currentFile, currentSourceLine));
            }
            sb.append(line).append('\n');
            outputLine++;
            currentSourceLine++;
        }

        if (sb.length() > 0 && sb.charAt(sb.length() - 1) == '\n') {
            sb.setLength(sb.length() - 1);
        }
        return sb.toString();
    }

    private static String joinMultiLinePipeStrings(String source) {
        int idx = 0;
        int len = source.length();
        StringBuilder sb = new StringBuilder(len);
        while (idx < len) {
            int start = source.indexOf("\"|", idx);
            if (start < 0) {
                sb.append(source, idx, len);
                break;
            }
            sb.append(source, idx, start);
            int end = source.indexOf("|\"", start + 2);
            if (end < 0) {
                sb.append(source, start, len);
                break;
            }
            end += 2;
            String pipeStr = source.substring(start, end);
            sb.append(pipeStr.replace("\n", " ").replace("\r", " "));
            idx = end;
        }
        return sb.toString();
    }

    // --- Legacy parse command (full tree JSON) ---

    private void handleParse(JsonObject req) throws Exception {
        String id = req.getString("id");
        String file = req.getString("file");
        String source = req.getString("source");

        LoadedGrammar loaded = grammars.get(id);
        if (loaded == null) { sendError("Grammar not loaded: " + id); return; }

        LexerInterpreter lexer = loaded.lexer.createLexerInterpreter(
            CharStreams.fromString(source));
        lexer.removeErrorListeners();
        CountingErrorListener lexerErrors = new CountingErrorListener();
        lexer.addErrorListener(lexerErrors);

        CommonTokenStream tokens = new CommonTokenStream(lexer);

        ParserInterpreter parser = loaded.parser.createParserInterpreter(tokens);
        parser.removeErrorListeners();
        CountingErrorListener parserErrors = new CountingErrorListener();
        parser.addErrorListener(parserErrors);

        int startRule = loaded.parser.getRule(loaded.entryRule).index;
        ParseTree tree = parser.parse(startRule);

        StringBuilder sb = new StringBuilder(4096);
        sb.append("{\"ok\":true,\"id\":\"").append(escapeJson(id))
          .append("\",\"file\":\"").append(escapeJson(file))
          .append("\",\"lexer_errors\":").append(lexerErrors.count)
          .append(",\"parser_errors\":").append(parserErrors.count)
          .append(",\"tree\":");
        treeToJson(tree, loaded.ruleNames, tokens, sb);
        sb.append("}");

        out.println(sb.toString());
        out.flush();
    }

    private void treeToJson(ParseTree tree, String[] ruleNames, CommonTokenStream tokens, StringBuilder sb) {
        if (tree instanceof TerminalNode) {
            Token tok = ((TerminalNode) tree).getSymbol();
            sb.append("{\"type\":\"terminal\",\"text\":\"")
              .append(escapeJson(tok.getText()))
              .append("\",\"token_type\":").append(tok.getType())
              .append(",\"line\":").append(tok.getLine())
              .append(",\"col\":").append(tok.getCharPositionInLine())
              .append(",\"start\":").append(tok.getStartIndex())
              .append(",\"stop\":").append(tok.getStopIndex())
              .append("}");
        } else if (tree instanceof RuleContext) {
            RuleContext ctx = (RuleContext) tree;
            int ruleIndex = ctx.getRuleIndex();
            String ruleName = (ruleIndex >= 0 && ruleIndex < ruleNames.length)
                ? ruleNames[ruleIndex] : "unknown_" + ruleIndex;

            Token start = tokens.get(ctx.getSourceInterval().a);
            Token stop = tokens.get(ctx.getSourceInterval().b);

            sb.append("{\"type\":\"rule\",\"rule\":\"").append(escapeJson(ruleName))
              .append("\",\"rule_index\":").append(ruleIndex)
              .append(",\"start_line\":").append(start.getLine())
              .append(",\"start_col\":").append(start.getCharPositionInLine())
              .append(",\"end_line\":").append(stop.getLine())
              .append(",\"end_col\":").append(stop.getCharPositionInLine() +
                  (stop.getStopIndex() - stop.getStartIndex() + 1));

            int childCount = tree.getChildCount();
            if (childCount > 0) {
                sb.append(",\"children\":[");
                for (int i = 0; i < childCount; i++) {
                    if (i > 0) sb.append(",");
                    treeToJson(tree.getChild(i), ruleNames, tokens, sb);
                }
                sb.append("]");
            }
            sb.append("}");
        }
    }

    private void sendError(String msg) {
        out.println("{\"ok\":false,\"error\":\"" + escapeJson(msg) + "\"}");
        out.flush();
    }

    private static String escapeJson(String s) {
        if (s == null) return "";
        StringBuilder sb = new StringBuilder(s.length());
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            switch (c) {
                case '"': sb.append("\\\""); break;
                case '\\': sb.append("\\\\"); break;
                case '\n': sb.append("\\n"); break;
                case '\r': sb.append("\\r"); break;
                case '\t': sb.append("\\t"); break;
                default:
                    if (c < 0x20) {
                        sb.append(String.format("\\u%04x", (int) c));
                    } else {
                        sb.append(c);
                    }
            }
        }
        return sb.toString();
    }

    static class CountingErrorListener extends BaseErrorListener {
        int count = 0;
        List<String> messages = new ArrayList<>();
        static final int MAX_MESSAGES = 10;
        @Override
        public void syntaxError(Recognizer<?, ?> recognizer, Object offendingSymbol,
                int line, int charPositionInLine, String msg, RecognitionException e) {
            count++;
            if (messages.size() < MAX_MESSAGES) {
                messages.add("line " + line + ":" + charPositionInLine + " " + msg);
            }
        }
    }

    static class JsonObject {
        private final Map<String, String> data = new LinkedHashMap<>();

        String getString(String key) {
            return data.get(key);
        }

        static JsonObject parse(String json) {
            JsonObject obj = new JsonObject();
            json = json.trim();
            if (!json.startsWith("{") || !json.endsWith("}")) {
                throw new IllegalArgumentException("Not a JSON object");
            }
            json = json.substring(1, json.length() - 1).trim();

            int i = 0;
            while (i < json.length()) {
                while (i < json.length() && Character.isWhitespace(json.charAt(i))) i++;
                if (i >= json.length()) break;

                if (json.charAt(i) != '"') {
                    throw new IllegalArgumentException("Expected '\"' at position " + i);
                }
                int keyStart = i + 1;
                int keyEnd = findUnescapedQuote(json, keyStart);
                String key = unescape(json.substring(keyStart, keyEnd));
                i = keyEnd + 1;

                while (i < json.length() && (json.charAt(i) == ' ' || json.charAt(i) == ':')) i++;

                String value;
                if (i < json.length() && json.charAt(i) == '"') {
                    int valStart = i + 1;
                    int valEnd = findUnescapedQuote(json, valStart);
                    value = unescape(json.substring(valStart, valEnd));
                    i = valEnd + 1;
                } else {
                    int valStart = i;
                    while (i < json.length() && json.charAt(i) != ',' && json.charAt(i) != '}') i++;
                    value = json.substring(valStart, i).trim();
                }
                obj.data.put(key, value);

                while (i < json.length() && (json.charAt(i) == ',' || json.charAt(i) == ' ')) i++;
            }
            return obj;
        }

        private static int findUnescapedQuote(String s, int from) {
            for (int i = from; i < s.length(); i++) {
                if (s.charAt(i) == '\\') { i++; continue; }
                if (s.charAt(i) == '"') return i;
            }
            throw new IllegalArgumentException("Unterminated string starting at " + from);
        }

        private static String unescape(String s) {
            if (!s.contains("\\")) return s;
            StringBuilder sb = new StringBuilder(s.length());
            for (int i = 0; i < s.length(); i++) {
                if (s.charAt(i) == '\\' && i + 1 < s.length()) {
                    i++;
                    switch (s.charAt(i)) {
                        case '"': sb.append('"'); break;
                        case '\\': sb.append('\\'); break;
                        case 'n': sb.append('\n'); break;
                        case 'r': sb.append('\r'); break;
                        case 't': sb.append('\t'); break;
                        default: sb.append('\\').append(s.charAt(i));
                    }
                } else {
                    sb.append(s.charAt(i));
                }
            }
            return sb.toString();
        }
    }
}
