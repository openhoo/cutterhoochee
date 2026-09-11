use cutterhoochee_lib::editor::operations::{apply_batch, ClipPatch, EditOp};
use cutterhoochee_lib::project::model::{
    AspectRatio, AssetKind, AssetManifest, FitMode, FrameRate, MediaClip, NormalizedAsset,
    NormalizedVideo, OriginalMediaMetadata, OriginalStreamKind, OriginalStreamMetadata,
    ProjectDocument, ProjectProfile, RgbaColor, TextItem, TextKind, TextStyle, Track, TrackKind,
};
use cutterhoochee_lib::project::store::{HistoryAction, ProjectStore};
use serde_json::{json, Value};
use std::error::Error;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_SIZES: &[usize] = &[100, 1_000, 5_000];
const DEFAULT_SAMPLES: usize = 7;
const CLIP_DURATION_FRAMES: u64 = 30;
const GAIN_DB: f64 = 6.0;
const RETAINED_HISTORY_ENTRIES: usize = 10;

const PROJECT_KIND: u32 = 0x1000_0000;
const ASSET_KIND: u32 = 0x1000_0001;
const VIDEO_TRACK_KIND: u32 = 0x1000_0002;
const AUDIO_TRACK_KIND: u32 = 0x1000_0003;
const TEXT_TRACK_KIND: u32 = 0x1000_0004;
const CLIP_KIND: u32 = 0x1000_0005;
const CAPTION_KIND: u32 = 0x1000_0006;
const SEED_TRANSACTION_KIND: u32 = 0x2000_0000;
const HISTORY_TRANSACTION_KIND: u32 = 0x2000_0001;
const DRY_RUN_TRANSACTION_KIND: u32 = 0x3000_0000;
const COMMIT_TRANSACTION_KIND: u32 = 0x3000_0001;

const WORKLOAD_VALIDATE: usize = 0;
const WORKLOAD_NOOP: usize = 1;
const WORKLOAD_GAIN: usize = 2;
const WORKLOAD_DIFF: usize = 3;
const WORKLOAD_CAPTIONS: usize = 4;
const WORKLOAD_SNAPSHOT: usize = 5;
const WORKLOAD_DRY_RUN: usize = 6;
const WORKLOAD_COMMIT: usize = 7;
const WORKLOAD_UNDO: usize = 8;
const WORKLOAD_REDO: usize = 9;
const WORKLOAD_OPEN: usize = 10;
const WORKLOAD_COUNT: usize = 11;

type BenchResult<T> = Result<T, Box<dyn Error>>;

struct Config {
    sizes: Vec<usize>,
    samples: usize,
    output: Option<PathBuf>,
}

struct Fixture {
    document: ProjectDocument,
    gained_document: ProjectDocument,
    gain_clip_id: String,
    gain_operation: EditOp,
    input_bytes: Vec<u8>,
    gained_bytes: Vec<u8>,
    input_checksum: String,
    gained_checksum: String,
    caption_count: usize,
    clips_with_captions: usize,
    max_captions_per_clip: usize,
}

struct PreparedStore {
    store: ProjectStore,
    baseline: ProjectDocument,
    root: PathBuf,
    app_data: PathBuf,
}

struct TimedWorkload {
    name: &'static str,
    size: usize,
    samples_ns: Vec<u64>,
    input_bytes: Vec<usize>,
    input_checksums: Vec<String>,
    output_bytes: Vec<usize>,
    output_checksums: Vec<String>,
    correctness: Vec<bool>,
}

impl TimedWorkload {
    fn new(name: &'static str, size: usize, sample_capacity: usize) -> Self {
        Self {
            name,
            size,
            samples_ns: Vec::with_capacity(sample_capacity),
            input_bytes: Vec::with_capacity(sample_capacity),
            input_checksums: Vec::with_capacity(sample_capacity),
            output_bytes: Vec::with_capacity(sample_capacity),
            output_checksums: Vec::with_capacity(sample_capacity),
            correctness: Vec::with_capacity(sample_capacity),
        }
    }

    fn record(&mut self, elapsed: Duration, input: &[u8], output: &[u8], correct: bool) {
        self.samples_ns.push(elapsed_ns(elapsed));
        self.input_bytes.push(input.len());
        self.input_checksums.push(checksum(input));
        self.output_bytes.push(output.len());
        self.output_checksums.push(checksum(output));
        self.correctness.push(correct);
    }

