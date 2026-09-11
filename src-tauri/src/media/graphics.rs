//! Safe, local SVG cards/overlays for agent-assisted editing.
//!
//! SVG is treated as data, not executable code: external references, scripts,
//! event attributes, foreignObject, and CSS imports are rejected before usvg
//! parses the document. Raster output is stored through the managed artifact
//! store, never at a caller-selected filesystem path.

use crate::editor::dispatcher::CallerContext;
use crate::error::{AppError, ErrorCode};
use crate::media::artifacts::{ArtifactKind, ArtifactRecord, ArtifactStore};
use crate::media::evidence::run_evidence_job;
use crate::project::model::{
    AssetKind, AssetManifest, NormalizedAsset, NormalizedVideo, OriginalMediaMetadata,
    OriginalStreamKind, OriginalStreamMetadata,
};
use crate::state::AppState;
use quick_xml::events::Event;
use quick_xml::Reader;
use resvg::{tiny_skia, usvg};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Arc;
use ts_rs::TS;
use uuid::Uuid;

pub const MAX_SVG_BYTES: usize = 256 * 1024;
pub const MAX_GRAPHIC_DIMENSION: u32 = 4_096;
pub const MAX_GRAPHIC_PIXELS: u64 = 16_000_000;
const RENDERER_VERSION: &str = "resvg-local-1";
pub(crate) const INTER_FONT_FAMILY: &str = "Inter Variable";
pub(crate) const INTER_FONT_RELEASE: &str = "4.1";
const INTER_REGULAR_FILE: &str = "InterVariable.ttf";
const INTER_REGULAR_SHA256: &str =
    "4989b125924991b90d05b2d16e0e388c48f7d5bb8b30539bbf9c755278d0ccaf";
const INTER_ITALIC_FILE: &str = "InterVariable-Italic.ttf";
const INTER_ITALIC_SHA256: &str =
    "d6f1f6a172d9e588438db9f986fd5cfad7b30f644374080a8a9d4d91e344586f";

/// The one font database shared by graphic overlays and canonical text
/// rasters.  It is populated only from the app's staged Inter resources; no
/// system font database or caller-controlled path participates in rendering.
#[derive(Debug)]
pub(crate) struct BundledFontCatalog {
    pub(crate) database: Arc<usvg::fontdb::Database>,
    pub(crate) identity: String,
    regular_face: usvg::fontdb::ID,
}

impl BundledFontCatalog {
    pub(crate) fn missing_glyphs(&self, text: &str) -> Vec<char> {
        let mut missing = Vec::new();
        let _ = self
            .database
            .with_face_data(self.regular_face, |data, index| {
                let Ok(face) = ttf_parser::Face::parse(data, index) else {
                    for character in text
                        .chars()
                        .filter(|character| visible_character(*character))
                    {
                        if !missing.contains(&character) {
                            missing.push(character);
                        }
                    }
                    return;
                };
                for character in text
                    .chars()
                    .filter(|character| visible_character(*character))
                {
                    if face.glyph_index(character).is_none() && !missing.contains(&character) {
                        missing.push(character);
                    }
                }
            });
        missing
    }
}

fn visible_character(character: char) -> bool {
    !character.is_whitespace()
        && !character.is_control()
        && !matches!(
            character,
            '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{2060}' | '\u{FE0E}' | '\u{FE0F}'
        )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(rename_all = "camelCase")]
pub struct CreateGraphicRequest {
    pub name: String,
    pub svg: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(rename_all = "camelCase")]
pub struct GraphicReply {
    pub asset_id: String,
    #[ts(type = "SafeInteger")]
    pub revision: u64,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub artifact: ArtifactRecord,
}

#[derive(Debug, Clone)]
pub struct GraphicsRuntime;

