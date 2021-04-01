use super::Result;
use fn_error_context::context;

/// Import a tarball
#[context("Importing")]
pub fn import_tarball(src: impl std::io::Read) -> Result<String> {
    let ungz = flate2::read::GzDecoder::new(src);
    let mut archive = tar::Archive::new(ungz);
    for entry in archive.entries()? {
        let entry = entry?;
        dbg!(entry.path()?);
    }
    Ok("".into())
}