    fn into_json(self) -> Value {
        let mut ordered = self.samples_ns.clone();
        ordered.sort_unstable();
        let count = ordered.len();
        let median_ns = ordered[count / 2];
        let p95_rank = (count * 95).div_ceil(100).max(1);
        let p95_ns = ordered[p95_rank - 1];
        let min_ns = ordered[0];
        let max_ns = ordered[count - 1];
        let total_ns: u128 = ordered.iter().map(|value| u128::from(*value)).sum();
        let mean_ns = u64::try_from(total_ns / count as u128).unwrap_or(u64::MAX);
        let all_passed = self.correctness.iter().all(|passed| *passed);
        json!({
            "name": self.name,
            "size": self.size,
            "samples_ns": self.samples_ns,
            "median_ns": median_ns,
            "p95_ns": p95_ns,
            "summary": {
                "count": count,
                "min_ns": min_ns,
                "mean_ns": mean_ns,
                "median_ns": median_ns,
                "p95_ns": p95_ns,
                "max_ns": max_ns,
            },
            "input_bytes": self.input_bytes,
            "input_checksums": self.input_checksums,
            "output_bytes": self.output_bytes,
            "output_checksums": self.output_checksums,
            "correctness": {
                "all_passed": all_passed,
                "per_sample": self.correctness,
            },
        })
    }
}

fn main() -> BenchResult<()> {
    let Some(config) = parse_args()? else {
        return Ok(());
    };

    let mut reports = Vec::with_capacity(config.sizes.len());
    for size in config.sizes.iter().copied() {
        reports.push(run_size(size, config.samples)?);
    }

    let report = json!({
        "benchmark": "cutterhoochee-editor",
        "version": 1,
        "clock": "std::time::Instant",
        "sizes": config.sizes,
        "samples": config.samples,
        "history_entries_for_store_cases": RETAINED_HISTORY_ENTRIES,
        "reports": reports,
        "limitations": [
            "Model and store workloads are measured separately; fixture construction and store seeding are excluded.",
            "Store cases include real project.json persistence and descriptor-checked temporary-disk I/O.",
            "Render-plan compilation is not included because it requires a managed artifact/font resolver and is not a pure model benchmark.",
        ],
    });
    let rendered = serde_json::to_string_pretty(&report)?;
    if let Some(path) = config.output {
        fs::write(path, rendered.as_bytes())?;
    }
    println!("{rendered}");
    Ok(())
}

fn parse_args() -> BenchResult<Option<Config>> {
    let mut sizes = DEFAULT_SIZES.to_vec();
    let mut samples = DEFAULT_SAMPLES;
    let mut output = None;
    let mut args = std::env::args().skip(1);

    while let Some(argument) = args.next() {
        if argument == "--help" || argument == "-h" {
            eprintln!(
                "Usage: benchmark-editor [--sizes 100,1000,5000] [--samples 7] [--output PATH]"
            );
            return Ok(None);
        }
        if let Some(value) = argument.strip_prefix("--sizes=") {
            sizes = parse_sizes(value)?;
            continue;
        }
        if argument == "--sizes" {
            sizes = parse_sizes(&next_argument(&mut args, "--sizes")?)?;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--samples=") {
            samples = parse_samples(value)?;
            continue;
        }
        if argument == "--samples" {
            samples = parse_samples(&next_argument(&mut args, "--samples")?)?;
            continue;
        }
        if let Some(value) = argument.strip_prefix("--output=") {
            output = Some(PathBuf::from(value));
            continue;
        }
        if argument == "--output" {
            output = Some(PathBuf::from(next_argument(&mut args, "--output")?));
            continue;
        }
        return Err(cli_error(format!("unknown argument: {argument}")));
    }

    if sizes.is_empty() {
        return Err(cli_error("--sizes must contain at least one positive size"));
    }
    if samples == 0 {
        return Err(cli_error("--samples must be positive"));
    }
    Ok(Some(Config {
        sizes,
        samples,
        output,
    }))
}