impl GraphicsRuntime {
    pub fn new() -> Self {
        Self
    }
    pub async fn handle(
        &self,
        request: CreateGraphicRequest,
        caller: &CallerContext,
        state: &AppState,
    ) -> Result<GraphicReply, AppError> {
        state.validate_generation(caller.generation)?;
        validate_graphic_request(&request)?;
        let store = state.current_store()?;
        let snapshot = store.snapshot()?;
        let artifacts = ArtifactStore::for_project(store.root(), store.workspace_id())?;
        let cache_key = graphic_cache_key(&request);
        let payload_hash = graphic_payload_hash(&request);
        let transaction_id = Uuid::new_v4().to_string();
        let generation = caller.generation;
        let run_id = caller.run_id().map(str::to_owned);
        let project_id = snapshot.project_id.clone();
        let expected_revision = snapshot.document.revision;
        let name = request.name.clone();
        let svg = request.svg.clone();
        let width = request.width;
        let height = request.height;
        let resource_dir = state.paths().resource_dir.clone();
        let state_for_job = state.clone();
        run_evidence_job(
            state,
            caller,
            "create_graphic",
            Some(project_id),
            move |context| {
                context.check_cancelled()?;
                let png = rasterize_svg(&svg, width, height, &resource_dir)?;
                if png.len() > 4 * 1024 * 1024 {
                    return Err(AppError::new(
                        ErrorCode::MediaUnsupported,
                        "The generated graphic exceeds the 4 MiB encoded image limit",
                    ));
                }
                context.progress(0.55)?;
                let artifact =
                    artifacts.put_bytes(&cache_key, "png", ArtifactKind::StillImage, &png)?;
                context.check_cancelled()?;

                let asset_id = deterministic_asset_id(&artifact.artifact_id);
                let asset = generated_asset(
                    &asset_id,
                    &name,
                    &png,
                    &artifact,
                    width,
                    height,
                    snapshot.document.profile.fps_num,
                    snapshot.document.profile.fps_den,
                );
                let asset_for_commit = asset.clone();
                let edit = state_for_job.commit_at_with_run(
                    generation,
                    run_id.as_deref(),
                    transaction_id,
                    expected_revision,
                    "Create graphic".to_owned(),
                    payload_hash,
                    move |document| {
                        if let Some(existing) = document
                            .assets
                            .iter()
                            .find(|candidate| candidate.id == asset_for_commit.id)
                        {
                            let existing_artifact = existing
                                .normalization
                                .as_ref()
                                .and_then(|normalization| normalization.video.as_ref())
                                .map(|video| video.master_artifact_id.as_str());
                            if existing_artifact
                                == Some(
                                    asset_for_commit
                                        .normalization
                                        .as_ref()
                                        .and_then(|normalization| normalization.video.as_ref())
                                        .expect("generated asset has a video normalization")
                                        .master_artifact_id
                                        .as_str(),
                                )
                            {
                                return Ok(());
                            }
                            return Err(AppError::new(
                                ErrorCode::IdempotencyConflict,
                                "The generated graphic asset ID is already present",
                            ));
                        }
                        document.assets.push(asset_for_commit);
                        Ok(())
                    },
                )?;
                context.progress(0.95)?;
                Ok(GraphicReply {
                    asset_id: asset.id,
                    revision: edit.revision,
                    name,
                    width,
                    height,
                    artifact,
                })
            },
        )
        .await
    }
}

impl Default for GraphicsRuntime {
    fn default() -> Self {
        Self::new()
    }
}

fn generated_asset(
    asset_id: &str,
    name: &str,
    png: &[u8],
    artifact: &ArtifactRecord,
    width: u32,
    height: u32,
    fps_num: u32,
    fps_den: u32,
) -> AssetManifest {
    let source_hash = digest_hex(png);
    AssetManifest {
        id: asset_id.to_owned(),
        kind: AssetKind::StillImage,
        content_hash: source_hash,
        original: OriginalMediaMetadata {
            file_name: format!("{name}.png"),
            location: None,
            byte_size: Some(png.len() as u64),
            modified_time_ms: None,
            streams: vec![OriginalStreamMetadata {
                kind: OriginalStreamKind::Video,
                codec: "png".to_owned(),
                duration_ms: Some(1),
                start_time_ms: Some(0),
                width: Some(width),
                height: Some(height),
                ..OriginalStreamMetadata::default()
            }],
        },
        normalization: Some(NormalizedAsset {
            renderer_version: RENDERER_VERSION.to_owned(),
            epoch_ms: 0,
            video: Some(NormalizedVideo {
                master_artifact_id: artifact.artifact_id.clone(),
                proxy_artifact_id: None,
                frame_count: 1,
                width,
                height,
                fps_num,
                fps_den,
                active_start_frame: 0,
                active_end_frame: 1,
                source_start_ms: 0,
                source_end_ms: 1,
                proxy_frame_count: None,
            }),
            audio: None,
        }),
    }
}

