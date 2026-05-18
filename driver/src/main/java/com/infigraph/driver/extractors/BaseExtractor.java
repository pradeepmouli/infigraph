package com.infigraph.driver.extractors;

import org.antlr.v4.runtime.*;
import org.antlr.v4.runtime.tree.*;

import java.util.*;

public abstract class BaseExtractor {

    public static class SymbolOut {
        public String id, name, kind, file, parent, signatureHash;
        public int startLine, startCol, endLine, endCol;

        public void toJson(StringBuilder sb) {
            sb.append("{\"id\":\"").append(escapeJson(id))
              .append("\",\"name\":\"").append(escapeJson(name))
              .append("\",\"kind\":\"").append(kind)
              .append("\",\"file\":\"").append(escapeJson(file))
              .append("\",\"start_line\":").append(startLine)
              .append(",\"start_col\":").append(startCol)
              .append(",\"end_line\":").append(endLine)
              .append(",\"end_col\":").append(endCol)
              .append(",\"signature_hash\":\"").append(signatureHash).append("\"");
            if (parent != null) {
                sb.append(",\"parent\":\"").append(escapeJson(parent)).append("\"");
            }
            sb.append("}");
        }
    }

    public static class RelationOut {
        public String sourceId, targetId, kind, file;
        public int startLine, startCol, endLine, endCol;

        public void toJson(StringBuilder sb) {
            sb.append("{\"source_id\":\"").append(escapeJson(sourceId))
              .append("\",\"target_id\":\"").append(escapeJson(targetId))
              .append("\",\"kind\":\"").append(kind)
              .append("\",\"file\":\"").append(escapeJson(file))
              .append("\",\"start_line\":").append(startLine)
              .append(",\"start_col\":").append(startCol)
              .append(",\"end_line\":").append(endLine)
              .append(",\"end_col\":").append(endCol)
              .append("}");
        }
    }

    public static class ExtractContext {
        public final String file;
        public final String fileStem;
        public final String[] ruleNames;
        public final List<SymbolOut> symbols = new ArrayList<>();
        public final List<RelationOut> relations = new ArrayList<>();
        public final Deque<String> scopeStack = new ArrayDeque<>();
        public final Set<String> seenIds = new HashSet<>();
        public final List<String> formNames = new ArrayList<>();
        public final Set<String> referencedForms = new LinkedHashSet<>();

        public ExtractContext(String file, String[] ruleNames, String source) {
            this.file = file;
            this.ruleNames = ruleNames;
            String stem = file;
            int lastSlash = stem.lastIndexOf('/');
            if (lastSlash >= 0) stem = stem.substring(lastSlash + 1);
            int lastDot = stem.lastIndexOf('.');
            if (lastDot >= 0) stem = stem.substring(0, lastDot);
            stem = stem.toUpperCase();
            this.fileStem = stem;

            String moduleId = file + "::" + stem;
            seenIds.add(moduleId);
            SymbolOut mod = new SymbolOut();
            mod.id = moduleId;
            mod.name = stem;
            mod.kind = "Module";
            mod.file = file;
            mod.startLine = 1; mod.startCol = 0;
            mod.endLine = 1; mod.endCol = 0;
            mod.signatureHash = hexHash(file);
            symbols.add(mod);
        }

        public String currentScope() {
            return scopeStack.isEmpty() ? null : scopeStack.peek();
        }

        public String makeId(String name) {
            String scope = currentScope();
            return scope != null ? file + "::" + scope + "::" + name : file + "::" + name;
        }

        public String sourceId() {
            String scope = currentScope();
            return scope != null ? file + "::" + scope : file + "::" + fileStem;
        }

