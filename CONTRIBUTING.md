# Contributing

Open an issue before a large editing, persistence, rendering, permission, or public-API change. Small fixes may go directly to a pull request. Follow the [code of conduct](CODE_OF_CONDUCT.md); report vulnerabilities privately using [SECURITY.md](SECURITY.md).

## Development

Use Node.js >=22.19, the pinned pnpm version in `package.json`, and Rust with the Linux GTK/WebKit/GStreamer development dependencies. The current native packaging path is Arch Linux x64 and requires the exact FFmpeg/x264 package provenance supported by `scripts/prepare-sidecars.mjs`. It is not a generic Ubuntu or cross-platform release recipe.

```sh
pnpm install --frozen-lockfile
pnpm build:agent
pnpm build:web
pnpm prepare:sidecars
pnpm test:unit
cargo test --locked --manifest-path src-tauri/Cargo.toml
pnpm version:check
```

Preparing sidecars downloads checksum-pinned Node and whisper.cpp inputs and stages the host's media tools; it does not download a speech model. A model download is a separate, explicit runtime consent operation.

For a desktop package, run `pnpm bundle:linux`. For an interactive desktop development session, run `pnpm dev` after staging. A browser-only frontend is not proof of native file access, rendering, audio, or permissions.

## Rendering and output ownership

The native pipeline is `ProjectDocument → RenderPlan → canonical RGBA/PCM → FFmpeg → verified output`. Preview and export consume the same immutable plan; neither output transport nor encoding defines a second timeline compositor.

| Module under `src-tauri/src/media/` | Responsibility |
| --- | --- |
| `render_plan.rs` | Compile and validate revision-pinned geometry, timing, audio envelopes, and artifact references. |
| `render/mod.rs` | Preview IPC, runtime state, revision capture, and transport coordination. |
| `render/frame.rs` | Shared layer composition and the stateless/pooled canonical RGBA renderers. |
| `render/audio.rs` | Indexed 48 kHz stereo mixing. |
| `render/decoder.rs`, `render/cache.rs` | Owned decoder processes and bounded artifact/raster caches. |
| `render/software.rs` | Reduced-resolution preview, JPEG transport, pacing, acknowledgements, and cancellation. |
| `export/mod.rs`, `export/runtime.rs` | Public export contracts, approval-gated orchestration, and job lifecycle. |
| `export/encode.rs` | Buffered PCM production, pooled frame streaming, encoding, and output verification. |
| `export/output.rs`, `export/subtitle.rs` | Destination identity, artifact pins, temporary files, rollback-safe publication, and SRT projection. |
| `ffmpeg.rs`, `jobs.rs` | Typed media commands and supervised, cancellable process execution. |

Keep output geometry in `ExportVideoSettings`; do not patch FFmpeg arguments after construction. Flush and synchronize intermediate audio before encoding. Publish MP4/SRT only after stream verification and exact destination approval checks; retain existing rollback and cancellation semantics.

For rendering changes, compare stateless and pooled frame hashes with `pnpm benchmark:render --project /absolute/fixture.cutproj --resources /absolute/repository/src-tauri`. Also exercise native playback, both export resolutions, optional SRT, and cancellation using disposable media. Compilation alone does not prove output fidelity or process cleanup.

## Change verification

Keep regression coverage tied to observable behavior: atomic project edits, durable persistence, stale-generation rejection, explicit permissions, and preview/export consistency. Use generated footage and disposable project directories. Do not commit application state, credentials, personal media, downloaded runtimes, or build output.

Rust owns the shared IPC declarations. After changing them, regenerate rather than editing `shared/src/generated.ts` by hand:

```sh
cargo run --locked --example generate-types --manifest-path src-tauri/Cargo.toml
pnpm build:shared
```

## Commits and releases

Use Conventional Commits. Pull requests must explain compatibility and security impact. Maintainers use a Conventional Commit title when squash-merging. Include lockfile updates with dependency changes.

`VERSION` is the canonical application release version. Hooversion updates it and the changelog, then runs `node scripts/release/sync-version.mjs` to synchronize the Rust, Tauri, and Node manifests. `pnpm version:check` detects drift. Do not independently bump one package or manually publish an unverified local AppImage.

The GitHub workflows and pinned OpenHoo tools define the CI and release gates. A successful source check does not by itself qualify a binary release; inspect the native build, checksums, SBOM, signatures, attestations, and publication verification.