fn next_argument<I>(args: &mut I, option: &str) -> BenchResult<String>
where
    I: Iterator<Item = String>,
{
    args.next()
        .ok_or_else(|| cli_error(format!("{option} requires a value")))
}

fn parse_sizes(value: &str) -> BenchResult<Vec<usize>> {
    if value.trim().is_empty() {
        return Err(cli_error("--sizes must not be empty"));
    }
    value
        .split(',')
        .map(|part| {
            let size = part
                .trim()
                .parse::<usize>()
                .map_err(|_| cli_error(format!("invalid project size: {part}")))?;
            if size == 0 {
                return Err(cli_error("project sizes must be positive"));
            }
            Ok(size)
        })
        .collect()
}

fn parse_samples(value: &str) -> BenchResult<usize> {
    let samples = value
        .trim()
        .parse::<usize>()
        .map_err(|_| cli_error(format!("invalid sample count: {value}")))?;
    if samples == 0 {
        return Err(cli_error("--samples must be positive"));
    }
    Ok(samples)
}

fn cli_error(message: impl Into<String>) -> Box<dyn Error> {
    io::Error::new(io::ErrorKind::InvalidInput, message.into()).into()
}

fn run_size(size: usize, samples: usize) -> BenchResult<Value> {
    let fixture = build_fixture(size)?;
    let input = &fixture.input_bytes;
    let mut workloads = (0..WORKLOAD_COUNT)
        .map(|index| TimedWorkload::new(workload_name(index), size, samples))
        .collect::<Vec<_>>();

    for _sample in 0..samples {
        let start = Instant::now();
        fixture.document.validate()?;
        let elapsed = start.elapsed();
        workloads[WORKLOAD_VALIDATE].record(elapsed, input, input, true);

        let mut no_op_candidate = fixture.document.clone();
        let start = Instant::now();
        apply_batch(&mut no_op_candidate, &[], &[])?;
        let elapsed = start.elapsed();
        assert_eq!(no_op_candidate, fixture.document);
        let no_op_output = serde_json::to_vec(&no_op_candidate)?;
        workloads[WORKLOAD_NOOP].record(elapsed, input, &no_op_output, true);

        let mut gain_candidate = fixture.document.clone();
        let start = Instant::now();
        apply_batch(
            &mut gain_candidate,
            std::slice::from_ref(&fixture.gain_operation),
            &[],
        )?;
        let elapsed = start.elapsed();
        assert_eq!(gain_candidate, fixture.gained_document);
        let gain_output = serde_json::to_vec(&gain_candidate)?;
        workloads[WORKLOAD_GAIN].record(elapsed, input, &gain_output, true);

        let start = Instant::now();
        let delta = fixture.document.diff(&fixture.gained_document)?;
        let elapsed = start.elapsed();
        assert_eq!(delta.changes.len(), 1);
        assert_eq!(delta.changes[0].entity.id, fixture.gain_clip_id);
        let delta_output = serde_json::to_vec(&delta)?;
        workloads[WORKLOAD_DIFF].record(elapsed, input, &delta_output, true);

        let start = Instant::now();
        let captions = fixture.document.projected_captions()?;
        let elapsed = start.elapsed();
        assert_eq!(captions.len(), fixture.caption_count);
        let captions_output = serde_json::to_vec(&captions)?;
        workloads[WORKLOAD_CAPTIONS].record(elapsed, input, &captions_output, true);
    }

    for sample in 0..samples {
        run_store_snapshot(&fixture, size, sample, &mut workloads[WORKLOAD_SNAPSHOT])?;
        run_store_dry_run(&fixture, size, sample, &mut workloads[WORKLOAD_DRY_RUN])?;
        let (commit_and_prior, undo_and_redo) = workloads.split_at_mut(WORKLOAD_UNDO);
        let commit_workload = &mut commit_and_prior[WORKLOAD_COMMIT];
        let (undo_only, redo_only) = undo_and_redo.split_at_mut(1);
        let undo_workload = &mut undo_only[0];
        let redo_workload = &mut redo_only[0];
        run_store_commit_history(
            &fixture,
            size,
            sample,
            commit_workload,
            undo_workload,
            redo_workload,
        )?;
        run_store_open(&fixture, size, sample, &mut workloads[WORKLOAD_OPEN])?;
    }

    let workload_values = workloads
        .into_iter()
        .map(TimedWorkload::into_json)
        .collect::<Vec<_>>();
    Ok(json!({
        "size": size,
        "fixture": {
            "assets": fixture.document.assets.len(),
            "clips": fixture.document.clips.len(),
            "captions": fixture.caption_count,
            "clips_with_captions": fixture.clips_with_captions,
            "max_captions_per_clip": fixture.max_captions_per_clip,
            "serialized_document_bytes": fixture.input_bytes.len(),
            "serialized_gained_document_bytes": fixture.gained_bytes.len(),
            "document_checksum": fixture.input_checksum,
            "gained_document_checksum": fixture.gained_checksum,
        },
        "workloads": workload_values,
    }))
}

