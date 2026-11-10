use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Write};
use std::path::Path;

use anyhow::{Context as _, Result, ensure};
use protobuf::Message;
use scip::types::occurrence::Typed_range;
use scip::types::{Index, MultiLineRange, Occurrence, PositionEncoding, SingleLineRange};
use serde_json::json;

pub type Range = [i32; 4];

pub fn encode_range([sl, sc, el, ec]: Range) -> Typed_range {
    if sl == el {
        Typed_range::SingleLineRange(SingleLineRange {
            line: sl,
            start_character: sc,
            end_character: ec,
            ..SingleLineRange::default()
        })
    } else {
        Typed_range::MultiLineRange(MultiLineRange {
            start_line: sl,
            start_character: sc,
            end_line: el,
            end_character: ec,
            ..MultiLineRange::default()
        })
    }
}

fn decode_range(occurrence: &Occurrence) -> Option<Range> {
    match &occurrence.typed_range {
        Some(Typed_range::SingleLineRange(r)) => {
            Some([r.line, r.start_character, r.line, r.end_character])
        }
        Some(Typed_range::MultiLineRange(r)) => {
            Some([r.start_line, r.start_character, r.end_line, r.end_character])
        }
        _ => match occurrence.range.as_slice() {
            [line, sc, ec] => Some([*line, *sc, *line, *ec]),
            [sl, sc, el, ec] => Some([*sl, *sc, *el, *ec]),
            _ => None,
        },
    }
}

// Decode both producers using the same SCIP bindings that write the native index.
// This runs outside the benchmarks.
pub fn dump(path: &Path) -> Result<()> {
    let index = Index::parse_from_reader(&mut BufReader::new(File::open(path)?))?;
    let native = index.metadata.tool_info.name == "scip-tsrs-spike";
    if native {
        let root = url::Url::parse(&index.metadata.project_root)?;
        ensure!(
            root.to_file_path().is_ok(),
            "SCIP project root must be a file URI"
        );
    }
    let mut checked_symbols = HashSet::new();
    let mut output = BufWriter::new(io::stdout().lock());
    for document in &index.documents {
        let file = &document.relative_path;
        if native {
            ensure!(
                document.position_encoding
                    == PositionEncoding::UTF16CodeUnitOffsetFromLineStart.into(),
                "missing UTF-16 position encoding for {file}"
            );
        }
        writeln!(output, "{}", json!([file]))?;
        for occurrence in &document.occurrences {
            if native {
                ensure!(
                    occurrence.typed_range.is_some() && occurrence.range.is_empty(),
                    "expected typed range for {file}"
                );
                ensure!(
                    matches!(occurrence.symbol_roles, 0 | 1),
                    "unexpected role for {file}"
                );
                if checked_symbols.insert(&occurrence.symbol) {
                    scip::symbol::parse_symbol(&occurrence.symbol).map_err(|error| {
                        anyhow::anyhow!("invalid SCIP symbol {}: {error:?}", occurrence.symbol)
                    })?;
                }
            }
            let range =
                decode_range(occurrence).with_context(|| format!("invalid range in {file}"))?;
            ensure!(
                range.iter().all(|&coord| coord >= 0) && range[..2] <= range[2..],
                "backwards range in {file}"
            );
            writeln!(
                output,
                "{}",
                json!([occurrence.symbol, file, range, occurrence.symbol_roles])
            )?;
        }
    }
    output.flush()?;
    Ok(())
}