fn graphic_payload_hash(request: &CreateGraphicRequest) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"cutterhoochee-create-graphic-v1\0");
    hasher.update(request.name.as_bytes());
    hasher.update([0]);
    hasher.update(request.svg.as_bytes());
    hasher.update(request.width.to_le_bytes());
    hasher.update(request.height.to_le_bytes());
    digest_hex(&hasher.finalize())
}

fn digest_hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}
fn deterministic_asset_id(artifact_id: &str) -> String {
    let digest = Sha256::digest(artifact_id.as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    // Use a UUID-shaped deterministic ID so repeated content-addressed
    // requests can safely produce a no-change transaction.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes).to_string()
}

pub fn validate_graphic_request(request: &CreateGraphicRequest) -> Result<(), AppError> {
    if request.name.trim().is_empty()
        || request.name.len() > 256
        || request.name.chars().any(char::is_control)
        || request.name.contains('/')
        || request.name.contains('\\')
    {
        return Err(AppError::invalid_argument("Graphic name is invalid"));
    }
    if request.svg.as_bytes().len() > MAX_SVG_BYTES {
        return Err(AppError::invalid_argument("SVG exceeds the 256 KiB limit"));
    }
    validate_dimensions(request.width, request.height)?;
    validate_svg_markup(&request.svg)?;
    Ok(())
}

pub fn validate_dimensions(width: u32, height: u32) -> Result<(), AppError> {
    if width == 0 || height == 0 || width > MAX_GRAPHIC_DIMENSION || height > MAX_GRAPHIC_DIMENSION
    {
        return Err(AppError::invalid_argument(
            "Graphic dimensions must be positive and at most 4096 pixels",
        ));
    }
    if u64::from(width)
        .checked_mul(u64::from(height))
        .is_none_or(|pixels| pixels > MAX_GRAPHIC_PIXELS)
    {
        return Err(AppError::invalid_argument(
            "Graphic dimensions exceed the 16 megapixel limit",
        ));
    }
    Ok(())
}

/// Parse XML before usvg and reject every capability that could fetch or run
/// content. Unknown markup remains literal data only when it is text; unknown
/// elements are rejected if they are executable/resource-bearing.
pub fn validate_svg_markup(svg: &str) -> Result<(), AppError> {
    if svg.as_bytes().len() > MAX_SVG_BYTES {
        return Err(AppError::invalid_argument("SVG exceeds the 256 KiB limit"));
    }
    let lower = svg.to_ascii_lowercase();
    if lower.contains("<!doctype") || lower.contains("<!entity") || lower.contains("<script") {
        return Err(AppError::invalid_argument(
            "SVG scripts, entities, and doctypes are not allowed",
        ));
    }
    let mut reader = Reader::from_str(svg);
    reader.config_mut().trim_text(false);
    let mut first_element = false;
    let mut depth = 0usize;
    loop {
        match reader.read_event() {
            Ok(Event::Start(event)) => {
                if first_element && depth == 0 {
                    return Err(AppError::invalid_argument("SVG must have one root element"));
                }
                let name = event.local_name().as_ref().to_ascii_lowercase();
                if !first_element {
                    if name != "svg" {
                        return Err(AppError::invalid_argument(
                            "SVG must have an svg root element",
                        ));
                    }
                    first_element = true;
                }
                reject_element_name(&name)?;
                reject_attributes(event.attributes())?;
                depth = depth
                    .checked_add(1)
                    .ok_or_else(|| AppError::schema("SVG nesting is too deep"))?;
            }
            Ok(Event::Empty(event)) => {
                if first_element && depth == 0 {
                    return Err(AppError::invalid_argument("SVG must have one root element"));
                }
                let name = event.local_name().as_ref().to_ascii_lowercase();
                if !first_element {
                    if name != "svg" {
                        return Err(AppError::invalid_argument(
                            "SVG must have an svg root element",
                        ));
                    }
                    first_element = true;
                }
                reject_element_name(&name)?;
                reject_attributes(event.attributes())?;
            }
            Ok(Event::End(_)) => {
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| AppError::schema("SVG closing element is unmatched"))?;
            }
            Ok(Event::DocType(_)) => {
                return Err(AppError::invalid_argument(
                    "SVG scripts, entities, and doctypes are not allowed",
                ));
            }
            Ok(Event::PI(_)) => {
                return Err(AppError::invalid_argument(
                    "SVG processing instructions are not allowed",
                ));
            }
            Ok(Event::Decl(_)) | Ok(Event::Comment(_)) => {}
            Ok(Event::Text(event)) => {
                let text = event.xml_content(quick_xml::XmlVersion::Implicit1_0);
                let text = quick_xml::escape::unescape(text.as_ref())
                    .map_err(|_| AppError::invalid_argument("SVG text is malformed"))?;
                reject_external_text(text.as_ref())?;
            }
            Ok(Event::CData(event)) => {
                let text = event.xml_content(quick_xml::XmlVersion::Implicit1_0);
                reject_external_text(text.as_ref())?
            }
            Ok(Event::Eof) => break,
            Ok(Event::GeneralRef(_)) => {
                return Err(AppError::invalid_argument(
                    "SVG entity references are not allowed",
                ));
            }
            Err(_) => return Err(AppError::invalid_argument("SVG markup is malformed")),
        }
    }
    if !first_element || depth != 0 {
        return Err(AppError::invalid_argument("SVG markup is incomplete"));
    }
    Ok(())
}