fn workload_name(index: usize) -> &'static str {
    match index {
        WORKLOAD_VALIDATE => "document_validate",
        WORKLOAD_NOOP => "apply_batch_noop",
        WORKLOAD_GAIN => "apply_batch_gain",
        WORKLOAD_DIFF => "document_diff_one_change",
        WORKLOAD_CAPTIONS => "projected_captions",
        WORKLOAD_SNAPSHOT => "store_snapshot",
        WORKLOAD_DRY_RUN => "store_dry_run_gain",
        WORKLOAD_COMMIT => "store_commit_gain",
        WORKLOAD_UNDO => "store_undo",
        WORKLOAD_REDO => "store_redo",
        WORKLOAD_OPEN => "store_open_retained_history",
        _ => unreachable!("unknown workload index"),
    }
}

fn build_fixture(size: usize) -> BenchResult<Fixture> {
    let project_id = fixed_uuid(PROJECT_KIND, 0);
    let asset_id = fixed_uuid(ASSET_KIND, 0);
    let video_track_id = fixed_uuid(VIDEO_TRACK_KIND, 0);
    let audio_track_id = fixed_uuid(AUDIO_TRACK_KIND, 0);
    let text_track_id = fixed_uuid(TEXT_TRACK_KIND, 0);
    let fps = FrameRate::new(30, 1)?;
    let profile = ProjectProfile::for_aspect(AspectRatio::Landscape, fps)?;
    let frame_count = CLIP_DURATION_FRAMES;
    let asset = AssetManifest {
        id: asset_id.clone(),
        kind: AssetKind::Video,
        content_hash: "benchmark-video-content-v1".to_owned(),
        original: OriginalMediaMetadata {
            file_name: "benchmark-source.mp4".to_owned(),
            location: None,
            byte_size: Some(1_024),
            modified_time_ms: None,
            streams: vec![OriginalStreamMetadata {
                kind: OriginalStreamKind::Video,
                codec: "h264".to_owned(),
                duration_ms: Some(1_000),
                start_time_ms: Some(0),
                width: Some(1_920),
                height: Some(1_080),
                ..Default::default()
            }],
        },
        normalization: Some(NormalizedAsset {
            renderer_version: "benchmark-renderer-v1".to_owned(),
            epoch_ms: 0,
            video: Some(NormalizedVideo {
                master_artifact_id: "benchmark-video-master".to_owned(),
                proxy_artifact_id: None,
                frame_count,
                width: 1_920,
                height: 1_080,
                fps_num: 30,
                fps_den: 1,
                active_start_frame: 0,
                active_end_frame: frame_count,
                source_start_ms: 0,
                source_end_ms: 1_000,
                proxy_frame_count: Some(frame_count),
            }),
            audio: None,
        }),
    };
    let tracks = vec![
        Track::new(
            video_track_id.clone(),
            TrackKind::Video,
            "Main Video".to_owned(),
        )?,
        Track::new(audio_track_id, TrackKind::Audio, "Main Audio".to_owned())?,
        Track::new(text_track_id.clone(), TrackKind::Text, "Text".to_owned())?,
    ];
    let mut clips = Vec::with_capacity(size);
    for index in 0..size {
        let start_frame = (index as u64)
            .checked_mul(CLIP_DURATION_FRAMES)
            .ok_or_else(|| cli_error("project size overflows the frame range"))?;
        clips.push(MediaClip {
            id: fixed_uuid(CLIP_KIND, index),
            track_id: video_track_id.clone(),
            asset_id: asset_id.clone(),
            start_frame,
            in_frame: 0,
            duration_frames: CLIP_DURATION_FRAMES,
            fit: FitMode::Contain,
            center_x: 5_000,
            center_y: 5_000,
            scale: 10_000,
            opacity: 10_000,
            gain_db: 0.0,
            audio_enabled: false,
            fade_in_frames: 0,
            fade_out_frames: 0,
        });
    }

    let mut text_items = Vec::with_capacity(size + size / 3 + 1);
    let mut caption_count = 0usize;
    let mut clips_with_captions = 0usize;
    let mut max_captions_per_clip = 0usize;
    for index in 0..size {
        let per_clip = if index % 4 == 0 { 2 } else { 1 };
        clips_with_captions += 1;
        caption_count += per_clip;
        max_captions_per_clip = max_captions_per_clip.max(per_clip);
        for slot in 0..per_clip {
            let (source_start_frame, source_duration_frames) = if per_clip == 2 {
                (slot as u64 * 15, 15)
            } else {
                (0, 20)
            };
            text_items.push(TextItem {
                id: fixed_uuid(CAPTION_KIND, index * 2 + slot),
                track_id: text_track_id.clone(),
                kind: TextKind::Caption,
                text: format!("Caption {index:05}.{slot}"),
                style: TextStyle::Clean,
                color: RgbaColor {
                    red: 255,
                    green: 255,
                    blue: 255,
                    alpha: 255,
                },
                font_size: 48,
                position_x: 5_000,
                position_y: 8_500,
                line_breaks: Vec::new(),
                start_frame: None,
                duration_frames: None,
                owner_clip_id: Some(fixed_uuid(CLIP_KIND, index)),
                source_start_frame: Some(source_start_frame),
                source_duration_frames: Some(source_duration_frames),
            });
        }
    }

    let document = ProjectDocument {
        project_id,
        name: "Editor benchmark".to_owned(),
        revision: 0,
        profile,
        assets: vec![asset],
        tracks,
        clips,
        text_items,
        transitions: Vec::new(),
    };
    document.validate()?;

    let gain_clip_id = fixed_uuid(CLIP_KIND, size - 1);
    let gained_document = document_with_gain(&document, &gain_clip_id);
    gained_document.validate()?;
    let gain_operation = EditOp::UpdateClip {
        clip_id: gain_clip_id.clone(),
        patch: ClipPatch {
            gain_db: Some(GAIN_DB),
            ..Default::default()
        },
    };
    let input_bytes = serde_json::to_vec(&document)?;
    let gained_bytes = serde_json::to_vec(&gained_document)?;
    Ok(Fixture {
        document,
        gained_document,
        gain_clip_id,
        gain_operation,
        input_checksum: checksum(&input_bytes),
        gained_checksum: checksum(&gained_bytes),
        input_bytes,
        gained_bytes,
        caption_count,
        clips_with_captions,
        max_captions_per_clip,
    })
}