        public void pushSymbol(String name, String kind, int sl, int sc, int el, int ec, String text, boolean formQualified) {
            if (formQualified && currentScope() == null && !formNames.isEmpty()) {
                for (String formName : formNames) {
                    String fqId = formName + "::" + name;
                    if (!seenIds.contains(fqId)) {
                        seenIds.add(fqId);
                        SymbolOut s = new SymbolOut();
                        s.id = fqId;
                        s.name = name;
                        s.kind = kind;
                        s.file = file;
                        s.startLine = sl; s.startCol = sc;
                        s.endLine = el; s.endCol = ec;
                        s.signatureHash = hexHash(text);
                        symbols.add(s);
                    }
                }
                return;
            }
            String id = makeId(name);
            if (seenIds.contains(id)) {
                String base = id + "@L" + sl;
                id = base;
                int n = 1;
                while (seenIds.contains(id)) {
                    id = base + "#" + n;
                    n++;
                }
            }
            seenIds.add(id);
            SymbolOut s = new SymbolOut();
            s.id = id;
            s.name = name;
            s.kind = kind;
            s.file = file;
            s.startLine = sl; s.startCol = sc;
            s.endLine = el; s.endCol = ec;
            s.signatureHash = hexHash(text);
            String scope = currentScope();
            if (scope != null) s.parent = file + "::" + scope;
            symbols.add(s);
        }

        public void pushRelation(String targetName, String kind, int sl, int sc, int el, int ec) {
            RelationOut r = new RelationOut();
            r.sourceId = sourceId();
            r.targetId = file + "::" + targetName;
            r.kind = kind;
            r.file = file;
            r.startLine = sl; r.startCol = sc;
            r.endLine = el; r.endCol = ec;
            relations.add(r);
        }

        public void pushFormQualifiedRelation(String formName, String fieldName, String kind, int sl, int sc, int el, int ec, boolean trackRef) {
            RelationOut r = new RelationOut();
            r.sourceId = sourceId();
            r.targetId = formName.toUpperCase() + "::" + fieldName;
            r.kind = kind;
            r.file = file;
            r.startLine = sl; r.startCol = sc;
            r.endLine = el; r.endCol = ec;
            relations.add(r);
            if (trackRef) {
                referencedForms.add(formName.toUpperCase());
            }
        }
    }

    // --- Parse tree helpers ---

    protected static String findChildText(ParseTree tree, String childRule, String[] ruleNames) {
        for (int i = 0; i < tree.getChildCount(); i++) {
            ParseTree child = tree.getChild(i);
            if (child instanceof RuleContext) {
                int idx = ((RuleContext) child).getRuleIndex();
                if (idx >= 0 && idx < ruleNames.length && ruleNames[idx].equals(childRule)) {
                    return collectText(child);
                }
            }
        }
        return null;
    }

    protected static String findChildRawText(ParseTree tree, String childRule, String[] ruleNames) {
        for (int i = 0; i < tree.getChildCount(); i++) {
            ParseTree child = tree.getChild(i);
            if (child instanceof RuleContext) {
                int idx = ((RuleContext) child).getRuleIndex();
                if (idx >= 0 && idx < ruleNames.length && ruleNames[idx].equals(childRule)) {
                    return collectRawText(child);
                }
            }
        }
        return null;
    }

    protected static String findChildTextByIndex(ParseTree tree, String childRule, int nthOccurrence, String[] ruleNames) {
        int count = 0;
        for (int i = 0; i < tree.getChildCount(); i++) {
            ParseTree child = tree.getChild(i);
            if (child instanceof RuleContext) {
                int idx = ((RuleContext) child).getRuleIndex();
                if (idx >= 0 && idx < ruleNames.length && ruleNames[idx].equals(childRule)) {
                    if (count == nthOccurrence) {
                        return collectText(child);
                    }
                    count++;
                }
            }
        }
        return null;
    }

    protected static String findChildRawTextByIndex(ParseTree tree, String childRule, int nthOccurrence, String[] ruleNames) {
        int count = 0;
        for (int i = 0; i < tree.getChildCount(); i++) {
            ParseTree child = tree.getChild(i);
            if (child instanceof RuleContext) {
                int idx = ((RuleContext) child).getRuleIndex();
                if (idx >= 0 && idx < ruleNames.length && ruleNames[idx].equals(childRule)) {
                    if (count == nthOccurrence) {
                        return collectRawText(child);
                    }
                    count++;
                }
            }
        }
        return null;
    }