fn reject_element_name(name: &str) -> Result<(), AppError> {
    if matches!(
        name,
        "script"
            | "foreignobject"
            | "image"
            | "use"
            | "iframe"
            | "audio"
            | "video"
            | "style"
            | "link"
            | "feimage"
    ) {
        return Err(AppError::invalid_argument(
            "SVG scripts, external resources, and embedded content are not allowed",
        ));
    }
    Ok(())
}

fn reject_attributes<'a, I, E>(attributes: I) -> Result<(), AppError>
where
    I: Iterator<Item = Result<quick_xml::events::attributes::Attribute<'a>, E>>,
    E: std::fmt::Debug,
{
    for attribute in attributes {
        let attribute =
            attribute.map_err(|_| AppError::invalid_argument("SVG attributes are malformed"))?;
        let name = attribute.key.as_ref().to_ascii_lowercase();
        let local_name = attribute.key.local_name().as_ref().to_ascii_lowercase();
        let value = attribute
            .normalized_value(quick_xml::XmlVersion::Implicit1_0)
            .map_err(|_| AppError::invalid_argument("SVG attributes are malformed"))?;
        if name == "xmlns" && value == "http://www.w3.org/2000/svg" {
            continue;
        }
        if local_name.starts_with("on")
            || matches!(local_name.as_str(), "href" | "src" | "base")
            || matches!(name.as_str(), "xlink:href" | "xmlns:xlink")
        {
            return Err(AppError::invalid_argument(
                "SVG event handlers and external references are not allowed",
            ));
        }
        reject_external_text(value.as_ref())?;
    }
    Ok(())
}

fn reject_external_text(value: &str) -> Result<(), AppError> {
    let lower = value.to_ascii_lowercase();
    if lower.contains("url(")
        || lower.contains("@import")
        || lower.contains("http:")
        || lower.contains("https:")
        || lower.contains("file:")
        || lower.contains("data:")
        || lower.contains("@font-face")
    {
        return Err(AppError::invalid_argument(
            "SVG external URLs, images, and fonts are not allowed",
        ));
    }
    Ok(())
}

pub fn rasterize_svg(
    svg: &str,
    width: u32,
    height: u32,
    resource_dir: &Path,
) -> Result<Vec<u8>, AppError> {
    let request = CreateGraphicRequest {
        name: "graphic".to_owned(),
        svg: svg.to_owned(),
        width,
        height,
    };
    validate_graphic_request(&request)?;
    let catalog = load_bundled_font_catalog(resource_dir)?;
    rasterize_svg_with_catalog(svg, width, height, &catalog)
}

/// Rasterize validated SVG data with the already-loaded, resource-backed font
/// catalog.  The resolver has no resources directory, so relative and
/// external files cannot be reached by the parser.
pub(crate) fn rasterize_svg_with_catalog(
    svg: &str,
    width: u32,
    height: u32,
    catalog: &BundledFontCatalog,
) -> Result<Vec<u8>, AppError> {
    let pixmap = render_svg_with_catalog(svg, width, height, catalog)?;
    pixmap
        .encode_png()
        .map_err(|_| AppError::io("The generated graphic PNG could not be encoded"))
}

