package com.infigraph.driver;

import org.antlr.v4.Tool;
import org.antlr.v4.tool.Grammar;
import org.antlr.v4.tool.LexerGrammar;
import org.antlr.v4.runtime.*;
import org.antlr.v4.runtime.tree.*;
import org.anarres.cpp.Feature;
import org.anarres.cpp.Preprocessor;
import org.anarres.cpp.StringLexerSource;
import org.anarres.cpp.VirtualFile;
import org.anarres.cpp.VirtualFileSystem;
import org.anarres.cpp.Source;

import com.infigraph.driver.extractors.BaseExtractor;

import java.io.*;
import java.nio.file.*;
import java.util.*;

public class GrammarDriver {

    private static final String HASH_PLACEHOLDER = "INFIGRAPH_HASH_OP";
    private static final String BACKSLASH_PLACEHOLDER = "INFIGRAPH_BSLASH_OP";

    static class LoadedGrammar {
        LexerGrammar lexer;
        Grammar parser;
        String entryRule;
        String[] ruleNames;
        String preprocessor;
        boolean emitReferencedFormImports;
        BaseExtractor extractor;
    }

    private final Map<String, LoadedGrammar> grammars = new HashMap<>();
    private final PrintWriter out;

    public GrammarDriver(PrintWriter out) {
        this.out = out;
    }

    public static void main(String[] args) throws Exception {
        PrintWriter out = new PrintWriter(new BufferedOutputStream(System.out), true);
        GrammarDriver driver = new GrammarDriver(out);

        out.println("{\"ok\":true,\"ready\":true,\"version\":4}");
        out.flush();

        BufferedReader in = new BufferedReader(new InputStreamReader(System.in));
        String line;
        while ((line = in.readLine()) != null) {
            line = line.trim();
            if (line.isEmpty()) continue;
            try {
                driver.handleRequest(line);
            } catch (Exception e) {
                driver.sendError(e.getMessage());
            }
        }
    }