fn document_with_gain(document: &ProjectDocument, clip_id: &str) -> ProjectDocument {
    let mut gained = document.clone();
    let clip = gained
        .clips
        .iter_mut()
        .find(|clip| clip.id == clip_id)
        .expect("gain clip must exist");
    clip.gain_db = GAIN_DB;
    gained
}

fn seed_store(
    fixture: &Fixture,
    size: usize,
    sample: usize,
    label: &str,
) -> BenchResult<PreparedStore> {
    let root = temporary_path(label, size, sample, "root");
    let app_data = temporary_path(label, size, sample, "app-data");
    let store = ProjectStore::create(&root, &app_data, "Editor benchmark", Some("16:9"), 30, 1)?;
    let initial = store.snapshot()?.document;
    let video_track_id = initial
        .tracks
        .iter()
        .find(|track| track.kind == TrackKind::Video)
        .expect("created store has a video track")
        .id
        .clone();
    let text_track_id = initial
        .tracks
        .iter()
        .find(|track| track.kind == TrackKind::Text)
        .expect("created store has a text track")
        .id
        .clone();
    let asset = fixture.document.assets[0].clone();
    let mut clips = fixture.document.clips.clone();
    for clip in &mut clips {
        clip.track_id = video_track_id.clone();
    }
    let mut text_items = fixture.document.text_items.clone();
    for item in &mut text_items {
        item.track_id = text_track_id.clone();
    }

    // This is deliberately one direct, unmeasured seed closure. It avoids
    // turning fixture construction into N measured InsertClip/AddText ops.
    store.commit(
        fixed_uuid(SEED_TRANSACTION_KIND, 0),
        0,
        "benchmark seed".to_owned(),
        "benchmark-seed-payload".to_owned(),
        move |document| {
            document.assets.extend([asset]);
            document.clips.extend(clips);
            document.text_items.extend(text_items);
            Ok(())
        },
    )?;

    let mut revision = store.snapshot()?.document.revision;
    for history_index in 0..RETAINED_HISTORY_ENTRIES.saturating_sub(1) {
        let history_clip_id = fixture.document.clips[history_index % fixture.document.clips.len()]
            .id
            .clone();
        let history_gain = -1.0 - (history_index as f64 / 100.0);
        let history_clip_id_for_callback = history_clip_id.clone();
        let result = store.commit(
            fixed_uuid(HISTORY_TRANSACTION_KIND, history_index),
            revision,
            format!("benchmark history {history_index}"),
            format!("benchmark-history-payload-{history_index}"),
            move |document| {
                let clip = document
                    .clips
                    .iter_mut()
                    .find(|clip| clip.id == history_clip_id_for_callback)
                    .expect("history clip must exist");
                clip.gain_db = history_gain;
                Ok(())
            },
        )?;
        revision = result.revision;
    }
    let baseline = store.snapshot()?.document;
    assert_eq!(baseline.clips.len(), size);
    assert_eq!(baseline.text_items.len(), fixture.document.text_items.len());
    baseline.validate()?;
    Ok(PreparedStore {
        store,
        baseline,
        root,
        app_data,
    })
}

