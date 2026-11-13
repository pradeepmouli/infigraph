"""Attribute the measured corpus differences, retaining a row for every tuple."""

import argparse
from collections import Counter, defaultdict
import gzip
import json
from pathlib import Path
import re


STDLIB = re.compile(r"^scip-typescript npm typescript (\S+) lib/`([^`]+)`/(.+)$")
PROMOTED_LIBS = {
    "lib.esnext.collection.d.ts": "lib.es2025.collection.d.ts",
    "lib.esnext.float16.d.ts": "lib.es2025.float16.d.ts",
}


def read_rows(path):
    open_text = gzip.open if path.suffix == ".gz" else Path.open
    with open_text(path, "rt") as stream:
        yield from map(json.loads, stream)


def range4(coords):
    if len(coords) == 3:
        return (coords[0], coords[1], coords[0], coords[2])
    return tuple(coords)


def site(row):
    _, file, coords, role = row["tuple"]
    return file, range4(coords), role


def trace_rows(path, files):
    return {
        (row["file"], range4(row["range"])): row
        for row in read_rows(path)
        if row["file"] in files
    } if files else {}


def origins(row):
    return [
        (decl.get("file"), range4(decl["range"]) if decl["range"] else None)
        for decl in row["chosenDeclarations"]
    ]


def first_by_file(sites):
    first = {}
    for file, coords in sites:
        first[file] = min(coords, first.get(file, coords))
    return first


def classify(mode, rows, oracle_path, native_path):
    by_site = defaultdict(set)
    files = set()
    for row in rows:
        symbol = row["tuple"][0]
        by_site[row["side"], site(row)].add(symbol)
        if symbol.startswith("local ") or "typeLiteral" in symbol:
            files.add(row["tuple"][1])
    oracle = trace_rows(oracle_path, files)
    native = trace_rows(native_path, files)
    index_signature_sites = {
        key for key in native.keys() & oracle.keys()
        if oracle[key]["checkerSymbol"] is None
        and native[key]["chosenDeclarations"]
        and all(d["kind"] == "IndexSignature" for d in native[key]["chosenDeclarations"])
    }
    index_shift = first_by_file(index_signature_sites)
    union_order = first_by_file(
        key for key in native.keys() & oracle.keys()
        if origins(native[key]) != origins(oracle[key])
        and set(origins(native[key])) == set(origins(oracle[key]))
        and any(s.startswith("local ") for s, _ in native[key]["emitted"])
    )

    for row in rows:
        side = row["side"]
        symbol = row["tuple"][0]
        file, coords, _ = site(row)
        key = file, coords
        other = "native" if side == "oracle" else "oracle"
        candidates = by_site[other, site(row)]
        lib = STDLIB.fullmatch(symbol)
        if lib:
            _, lib_file, descriptors = lib.groups()
            counterparts = {
                candidate.groups()[1:] for s in candidates if (candidate := STDLIB.fullmatch(s))
            }
            if (lib_file, descriptors) in counterparts and mode == "bundled":
                cause = "TypeScript package version in standard-library symbols"
            elif mode == "shared-lib" and side == "oracle" and lib_file in PROMOTED_LIBS:
                cause = "Legacy standard-library files skipped by TS7 lib mapping"
            elif mode == "bundled" and any(
                a == lib_file and (b, descriptors) in counterparts
                or b == lib_file and (a, descriptors) in counterparts
                for a, b in PROMOTED_LIBS.items()
            ):
                cause = "Standard-library declarations moved from ESNext to ES2025"
            elif mode == "bundled" and side == "native" and lib_file.startswith("lib.es2026."):
                cause = "Additional declarations in bundled TS7 standard library"
            else:
                raise ValueError(f"unexplained standard-library difference {row}")
        elif symbol.startswith("local "):
            if key in index_signature_sites and side == "native":
                cause = "TS7 returns an index-signature declaration for process.env"
            elif (any(s.startswith("local ") for s in candidates)
                  and set(origins(oracle[key])) == set(origins(native[key]))):
                if file in index_shift and index_shift[file] < coords:
                    cause = "Local ordinals shifted by an earlier index-signature reference"
                elif file in union_order and union_order[file] <= coords:
                    cause = "Local ordinals assigned in a different union-declaration order"
                else:
                    raise ValueError(f"unexplained local ordinal change {row}")
            else:
                raise ValueError(f"unexplained local target change {row}")
        elif "typeLiteral" in symbol and any(
            re.sub(r"typeLiteral\d+:", "typeLiteral:", symbol)
            == re.sub(r"typeLiteral\d+:", "typeLiteral:", candidate)
            for candidate in candidates
        ) and set(origins(oracle[key])) == set(origins(native[key])):
            cause = "Type-literal ordinal shifted by shared counter and union order"
        else:
            raise ValueError(f"unexplained difference {row}")
        yield {**row, "cause": cause}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("bundled", "shared-lib"))
    for name in ("diff", "oracle_trace", "native_trace", "ledger"):
        parser.add_argument(name, type=Path)
    args = parser.parse_args()
    counts = defaultdict(Counter)
    with args.ledger.open("w") as output:
        for row in classify(args.mode, list(read_rows(args.diff)), args.oracle_trace, args.native_trace):
            counts[row["cause"]][row["side"]] += 1
            print(json.dumps(row), file=output)
    print(json.dumps(counts, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
