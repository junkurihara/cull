//! Synthetic ComfyUI-output fixtures (dev/test only).
//!
//! There is no real `/srv/enc` mount in development, so tests and manual
//! inspection run against fabricated PNGs built here. The same builder backs
//! both the `gen_fixtures` binary (writes into `.tmp/fixtures/` for eyeballing)
//! and the E2E integration tests (build into a tempdir for reproducibility),
//! so there is a single source of truth for the fixture layout required by the
//! spec: numbered files, date/named subfolders, prompt-bearing / plain /
//! broken PNGs, and a confusingly-named `keep` subfolder that must NOT be
//! pruned (its path differs from KEEP_DIR).

use std::fs::{self, File};
use std::io::{self, BufWriter};
use std::path::Path;

/// A realistic API-format graph with positive/negative CLIPTextEncode nodes.
pub const SAMPLE_GRAPH: &str = r#"{
  "3": {"class_type":"KSampler","inputs":{"seed":42,"steps":20,"cfg":7.0,"positive":["6",0],"negative":["7",0],"model":["4",0],"latent_image":["5",0]}},
  "4": {"class_type":"CheckpointLoaderSimple","inputs":{"ckpt_name":"sd_xl_base_1.0.safetensors"}},
  "5": {"class_type":"EmptyLatentImage","inputs":{"width":1024,"height":1024,"batch_size":1}},
  "6": {"class_type":"CLIPTextEncode","inputs":{"text":"a serene mountain lake at dawn, highly detailed","clip":["4",1]}},
  "7": {"class_type":"CLIPTextEncode","inputs":{"text":"blurry, low quality, watermark","clip":["4",1]}}
}"#;

/// Build the full fixture tree under `source` (the SOURCE_DIR / output root).
///
/// Layout produced:
/// ```text
/// <source>/
///   ComfyUI_00001_.png        (a) numbered  + (d) prompt graph -> meta extracts pos/neg
///   ComfyUI_00002_.png        (a) numbered  + plain (no text chunk) -> meta empty
///   ComfyUI_00003_.png        (a) numbered  + (d) broken prompt JSON -> raw-only
///   2026-06-01/ComfyUI_00001_.png, _00002_.png   (b) date subfolder
///   proj-a/ComfyUI_00009_.png                     (c) arbitrary-named subfolder
///   proj-a/keep/inside.png    (e) confusing "keep" (path != KEEP_DIR) -> NOT pruned
///   keep/already_kept.png     KEEP_DIR content -> pruned
///   trash/already_trashed.png TRASH_DIR content -> pruned
/// ```
pub fn build(source: &Path) -> io::Result<()> {
    // (a) numbered files at the root, doubling as the (d) metadata variants.
    write_png(
        &source.join("ComfyUI_00001_.png"),
        Some(("prompt", SAMPLE_GRAPH)),
    )?;
    write_png(&source.join("ComfyUI_00002_.png"), None)?;
    write_png(
        &source.join("ComfyUI_00003_.png"),
        Some(("prompt", "{ this is not valid json")),
    )?;

    // (b) date subfolder.
    write_png(&source.join("2026-06-01/ComfyUI_00001_.png"), None)?;
    write_png(&source.join("2026-06-01/ComfyUI_00002_.png"), None)?;

    // (c) arbitrary-named subfolder.
    write_png(&source.join("proj-a/ComfyUI_00009_.png"), None)?;

    // (e) a subfolder literally named "keep" but at a different path than
    // KEEP_DIR (<source>/keep): it must still be walked.
    write_png(&source.join("proj-a/keep/inside.png"), None)?;

    // KEEP_DIR / TRASH_DIR contents: these must be pruned from enumeration.
    write_png(&source.join("keep/already_kept.png"), None)?;
    write_png(&source.join("trash/already_trashed.png"), None)?;

    Ok(())
}

/// Write a 1x1 grayscale PNG, optionally embedding one tEXt chunk (emitted
/// before IDAT, matching where ComfyUI writes its `prompt`/`workflow`).
fn write_png(path: &Path, text: Option<(&str, &str)>) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = File::create(path)?;
    let mut encoder = png::Encoder::new(BufWriter::new(file), 1, 1);
    encoder.set_color(png::ColorType::Grayscale);
    encoder.set_depth(png::BitDepth::Eight);
    if let Some((k, t)) = text {
        encoder
            .add_text_chunk(k.to_string(), t.to_string())
            .map_err(io::Error::other)?;
    }
    let mut writer = encoder.write_header().map_err(io::Error::other)?;
    writer.write_image_data(&[0u8]).map_err(io::Error::other)?;
    Ok(())
}