fn run_store_snapshot(
    fixture: &Fixture,
    size: usize,
    sample: usize,
    workload: &mut TimedWorkload,
) -> BenchResult<()> {
    let prepared = seed_store(fixture, size, sample, "snapshot")?;
    let input = serde_json::to_vec(&prepared.baseline)?;
    let start = Instant::now();
    let snapshot = prepared.store.snapshot()?;
    let elapsed = start.elapsed();
    assert_eq!(snapshot.document, prepared.baseline);
    let output = serde_json::to_vec(&snapshot.document)?;
    workload.record(elapsed, &input, &output, true);
    cleanup_store(prepared);
    Ok(())
}

fn run_store_dry_run(
    fixture: &Fixture,
    size: usize,
    sample: usize,
    workload: &mut TimedWorkload,
) -> BenchResult<()> {
    let prepared = seed_store(fixture, size, sample, "dry-run")?;
    let input = serde_json::to_vec(&prepared.baseline)?;
    let expected_revision = prepared.baseline.revision;
    let operation = fixture.gain_operation.clone();
    let start = Instant::now();
    let result = prepared.store.dry_run(
        fixed_uuid(DRY_RUN_TRANSACTION_KIND, 0),
        expected_revision,
        "benchmark dry run gain".to_owned(),
        "benchmark-dry-run-payload".to_owned(),
        move |document| apply_batch(document, std::slice::from_ref(&operation), &[]),
    )?;
    let elapsed = start.elapsed();
    assert!(result.changed);
    assert_eq!(result.revision, expected_revision);
    assert_eq!(result.affected_entities.len(), 1);
    assert_eq!(result.affected_entities[0].id, fixture.gain_clip_id);
    let after = prepared.store.snapshot()?;
    assert_eq!(after.document, prepared.baseline);
    let output = serde_json::to_vec(&result)?;
    workload.record(elapsed, &input, &output, true);
    cleanup_store(prepared);
    Ok(())
}

