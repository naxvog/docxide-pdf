mod clock;
mod docx;
mod error;
mod fonts;
mod geometry;
mod math;
mod model;
mod pdf;

pub use error::Error;
pub use fonts::discovery::{
    FontRequest, clear_fonts, register_font, take_font_requests, use_system_fonts,
};
pub use pdf::{BookmarkPosition, take_bookmark_positions};

use std::path::Path;
use crate::clock::Instant;

pub fn convert_docx_to_pdf(input: impl AsRef<Path>, path: impl AsRef<Path>) -> Result<(), Error> {
    reject_old_env_vars()?;
    let doc = docx::parse(input.as_ref())?;
    render_and_write(&doc, path)
}

pub fn convert_docx_bytes_to_pdf(input: &[u8], path: impl AsRef<Path>) -> Result<(), Error> {
    reject_old_env_vars()?;
    let doc = docx::parse_bytes(input)?;
    render_and_write(&doc, path)
}

/// For callers that serve or store the PDF themselves (web handlers, databases, wasm).
pub fn convert_docx_bytes_to_pdf_bytes(input: &[u8]) -> Result<Vec<u8>, Error> {
    reject_old_env_vars()?;
    pdf::render(&docx::parse_bytes(input)?)
}

/// The variables lost their old `DOCXSIDE_` prefix; failing loudly beats silently
/// dropping someone's font directories and rendering with fallbacks.
fn reject_old_env_vars() -> Result<(), Error> {
    if cfg!(target_family = "wasm") {
        return Ok(());
    }
    match old_env_var(std::env::vars_os().map(|(key, _)| key)) {
        Some(name) => Err(Error::RenamedEnvVar(name)),
        None => Ok(()),
    }
}

fn old_env_var(keys: impl Iterator<Item = std::ffi::OsString>) -> Option<String> {
    keys.filter_map(|key| key.into_string().ok())
        .find(|key| key.starts_with("DOCXSIDE_"))
}

fn render_and_write(doc: &model::Document, path: impl AsRef<Path>) -> Result<(), Error> {
    let path = path.as_ref().with_extension("pdf");
    let t0 = Instant::now();

    let bytes = pdf::render(doc)?;
    let t_render = t0.elapsed();

    std::fs::write(&path, &bytes)?;
    let t_total = t0.elapsed();

    log::info!(
        "Timing: render={:.1}ms, write={:.1}ms, total={:.1}ms (output {} bytes)",
        t_render.as_secs_f64() * 1000.0,
        (t_total - t_render).as_secs_f64() * 1000.0,
        t_total.as_secs_f64() * 1000.0,
        bytes.len(),
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_prefix_is_reported_with_its_new_name() {
        let keys = ["PATH", "DOCXIDE_FONTS", "DOCXSIDE_FONTS"].map(Into::into);
        let old = old_env_var(keys.into_iter()).unwrap();
        assert_eq!(
            Error::RenamedEnvVar(old).to_string(),
            "DOCXSIDE_FONTS is no longer read; rename it to DOCXIDE_FONTS"
        );
        assert_eq!(old_env_var(["DOCXIDE_FONTS".into()].into_iter()), None);
    }
}