/// Rasterize SVG data and remove transparent edge pixels from the result.
///
/// Text overlays use this so their compositor rectangle is based on the
/// painted raster bounds rather than the synthetic allocation used to fit
/// their source SVG.  The crop is deliberately opt-in: boxed text and
/// generated graphics retain their requested background/canvas dimensions.
pub(crate) fn rasterize_svg_with_catalog_trimmed(
    svg: &str,
    width: u32,
    height: u32,
    catalog: &BundledFontCatalog,
) -> Result<(Vec<u8>, u32, u32), AppError> {
    let pixmap = render_svg_with_catalog(svg, width, height, catalog)?;
    let Some((x, y, crop_width, crop_height)) = alpha_bounds(&pixmap) else {
        let png = pixmap
            .encode_png()
            .map_err(|_| AppError::io("The generated graphic PNG could not be encoded"))?;
        return Ok((png, pixmap.width(), pixmap.height()));
    };

    let mut cropped = tiny_skia::Pixmap::new(crop_width, crop_height)
        .ok_or_else(|| AppError::invalid_argument("Graphic dimensions cannot be allocated"))?;
    let source_stride = pixmap.width() as usize * 4;
    let destination_stride = crop_width as usize * 4;
    let source_x = x as usize * 4;
    let source_y = y as usize * source_stride;
    for row in 0..crop_height as usize {
        let source_start = source_y + row * source_stride + source_x;
        let destination_start = row * destination_stride;
        cropped.data_mut()[destination_start..destination_start + destination_stride]
            .copy_from_slice(&pixmap.data()[source_start..source_start + destination_stride]);
    }
    let png = cropped
        .encode_png()
        .map_err(|_| AppError::io("The generated graphic PNG could not be encoded"))?;
    Ok((png, crop_width, crop_height))
}

fn render_svg_with_catalog(
    svg: &str,
    width: u32,
    height: u32,
    catalog: &BundledFontCatalog,
) -> Result<tiny_skia::Pixmap, AppError> {
    validate_svg_markup(svg)?;
    let mut options = usvg::Options::default();
    options.resources_dir = None;
    options.font_family = INTER_FONT_FAMILY.to_owned();
    options.fontdb = Arc::clone(&catalog.database);
    let tree = usvg::Tree::from_str(svg, &options)
        .map_err(|_| AppError::invalid_argument("SVG could not be rasterized"))?;
    let mut pixmap = tiny_skia::Pixmap::new(width, height)
        .ok_or_else(|| AppError::invalid_argument("Graphic dimensions cannot be allocated"))?;
    resvg::render(&tree, tiny_skia::Transform::default(), &mut pixmap.as_mut());
    Ok(pixmap)
}

fn alpha_bounds(pixmap: &tiny_skia::Pixmap) -> Option<(u32, u32, u32, u32)> {
    let width = pixmap.width() as usize;
    let mut bounds: Option<(u32, u32, u32, u32)> = None;
    for (index, pixel) in pixmap.data().chunks_exact(4).enumerate() {
        if pixel[3] == 0 {
            continue;
        }
        let x = (index % width) as u32;
        let y = (index / width) as u32;
        bounds = Some(match bounds {
            Some((left, top, right, bottom)) => {
                (left.min(x), top.min(y), right.max(x), bottom.max(y))
            }
            None => (x, y, x, y),
        });
    }
    bounds.map(|(left, top, right, bottom)| (left, top, right - left + 1, bottom - top + 1))
}

/// Load only the pinned Inter resources from the app's staged resource tree.
/// The regular face is required for all canonical plans; italic is included
/// when present so graphic and text rendering share one stable catalog.
pub(crate) fn load_bundled_font_catalog(
    resource_dir: &Path,
) -> Result<Arc<BundledFontCatalog>, AppError> {
    if !resource_dir.is_absolute() {
        return Err(AppError::invalid_argument(
            "The bundled resource directory must be absolute",
        ));
    }
    let regular_path = locate_bundled_font(resource_dir, INTER_REGULAR_FILE).ok_or_else(|| {
        AppError::new(
            ErrorCode::MediaUnsupported,
            "The bundled Inter regular font is unavailable",
        )
    })?;
    let mut database = usvg::fontdb::Database::new();
    let regular_bytes = std::fs::read(&regular_path).map_err(|_| {
        AppError::new(
            ErrorCode::MediaUnsupported,
            "The bundled Inter font could not be read",
        )
    })?;
    verify_font_bytes(INTER_REGULAR_FILE, INTER_REGULAR_SHA256, &regular_bytes)?;
    database.load_font_data(regular_bytes);

    let mut identity =
        format!("Inter@{INTER_FONT_RELEASE};{INTER_REGULAR_FILE}:{INTER_REGULAR_SHA256}");
    if let Some(italic_path) = locate_bundled_font(resource_dir, INTER_ITALIC_FILE) {
        let italic_bytes = std::fs::read(&italic_path).map_err(|_| {
            AppError::new(
                ErrorCode::MediaUnsupported,
                "The bundled Inter italic font could not be read",
            )
        })?;
        verify_font_bytes(INTER_ITALIC_FILE, INTER_ITALIC_SHA256, &italic_bytes)?;
        database.load_font_data(italic_bytes);
        identity.push(';');
        identity.push_str(INTER_ITALIC_FILE);
        identity.push(':');
        identity.push_str(INTER_ITALIC_SHA256);
    }
    let families = [usvg::fontdb::Family::Name(INTER_FONT_FAMILY)];
    let regular_face = database
        .query(&usvg::fontdb::Query {
            families: &families,
            ..usvg::fontdb::Query::default()
        })
        .ok_or_else(|| {
            AppError::new(
                ErrorCode::MediaUnsupported,
                "The staged Inter font has no selectable regular face",
            )
        })?;
    Ok(Arc::new(BundledFontCatalog {
        database: Arc::new(database),
        identity,
        regular_face,
    }))
}

