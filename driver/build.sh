#!/bin/bash
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
OUT_DIR="$SCRIPT_DIR/target"
LIB_DIR="$SCRIPT_DIR/lib"
ANTLR_JAR="$LIB_DIR/antlr4-complete.jar"
JCPP_JAR="$LIB_DIR/jcpp-1.4.14.jar"
SLF4J_API_JAR="$LIB_DIR/slf4j-api-1.7.36.jar"
SLF4J_NOP_JAR="$LIB_DIR/slf4j-nop-1.7.36.jar"

mkdir -p "$LIB_DIR"

# Download dependencies if not present
if [ ! -f "$ANTLR_JAR" ]; then
    echo "Downloading ANTLR4 runtime..."
    curl -sL -o "$ANTLR_JAR" "https://www.antlr.org/download/antlr-4.13.2-complete.jar"
fi

if [ ! -f "$JCPP_JAR" ]; then
    echo "Downloading JCPP (C preprocessor)..."
    curl -sL -o "$JCPP_JAR" "https://repo1.maven.org/maven2/org/anarres/jcpp/1.4.14/jcpp-1.4.14.jar"
fi

if [ ! -f "$SLF4J_API_JAR" ]; then
    echo "Downloading slf4j-api..."
    curl -sL -o "$SLF4J_API_JAR" "https://repo1.maven.org/maven2/org/slf4j/slf4j-api/1.7.36/slf4j-api-1.7.36.jar"
fi

if [ ! -f "$SLF4J_NOP_JAR" ]; then
    echo "Downloading slf4j-nop..."
    curl -sL -o "$SLF4J_NOP_JAR" "https://repo1.maven.org/maven2/org/slf4j/slf4j-nop/1.7.36/slf4j-nop-1.7.36.jar"
fi

CP="$ANTLR_JAR:$JCPP_JAR:$SLF4J_API_JAR:$SLF4J_NOP_JAR"

# Compile
mkdir -p "$OUT_DIR"
echo "Compiling GrammarDriver..."
EXTRACTOR_DIR="$SCRIPT_DIR/src/main/java/com/infigraph/driver/extractors"
EXTRACTOR_FILES=("$EXTRACTOR_DIR/BaseExtractor.java")
for f in "$EXTRACTOR_DIR"/*Extractor.java; do
    [[ "$f" == *BaseExtractor.java ]] && continue
    EXTRACTOR_FILES+=("$f")
done

javac -cp "$CP" \
    -d "$OUT_DIR" \
    "${EXTRACTOR_FILES[@]}" \
    "$SCRIPT_DIR/src/main/java/com/infigraph/driver/GrammarDriver.java"

# Create fat jar with all dependencies bundled
echo "Building infigraph-driver.jar..."
cd "$OUT_DIR"

# Extract dependency classes
jar xf "$ANTLR_JAR"
jar xf "$JCPP_JAR"
jar xf "$SLF4J_API_JAR"
jar xf "$SLF4J_NOP_JAR"

# Create manifest
mkdir -p META-INF
echo "Main-Class: com.infigraph.driver.GrammarDriver" > META-INF/MANIFEST.MF

# Build jar
jar cfm "$SCRIPT_DIR/infigraph-driver.jar" META-INF/MANIFEST.MF \
    com/ org/ META-INF/

echo "Built: $SCRIPT_DIR/infigraph-driver.jar"
echo "Run:   java -jar $SCRIPT_DIR/infigraph-driver.jar"
