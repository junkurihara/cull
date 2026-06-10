//! On-demand PNG metadata extraction (design.md §5).
//!
//! Triggered for a single image at a time (the `i` key / tap), never as part
//! of enumeration, so the cost stays off the hot path. We read the PNG text
//! chunks (tEXt/zTXt/iTXt) only up to the IDAT — image generators write their `prompt`
//! and `workflow` chunks before the image data, so pixels are never decoded.
//!
//! Extraction is layered and best-effort:
//!   1. The raw `prompt` JSON (the executed API graph) is pretty-printed. This
//!      is the baseline and is shown whenever any text chunk is present.
//!   2. On top of that we *try* to walk the graph and pull the positive /
//!      negative prompt strings from CLIPTextEncode nodes. This walk is
//!      fragile across custom nodes and multi-encoder setups, so any failure
//!      degrades silently to raw-only (never an error / panic).

use serde::Serialize;
use serde_json::{Map, Value};
use std::cmp::Ordering;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

/// Positive/negative prompts attributed to one sampler node.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct SamplerPrompt {
    pub sampler_node: String,
    pub positive: Option<String>,
    pub negative: Option<String>,
}

/// Extracted metadata for one image. `raw` is the formatted prompt JSON (or
/// the raw chunk text if it is not valid JSON); `None` means no usable text
/// chunk was found. `prompts` is the best-effort graph-walk result and may be
/// empty even when `raw` is present.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Meta {
    pub raw: Option<String>,
    pub prompts: Vec<SamplerPrompt>,
}

impl Meta {
    fn empty() -> Self {
        Meta {
            raw: None,
            prompts: Vec::new(),
        }
    }
}

/// Extract metadata from a PNG. Never panics and never returns an error: a
/// non-PNG, a PNG without text chunks, or a malformed graph all yield a
/// gracefully reduced [`Meta`] (design.md §5).
pub fn extract_meta(path: &Path) -> Meta {
    let text = match read_prompt_text(path) {
        Some(t) => t,
        None => return Meta::empty(),
    };

    let parsed: Option<Value> = serde_json::from_str(&text).ok();
    let raw = match &parsed {
        Some(v) => serde_json::to_string_pretty(v).unwrap_or_else(|_| text.clone()),
        None => text.clone(),
    };
    let prompts = parsed.as_ref().map(walk_graph).unwrap_or_default();

    Meta {
        raw: Some(raw),
        prompts,
    }
}

/// Read the most relevant text chunk, preferring `prompt` (the API graph),
/// then `workflow`, then any chunk. Returns the chunk's text payload.
fn read_prompt_text(path: &Path) -> Option<String> {
    let file = File::open(path).ok()?;
    let decoder = png::Decoder::new(BufReader::new(file));
    // read_info parses chunks up to IDAT; the generator's metadata lives there.
    let reader = decoder.read_info().ok()?;
    let info = reader.info();

    let mut found: Vec<(String, String)> = Vec::new();
    for c in &info.uncompressed_latin1_text {
        found.push((c.keyword.clone(), c.text.clone()));
    }
    for c in &info.compressed_latin1_text {
        if let Ok(t) = c.get_text() {
            found.push((c.keyword.clone(), t));
        }
    }
    for c in &info.utf8_text {
        if let Ok(t) = c.get_text() {
            found.push((c.keyword.clone(), t));
        }
    }

    for want in ["prompt", "workflow"] {
        if let Some((_, t)) = found.iter().find(|(k, _)| k == want) {
            return Some(t.clone());
        }
    }
    found.into_iter().next().map(|(_, t)| t)
}

/// Walk an API-format graph and collect positive/negative prompts per sampler.
/// Returns an empty vec on any structural mismatch (the raw JSON still shown).
fn walk_graph(v: &Value) -> Vec<SamplerPrompt> {
    let graph = match v.as_object() {
        Some(o) => o,
        None => return Vec::new(),
    };

    // A sampler is any node whose inputs carry both `positive` and `negative`.
    // Matching on inputs rather than class_type tolerates KSampler variants and
    // custom samplers (design.md §5, decision A-2 applies to the encoders).
    let mut sampler_ids: Vec<String> = graph
        .iter()
        .filter(|(_, node)| is_sampler(node))
        .map(|(id, _)| id.clone())
        .collect();
    sampler_ids.sort_by(|a, b| node_id_cmp(a, b));

    let mut out = Vec::new();
    for id in sampler_ids {
        let inputs = match graph
            .get(&id)
            .and_then(|n| n.get("inputs"))
            .and_then(Value::as_object)
        {
            Some(i) => i,
            None => continue,
        };
        let positive = resolve_conditioning(graph, inputs, "positive");
        let negative = resolve_conditioning(graph, inputs, "negative");
        // Skip samplers we could not attribute any text to: they would be noise.
        if positive.is_some() || negative.is_some() {
            out.push(SamplerPrompt {
                sampler_node: id,
                positive,
                negative,
            });
        }
    }
    out
}

