use std::path::Path;

use anyhow::Result;
use infigraph_core::clone::clone_infigraph_dir;

pub(crate) fn cmd_clone(src: &Path, dst: &Path) -> Result<()> {
    clone_infigraph_dir(src, dst)?;
    println!(
        "Cloned .infigraph/ from {} to {} (locks and logs excluded).",
        src.display(),
        dst.display()
    );
    Ok(())
}
