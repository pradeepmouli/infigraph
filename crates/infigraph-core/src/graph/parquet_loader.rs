use anyhow::Result;
use arrow::array::StringArray;
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use std::path::Path;
use std::sync::Arc;

pub fn write_edge_parquet(path: &Path, pairs: &[(&str, &str)]) -> Result<()> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("from", DataType::Utf8, false),
        Field::new("to", DataType::Utf8, false),
    ]));

    let from_arr = StringArray::from(pairs.iter().map(|(a, _)| *a).collect::<Vec<_>>());
    let to_arr = StringArray::from(pairs.iter().map(|(_, b)| *b).collect::<Vec<_>>());
    let batch = RecordBatch::try_new(schema.clone(), vec![Arc::new(from_arr), Arc::new(to_arr)])?;

    let file = std::fs::File::create(path)?;
    let mut writer = ArrowWriter::try_new(file, schema, None)?;
    writer.write(&batch)?;
    writer.close()?;
    Ok(())
}

pub fn write_node_parquet(
    path: &Path,
    fields: &[(&str, DataType)],
    columns: Vec<Arc<dyn arrow::array::Array>>,
) -> Result<()> {
    let schema = Arc::new(Schema::new(
        fields
            .iter()
            .map(|(name, dtype)| Field::new(*name, dtype.clone(), true))
            .collect::<Vec<_>>(),
    ));

    let batch = RecordBatch::try_new(schema.clone(), columns)?;
    let file = std::fs::File::create(path)?;
    let mut writer = ArrowWriter::try_new(file, schema, None)?;
    writer.write(&batch)?;
    writer.close()?;
    Ok(())
}