fn is_sampler(node: &Value) -> bool {
    match node.get("inputs").and_then(Value::as_object) {
        Some(inputs) => inputs.contains_key("positive") && inputs.contains_key("negative"),
        None => false,
    }
}

/// Resolve a sampler's `positive`/`negative` input to a prompt string.
///
/// Per decision A-3 we only follow the single conditioning link and read a
/// literal `text` (or SDXL `text_g`/`text_l`) string off a CLIPTextEncode-like
/// node. If the conditioning is anything else (a Reroute/Primitive link, a
/// non-encoder node, a missing reference), we give up and return `None`.
fn resolve_conditioning(
    graph: &Map<String, Value>,
    inputs: &Map<String, Value>,
    key: &str,
) -> Option<String> {
    let target_id = link_target(inputs.get(key)?)?;
    let node = graph.get(target_id)?;
    node_text(node)
}

/// A conditioning input is `[node_id, output_index]`; return the node id.
fn link_target(value: &Value) -> Option<&str> {
    value.as_array()?.first()?.as_str()
}

/// Pull the prompt text from a CLIPTextEncode-like node. Matches `class_type`
/// by substring (A-2) to cover variants (e.g. `CLIPTextEncodeSDXL`). Returns
/// `None` if the node is not an encoder or its text input is not a literal
/// string (e.g. it is itself a link).
fn node_text(node: &Value) -> Option<String> {
    let class = node.get("class_type")?.as_str()?;
    if !class.contains("CLIPTextEncode") {
        return None;
    }
    let inputs = node.get("inputs")?.as_object()?;

    if let Some(t) = inputs.get("text").and_then(Value::as_str) {
        return Some(t.to_string());
    }

    // SDXL-style dual encoders: prefer text_g, append text_l if it differs.
    let g = inputs.get("text_g").and_then(Value::as_str);
    let l = inputs.get("text_l").and_then(Value::as_str);
    match (g, l) {
        (Some(g), Some(l)) if g != l => Some(format!("{g}\n{l}")),
        (Some(g), _) => Some(g.to_string()),
        (None, Some(l)) => Some(l.to_string()),
        (None, None) => None,
    }
}