fn run_store_commit_history(
    fixture: &Fixture,
    size: usize,
    sample: usize,
    commit_workload: &mut TimedWorkload,
    undo_workload: &mut TimedWorkload,
    redo_workload: &mut TimedWorkload,
) -> BenchResult<()> {
    let prepared = seed_store(fixture, size, sample, "history")?;
    let input = serde_json::to_vec(&prepared.baseline)?;
    let gained = document_with_gain(&prepared.baseline, &fixture.gain_clip_id);
    let operation = fixture.gain_operation.clone();
    let transaction_id = fixed_uuid(COMMIT_TRANSACTION_KIND, 0);
    let expected_revision = prepared.baseline.revision;

    let start = Instant::now();
    let commit_result = prepared.store.commit(
        transaction_id.clone(),
        expected_revision,
        "benchmark commit gain".to_owned(),
        "benchmark-commit-payload".to_owned(),
        move |document| apply_batch(document, std::slice::from_ref(&operation), &[]),
    )?;
    let commit_elapsed = start.elapsed();
    assert!(commit_result.changed);
    let committed = prepared.store.snapshot()?;
    assert!(same_content(&committed.document, &gained));
    let commit_output = serde_json::to_vec(&committed.document)?;
    commit_workload.record(commit_elapsed, &input, &commit_output, true);

    let start = Instant::now();
    let undo_result = prepared.store.history(
        HistoryAction::Undo,
        commit_result.revision,
        Some(transaction_id.clone()),
    )?;
    let undo_elapsed = start.elapsed();
    assert!(undo_result.changed);
    let undone = prepared.store.snapshot()?;
    assert!(same_content(&undone.document, &prepared.baseline));
    let undo_output = serde_json::to_vec(&undone.document)?;
    undo_workload.record(undo_elapsed, &input, &undo_output, true);

    let start = Instant::now();
    let redo_result = prepared.store.history(
        HistoryAction::Redo,
        undo_result.revision,
        Some(transaction_id),
    )?;
    let redo_elapsed = start.elapsed();
    assert!(redo_result.changed);
    let redone = prepared.store.snapshot()?;
    assert!(same_content(&redone.document, &gained));
    let redo_output = serde_json::to_vec(&redone.document)?;
    redo_workload.record(redo_elapsed, &input, &redo_output, true);

    let envelope = prepared.store.envelope()?;
    assert_eq!(envelope.history.undo.len(), RETAINED_HISTORY_ENTRIES + 1);
    envelope.validate_open()?;
    cleanup_store(prepared);
    Ok(())
}

fn run_store_open(
    fixture: &Fixture,
    size: usize,
    sample: usize,
    workload: &mut TimedWorkload,
) -> BenchResult<()> {
    let prepared = seed_store(fixture, size, sample, "open")?;
    let input = serde_json::to_vec(&prepared.baseline)?;
    let expected = prepared.baseline.clone();
    let root = prepared.root.clone();
    let app_data = prepared.app_data.clone();
    drop(prepared.store);

    let start = Instant::now();
    let reopened = ProjectStore::open(&root, &app_data)?;
    let elapsed = start.elapsed();
    let envelope = reopened.envelope()?;
    assert_eq!(envelope.history.undo.len(), RETAINED_HISTORY_ENTRIES);
    envelope.validate_open()?;
    let snapshot = reopened.snapshot()?;
    assert!(same_content(&snapshot.document, &expected));
    let output = serde_json::to_vec(&snapshot.document)?;
    workload.record(elapsed, &input, &output, true);
    drop(reopened);
    cleanup_paths(root, app_data);
    Ok(())
}

fn same_content(left: &ProjectDocument, right: &ProjectDocument) -> bool {
    let mut left = left.clone();
    let mut right = right.clone();
    left.revision = 0;
    right.revision = 0;
    left == right
}

fn cleanup_store(prepared: PreparedStore) {
    drop(prepared.store);
    cleanup_paths(prepared.root, prepared.app_data);
}

fn cleanup_paths(root: PathBuf, app_data: PathBuf) {
    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_dir_all(app_data);
}

fn temporary_path(label: &str, size: usize, sample: usize, suffix: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    std::env::temp_dir().join(format!(
        "cutterhoochee-editor-benchmark-{label}-{}-{size}-{sample}-{stamp}-{suffix}",
        std::process::id(),
    ))
}

fn fixed_uuid(kind: u32, index: usize) -> String {
    format!("{kind:08x}-0000-4000-8000-{:012x}", index as u64 + 1)
}

fn elapsed_ns(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

fn checksum(bytes: &[u8]) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("fnv1a64:{hash:016x}")
}
