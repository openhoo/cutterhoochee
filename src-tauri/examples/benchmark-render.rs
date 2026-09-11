//! Real-media release benchmark. Supply a generated project and its captured immutable
//! preview plan. Decoder startup is included in frame timings; fixture loading,
//! checksum calculation, and pooled-renderer construction are reported separately.
use cutterhoochee_lib::media::artifacts::ArtifactStore;
use cutterhoochee_lib::media::ffmpeg::FfmpegToolchain;
use cutterhoochee_lib::media::render::frame::{render_rgba_frame, CanonicalFrameRenderer};
use cutterhoochee_lib::media::render_plan::{compile_render_plan, RenderPlan};
use cutterhoochee_lib::project::model::ProjectEnvelope;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

fn summary(samples: &[f64]) -> Value {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    json!({"samples_ms": samples, "median_ms": sorted[sorted.len()/2],
        "p95_ms": sorted[((sorted.len() as f64 * 0.95).ceil() as usize).saturating_sub(1)],
        "total_ms": samples.iter().sum::<f64>()})
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut project = None;
    let mut plan_path = None;
    let mut resources = None;
    let mut output = None;
    let mut frames = 90u64;
    let mut samples = 7usize;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--project" => {
                project = Some(PathBuf::from(args.next().ok_or("missing project path")?))
            }
            "--plan" => plan_path = Some(PathBuf::from(args.next().ok_or("missing plan path")?)),
            "--resources" => {
                resources = Some(PathBuf::from(
                    args.next().ok_or("missing resource directory")?,
                ))
            }
            "--output" => output = Some(PathBuf::from(args.next().ok_or("missing output path")?)),
            "--frames" => frames = args.next().ok_or("missing frames")?.parse()?,
            "--samples" => samples = args.next().ok_or("missing samples")?.parse()?,
            "--help" => {
                println!("benchmark-render --project GENERATED.cutproj (--plan PLAN.json | --resources ABSOLUTE_RESOURCE_DIR) [--frames 90] [--samples 7] [--output results.json]\nUses CUTTERHOOCHEE_FFMPEG/CUTTERHOOCHEE_FFPROBE or the host tools. Run in release mode.");
                return Ok(());
            }
            _ => return Err(format!("unknown argument: {arg}").into()),
        }
    }
    if frames == 0 || frames > 3000 || samples == 0 || samples > 100 {
        return Err("frames must be 1..=3000 and samples 1..=100".into());
    }
    let project = project.ok_or("--project is required")?;
    let mut store = ArtifactStore::for_project(&project, "performance-benchmark")?;
    if let Some(resources) = resources {
        store = store.with_font_resource_dir(&resources)?;
    }
    let artifacts = Arc::new(store);
    let plan: RenderPlan = if let Some(path) = plan_path {
        serde_json::from_slice(&std::fs::read(path)?)?
    } else {
        let envelope: ProjectEnvelope =
            serde_json::from_slice(&std::fs::read(project.join("project.json"))?)?;
        compile_render_plan(&envelope.document, artifacts.as_ref())?
    };
    plan.validate()?;
    if plan.duration_frames == 0 {
        return Err("plan must contain frames".into());
    }
    let toolchain = FfmpegToolchain::default();
    toolchain.validate()?;
    let mut standalone = Vec::new();
    let mut standalone_hashes = Vec::new();
    for index in 0..samples {
        let frame = (index as u64 * 31 + 3) % plan.duration_frames;
        let start = Instant::now();
        let rgba = render_rgba_frame(&plan, frame, artifacts.as_ref(), &toolchain)?;
        standalone.push(start.elapsed().as_secs_f64() * 1000.0);
        standalone_hashes
            .push(json!({"frame": frame, "rgba_sha256": format!("{:x}", Sha256::digest(&rgba))}));
        std::hint::black_box(rgba);
    }
    let start = Instant::now();
    let renderer = CanonicalFrameRenderer::new(plan.clone(), artifacts, toolchain)?;
    let setup_ms = start.elapsed().as_secs_f64() * 1000.0;
    let mut sequential = Vec::new();
    let mut hashes = Vec::new();
    let measured_frames = frames.min(plan.duration_frames);
    for frame in 0..measured_frames {
        let start = Instant::now();
        let rgba = renderer.render(frame)?;
        sequential.push(start.elapsed().as_secs_f64() * 1000.0);
        let hash = format!("{:x}", Sha256::digest(&rgba));
        if let Some(reference) = standalone_hashes
            .iter()
            .find(|v| v["frame"].as_u64() == Some(frame))
        {
            if reference["rgba_sha256"].as_str() != Some(hash.as_str()) {
                return Err(format!("pooled and stateless pixels differ at frame {frame}").into());
            }
        }
        hashes.push(json!({"frame": frame, "rgba_sha256": hash}));
    }
    let result = json!({"benchmark": "canonical-render", "plan_hash": plan.plan_hash,
        "width": plan.width, "height": plan.height, "clip_count": plan.layers.iter().map(|l| l.segments.len()).sum::<usize>(),
        "stateless_frame": summary(&standalone),
        "pooled_setup_ms": setup_ms, "pooled_frames": summary(&sequential),
        "pooled_render_fps": measured_frames as f64 / (sequential.iter().sum::<f64>() / 1000.0),
        "standalone_hashes": standalone_hashes, "frame_hashes": hashes});
    let text = serde_json::to_string_pretty(&result)?;
    if let Some(path) = output {
        std::fs::write(path, &text)?;
    }
    println!("{text}");
    Ok(())
}