    private void handleRequest(String json) throws Exception {
        JsonObject req = JsonObject.parse(json);
        String cmd = req.getString("cmd");

        switch (cmd) {
            case "load": handleLoad(req); break;
            case "set_extractor": handleSetExtractor(req); break;
            case "extract": handleExtract(req); break;
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
        loaded.preprocessor = req.getString("preprocessor");
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

        try {
            Class<?> cls = Class.forName("com.infigraph.driver.extractors." + className);
            loaded.extractor = (BaseExtractor) cls.getDeclaredConstructor().newInstance();
        } catch (Exception e) {
            sendError("Failed to load extractor '" + className + "': " + e.getMessage());
            return;
        }

        out.println("{\"ok\":true}");
        out.flush();
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

        if ("c".equals(loaded.preprocessor)) {
            try {
                source = preprocessC(source, file, definesStr, includePathsStr);
            } catch (Exception e) {
                // JCPP failed — fall back to raw source
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

        BaseExtractor.ExtractContext ctx = new BaseExtractor.ExtractContext(file, loaded.ruleNames, source);
        loaded.extractor.init(ctx, source);
        loaded.extractor.extract(tree, tokens, ctx);

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
        sb.append("]}");

        out.println(sb.toString());
        out.flush();
    }

    // --- C Preprocessor via JCPP ---

    private String preprocessC(String source, String filePath, String definesStr, String includePathsStr) throws Exception {
        String escaped = escapeNonDirectiveChars(source);

        Preprocessor pp = new Preprocessor();

        if (definesStr != null && !definesStr.isEmpty()) {
            for (String def : definesStr.split(",")) {
                def = def.trim();
                if (!def.isEmpty()) {
                    int eq = def.indexOf('=');
                    if (eq >= 0) {
                        pp.addMacro(def.substring(0, eq), def.substring(eq + 1));
                    } else {
                        pp.addMacro(def, "1");
                    }
                }
            }
        }

        List<String> includePaths = new ArrayList<>();
        if (includePathsStr != null && !includePathsStr.isEmpty()) {
            for (String p : includePathsStr.split(",")) {
                p = p.trim();
                if (!p.isEmpty()) includePaths.add(p);
            }
        }

        Path sourceDir = Paths.get(filePath).getParent();
        if (sourceDir != null) {
            includePaths.add(0, sourceDir.toString());
        }

        for (String path : includePaths) {
            pp.getSystemIncludePath().add(path);
            pp.getQuoteIncludePath().add(path);
        }

        pp.setFileSystem(new EscapingFileSystem(includePaths));
        pp.setListener(new LenientPreprocessorListener());
        pp.addInput(new StringLexerSource(escaped, true));
        pp.addFeature(Feature.KEEPCOMMENTS);

        StringBuilder result = new StringBuilder(source.length());
        try {
            for (;;) {
                org.anarres.cpp.Token tok = pp.token();
                if (tok.getType() == org.anarres.cpp.Token.EOF) break;
                if (tok.getType() == org.anarres.cpp.Token.INVALID) continue;
                result.append(tok.getText());
            }
        } finally {
            pp.close();
        }

        return result.toString()
            .replace(HASH_PLACEHOLDER, "#")
            .replace(BACKSLASH_PLACEHOLDER, "\\");
    }

    private static String escapeNonDirectiveChars(String source) {
        StringBuilder sb = new StringBuilder(source.length());
        String[] lines = source.split("\n", -1);
        boolean inMacroContinuation = false;
        for (int i = 0; i < lines.length; i++) {
            String line = lines[i];
            String trimmed = line.stripLeading();
            boolean isDirectiveLine = trimmed.startsWith("#") || inMacroContinuation;
            inMacroContinuation = isDirectiveLine && trimmed.endsWith("\\");
            if (isDirectiveLine) {
                sb.append(line);
            } else {
                sb.append(line.replace("#", HASH_PLACEHOLDER)
                              .replace("\\", BACKSLASH_PLACEHOLDER));
            }
            if (i < lines.length - 1) sb.append('\n');
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
        @Override
        public void syntaxError(Recognizer<?, ?> recognizer, Object offendingSymbol,
                int line, int charPositionInLine, String msg, RecognitionException e) {
            count++;
        }
    }

    static class LenientPreprocessorListener implements org.anarres.cpp.PreprocessorListener {
        @Override
        public void handleWarning(Source source, int line, int col, String msg) {}

        @Override
        public void handleError(Source source, int line, int col, String msg) {}

        @Override
        public void handleSourceChange(Source source, org.anarres.cpp.PreprocessorListener.SourceChangeEvent event) {}
    }

    static class EscapingFileSystem implements VirtualFileSystem {
        private final List<String> searchPaths;

        EscapingFileSystem(List<String> searchPaths) {
            this.searchPaths = searchPaths;
        }

        @Override
        public VirtualFile getFile(String path) {
            return new EscapingFile(Paths.get(path));
        }

        @Override
        public VirtualFile getFile(String dir, String name) {
            return new EscapingFile(Paths.get(dir, name));
        }

        private Path resolve(String name) {
            for (String base : searchPaths) {
                Path candidate = Paths.get(base, name);
                if (Files.exists(candidate)) return candidate;
            }
            Path direct = Paths.get(name);
            if (Files.exists(direct)) return direct;
            return null;
        }

        class EscapingFile implements VirtualFile {
            private final Path path;

            EscapingFile(Path path) {
                this.path = path;
            }

            @Override public boolean isFile() { return Files.isRegularFile(path); }
            @Override public String getPath() { return path.toString(); }
            @Override public String getName() { return path.getFileName().toString(); }

            @Override
            public VirtualFile getParentFile() {
                Path parent = path.getParent();
                return parent != null ? new EscapingFile(parent) : null;
            }

            @Override
            public VirtualFile getChildFile(String name) {
                return new EscapingFile(path.resolve(name));
            }

            @Override
            public Source getSource() throws IOException {
                String content = new String(Files.readAllBytes(path));
                String escaped = escapeNonDirectiveChars(content);
                return new StringLexerSource(escaped, true);
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