/// Order node ids numerically when both are integers (the common generator convention),
/// otherwise lexicographically.
fn node_id_cmp(a: &str, b: &str) -> Ordering {
    match (a.parse::<u64>(), b.parse::<u64>()) {
        (Ok(x), Ok(y)) => x.cmp(&y),
        _ => a.cmp(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufWriter;

    /// Write a 1x1 grayscale PNG, optionally embedding one tEXt chunk.
    fn write_png(path: &Path, text: Option<(&str, &str)>) {
        let file = File::create(path).unwrap();
        let mut encoder = png::Encoder::new(BufWriter::new(file), 1, 1);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Eight);
        if let Some((k, t)) = text {
            // Added on the Encoder so the tEXt chunk is emitted before IDAT.
            encoder
                .add_text_chunk(k.to_string(), t.to_string())
                .unwrap();
        }
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[0u8]).unwrap();
    }

    fn tmp_png(name: &str, text: Option<(&str, &str)>) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        write_png(&path, text);
        (dir, path)
    }

    #[test]
    fn extracts_positive_and_negative() {
        let graph = r#"{
            "3": {"class_type":"KSampler","inputs":{"positive":["6",0],"negative":["7",0],"seed":1}},
            "6": {"class_type":"CLIPTextEncode","inputs":{"text":"a beautiful sunset"}},
            "7": {"class_type":"CLIPTextEncode","inputs":{"text":"ugly, blurry"}}
        }"#;
        let (_d, p) = tmp_png("a.png", Some(("prompt", graph)));
        let meta = extract_meta(&p);
        assert!(meta.raw.is_some());
        assert_eq!(
            meta.prompts,
            vec![SamplerPrompt {
                sampler_node: "3".to_string(),
                positive: Some("a beautiful sunset".to_string()),
                negative: Some("ugly, blurry".to_string()),
            }]
        );
    }

    #[test]
    fn sdxl_dual_encoder_combines_text_g_and_l() {
        let graph = r#"{
            "3": {"class_type":"KSamplerAdvanced","inputs":{"positive":["6",0],"negative":["7",0]}},
            "6": {"class_type":"CLIPTextEncodeSDXL","inputs":{"text_g":"big","text_l":"small"}},
            "7": {"class_type":"CLIPTextEncodeSDXL","inputs":{"text_g":"neg","text_l":"neg"}}
        }"#;
        let (_d, p) = tmp_png("sdxl.png", Some(("prompt", graph)));
        let meta = extract_meta(&p);
        assert_eq!(meta.prompts.len(), 1);
        assert_eq!(meta.prompts[0].positive.as_deref(), Some("big\nsmall"));
        assert_eq!(meta.prompts[0].negative.as_deref(), Some("neg"));
    }

    #[test]
    fn linked_text_input_falls_back_to_none() {
        // positive's text is itself a link (Reroute/Primitive): per A-3 we do
        // not chase it, so positive is None while negative still resolves.
        let graph = r#"{
            "3": {"class_type":"KSampler","inputs":{"positive":["6",0],"negative":["7",0]}},
            "6": {"class_type":"CLIPTextEncode","inputs":{"text":["10",0]}},
            "7": {"class_type":"CLIPTextEncode","inputs":{"text":"neg"}},
            "10": {"class_type":"PrimitiveNode","inputs":{}}
        }"#;
        let (_d, p) = tmp_png("link.png", Some(("prompt", graph)));
        let meta = extract_meta(&p);
        assert_eq!(meta.prompts.len(), 1);
        assert_eq!(meta.prompts[0].positive, None);
        assert_eq!(meta.prompts[0].negative.as_deref(), Some("neg"));
    }

    #[test]
    fn multiple_samplers_enumerated_in_node_id_order() {
        let graph = r#"{
            "10": {"class_type":"KSampler","inputs":{"positive":["11",0],"negative":["12",0]}},
            "11": {"class_type":"CLIPTextEncode","inputs":{"text":"second pos"}},
            "12": {"class_type":"CLIPTextEncode","inputs":{"text":"second neg"}},
            "3": {"class_type":"KSampler","inputs":{"positive":["6",0],"negative":["7",0]}},
            "6": {"class_type":"CLIPTextEncode","inputs":{"text":"first pos"}},
            "7": {"class_type":"CLIPTextEncode","inputs":{"text":"first neg"}}
        }"#;
        let (_d, p) = tmp_png("multi.png", Some(("prompt", graph)));
        let meta = extract_meta(&p);
        // Numeric order: node 3 before node 10.
        let ids: Vec<&str> = meta
            .prompts
            .iter()
            .map(|s| s.sampler_node.as_str())
            .collect();
        assert_eq!(ids, vec!["3", "10"]);
        assert_eq!(meta.prompts[0].positive.as_deref(), Some("first pos"));
        assert_eq!(meta.prompts[1].positive.as_deref(), Some("second pos"));
    }

    #[test]
    fn no_sampler_yields_raw_only() {
        let graph = r#"{"1":{"class_type":"LoadImage","inputs":{"image":"x.png"}}}"#;
        let (_d, p) = tmp_png("nosampler.png", Some(("prompt", graph)));
        let meta = extract_meta(&p);
        assert!(meta.raw.is_some());
        assert!(meta.prompts.is_empty());
    }

    #[test]
    fn broken_json_yields_raw_text_only() {
        let (_d, p) = tmp_png("broken.png", Some(("prompt", "{ this is not json")));
        let meta = extract_meta(&p);
        assert_eq!(meta.raw.as_deref(), Some("{ this is not json"));
        assert!(meta.prompts.is_empty());
    }

    #[test]
    fn png_without_text_chunk_yields_empty_meta() {
        let (_d, p) = tmp_png("plain.png", None);
        assert_eq!(extract_meta(&p), Meta::empty());
    }

    #[test]
    fn non_png_file_yields_empty_meta() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notapng.png");
        std::fs::write(&path, b"this is not a png").unwrap();
        assert_eq!(extract_meta(&path), Meta::empty());
    }

    #[test]
    fn workflow_used_when_prompt_absent() {
        // Only a `workflow` chunk; it is shown raw. Its UI-graph schema does not
        // match the sampler heuristic, so no prompts are extracted.
        let wf = r#"{"nodes":[{"id":3,"type":"KSampler"}]}"#;
        let (_d, p) = tmp_png("wf.png", Some(("workflow", wf)));
        let meta = extract_meta(&p);
        assert!(meta.raw.is_some());
        assert!(meta.prompts.is_empty());
    }
}