    protected static String collectText(ParseTree tree) {
        if (tree instanceof TerminalNode) {
            return ((TerminalNode) tree).getText();
        }
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < tree.getChildCount(); i++) {
            String childText = collectText(tree.getChild(i));
            if (childText != null && !childText.isEmpty()) {
                if (sb.length() > 0) sb.append(' ');
                sb.append(childText);
            }
        }
        return sb.toString();
    }

    protected static String collectRawText(ParseTree tree) {
        if (tree instanceof TerminalNode) {
            return ((TerminalNode) tree).getText();
        }
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < tree.getChildCount(); i++) {
            sb.append(collectRawText(tree.getChild(i)));
        }
        return sb.toString();
    }

    protected static boolean hasChildRule(ParseTree tree, String ruleName, String[] ruleNames) {
        for (int i = 0; i < tree.getChildCount(); i++) {
            ParseTree child = tree.getChild(i);
            if (child instanceof RuleContext) {
                int idx = ((RuleContext) child).getRuleIndex();
                if (idx >= 0 && idx < ruleNames.length && ruleNames[idx].equals(ruleName)) return true;
            }
            if (child instanceof TerminalNode) {
                if (((TerminalNode) child).getText().equals(ruleName)) return true;
            }
        }
        return false;
    }

    protected static boolean hasChildToken(ParseTree tree, String tokenText) {
        for (int i = 0; i < tree.getChildCount(); i++) {
            ParseTree child = tree.getChild(i);
            if (child instanceof TerminalNode && ((TerminalNode) child).getText().equals(tokenText)) {
                return true;
            }
        }
        return false;
    }

    protected static String hexHash(String s) {
        if (s == null) return "0000000000000000";
        long h = 0;
        for (int i = 0; i < s.length(); i++) {
            h = 31 * h + s.charAt(i);
        }
        return String.format("%016x", h);
    }

    protected static void parseFormNames(String source, List<String> formNames) {
        for (String line : source.split("\n")) {
            String trimmed = line.stripLeading();
            if (trimmed.startsWith("FORM ")) {
                String rest = trimmed.substring(5).trim();
                int dotPos = rest.indexOf('.');
                if (dotPos >= 0) {
                    String afterDot = rest.substring(dotPos + 1);
                    afterDot = afterDot.replace(";", "").trim();
                    int spacePos = afterDot.indexOf(' ');
                    String name = spacePos >= 0 ? afterDot.substring(0, spacePos) : afterDot;
                    if (!name.isEmpty()) {
                        formNames.add(name.toUpperCase());
                    }
                }
            }
        }
    }

    protected static int[] getSpan(RuleContext rc, CommonTokenStream tokens) {
        Token start = tokens.get(rc.getSourceInterval().a);
        Token stop = tokens.get(rc.getSourceInterval().b);
        return new int[] {
            start.getLine(), start.getCharPositionInLine(),
            stop.getLine(), stop.getCharPositionInLine() + (stop.getStopIndex() - stop.getStartIndex() + 1)
        };
    }

    public void init(ExtractContext ctx, String source) {
        // Override in subclasses for source-level pre-processing (e.g., FORM scanning)
    }

    public void extract(ParseTree tree, CommonTokenStream tokens, ExtractContext ctx) {
        walkTree(tree, tokens, ctx);
    }

    protected void walkTree(ParseTree tree, CommonTokenStream tokens, ExtractContext ctx) {
        if (!(tree instanceof RuleContext)) return;

        RuleContext rc = (RuleContext) tree;
        int ruleIndex = rc.getRuleIndex();
        String ruleName = (ruleIndex >= 0 && ruleIndex < ctx.ruleNames.length)
            ? ctx.ruleNames[ruleIndex] : "";

        boolean isScope = processRule(ruleName, tree, tokens, ctx);

        for (int i = 0; i < tree.getChildCount(); i++) {
            walkTree(tree.getChild(i), tokens, ctx);
        }

        if (isScope) ctx.scopeStack.pop();
    }

    protected abstract boolean processRule(String ruleName, ParseTree tree, CommonTokenStream tokens, ExtractContext ctx);

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
}