fn locate_bundled_font(resource_dir: &Path, file_name: &str) -> Option<std::path::PathBuf> {
    [
        resource_dir.join("fonts").join(file_name),
        resource_dir.join("resources/fonts").join(file_name),
    ]
    .into_iter()
    .find(|path| {
        std::fs::symlink_metadata(path)
            .map(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
            .unwrap_or(false)
    })
}

fn verify_font_bytes(file_name: &str, expected: &str, bytes: &[u8]) -> Result<(), AppError> {
    let actual = hex_digest(Sha256::digest(bytes).as_slice());
    if actual != expected {
        return Err(AppError::new(
            ErrorCode::MediaUnsupported,
            format!("The staged {file_name} checksum does not match the pinned Inter resource"),
        ));
    }
    Ok(())
}

fn graphic_cache_key(request: &CreateGraphicRequest) -> String {
    let mut hasher = Sha256::new();
    hasher.update(RENDERER_VERSION.as_bytes());
    hasher.update(request.width.to_le_bytes());
    hasher.update(request.height.to_le_bytes());
    hasher.update(request.svg.as_bytes());
    format!("graphic:{}", hex_digest(hasher.finalize().as_slice()))
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(char::from(b"0123456789abcdef"[(byte >> 4) as usize]));
        result.push(char::from(b"0123456789abcdef"[(byte & 0x0f) as usize]));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(svg: &str) -> CreateGraphicRequest {
        CreateGraphicRequest {
            name: "card".to_owned(),
            svg: svg.to_owned(),
            width: 640,
            height: 360,
        }
    }

    #[test]
    fn safe_shapes_pass_markup_validation() {
        validate_graphic_request(&request(
            r##"<svg xmlns="http://www.w3.org/2000/svg"><rect width="100%" height="100%" fill="#123456"/></svg>"##,
        ))
        .expect("safe SVG");
    }

    #[test]
    fn executable_and_external_svg_content_is_rejected() {
        for svg in [
            r#"<svg><script>alert(1)</script></svg>"#,
            r#"<svg><foreignObject><div>x</div></foreignObject></svg>"#,
            r#"<svg><image href="https://example.invalid/x.png"/></svg>"#,
            r#"<svg><rect onclick="alert(1)"/></svg>"#,
            r#"<!DOCTYPE svg [<!ENTITY x SYSTEM "file:///etc/passwd">]><svg>&x;</svg>"#,
        ] {
            assert!(
                validate_svg_markup(svg).is_err(),
                "unsafe SVG accepted: {svg}"
            );
        }
    }

    #[test]
    fn alpha_bounds_uses_nontransparent_pixel_extent() {
        let mut pixmap = tiny_skia::Pixmap::new(6, 5).expect("test pixmap");
        pixmap.data_mut()[(1 * 6 + 2) * 4 + 3] = 255;
        pixmap.data_mut()[(3 * 6 + 4) * 4 + 3] = 128;
        assert_eq!(alpha_bounds(&pixmap), Some((2, 1, 3, 3)));
    }

    #[test]
    fn dimensions_are_bounded_before_allocation() {
        assert!(validate_dimensions(4_096, 4_096).is_err());
        assert!(validate_dimensions(4_000, 4_000).is_ok());
    }
}
