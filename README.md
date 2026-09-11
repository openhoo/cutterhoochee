# Cutterhoochee

A local-first desktop video editor with multitrack editing, captions, local transcription, and an optional Pi-powered assistant.

## User documentation / Benutzerdokumentation

**[Read online / Online lesen](https://openhoo.github.io/cutterhoochee/)** · [Source / Quellcode](https://github.com/openhoo/cutterhoochee) · [Releases](https://github.com/openhoo/cutterhoochee/releases)

**[Open the offline documentation / Offline-Dokumentation öffnen](docs/index.html)** — open this local HTML file in a browser; no server is required.

```bash
xdg-open docs/index.html
```

- **[English — Complete user guide](docs/user-guide.en.md)**: installation, first project, workspace, editing, playback, transcription, assistant permissions, export, shortcuts, troubleshooting, and privacy.
- **[Deutsch — Vollständiges Benutzerhandbuch](docs/user-guide.de.md)**: Installation, erstes Projekt, Arbeitsoberfläche, Schnitt, Wiedergabe, Transkription, Assistent und Berechtigungen, Export, Tastenkürzel, Fehlerbehebung und Datenschutz.

Both guides include screenshots and animated GIF demonstrations. The application currently uses English interface labels; the German guide explains those same labels in German.

Beide Handbücher enthalten Screenshots und animierte GIF-Demonstrationen. Die Anwendung verwendet derzeit englische Oberflächenbeschriftungen; das deutsche Handbuch erläutert genau diese Beschriftungen auf Deutsch.

![Cutterhoochee workspace with generated demonstration footage](docs/assets/workspace-dark.png)

## Run the Linux package / Linux-Paket starten

After a local build, the Linux x64 AppImage is written to the following path (the version follows `VERSION`). This is a build-output location, not a hosted download link. A source checkout does not contain prebuilt binaries.

Nach einem lokalen Build liegt das Linux-x64-AppImage unter folgendem Pfad (die Version folgt `VERSION`). Dies ist ein Build-Ausgabepfad, kein gehosteter Download-Link. Ein Quellcode-Checkout enthält keine vorgefertigten Binärdateien.

```bash
appimage="src-tauri/target/release/bundle/appimage/Cutterhoochee_$(cat VERSION)_amd64.AppImage"
chmod +x "$appimage"
"$appimage"
```

The Linux package has been exercised on Linux x64; Windows and macOS are not claimed as verified releases. Local transcription requires a speech model downloaded with your approval. Cloud assistance is optional and requires a supported provider connection. See the guides for installation details, consent boundaries, and recovery advice.

Das Linux-Paket wurde unter Linux x64 erprobt; Windows und macOS sind nicht als verifizierte Veröffentlichungen ausgewiesen. Für die lokale Transkription ist ein mit Ihrer Zustimmung heruntergeladenes Sprachmodell erforderlich. Cloud-Unterstützung ist optional und setzt eine unterstützte Anbieter-Verbindung voraus. Installationshinweise, Zustimmungsgrenzen und Hinweise zur Fehlerbehebung stehen in den Handbüchern.

## Build and release

The source includes the Rust/Tauri desktop application, the TypeScript UI, the Pi agent, shared IPC contracts, build scripts, and documentation. Native sidecars, generated runtime manifests, and local editing projects are deliberately excluded from Git.

The current packaging implementation targets **Arch Linux x64**. It requires Node.js >=22.19, the pnpm version pinned in `package.json`, Rust, the GTK/WebKit/GStreamer development libraries, and the exact Arch FFmpeg/x264 provenance checked by `scripts/prepare-sidecars.mjs`. The release workflow fixes the native environment; do not assume the same packaging script works on an arbitrary Ubuntu host.

```sh
pnpm install --frozen-lockfile
pnpm bundle:linux
```

The build stages checksum-verified runtime dependencies; speech models are downloaded only through the application's separate consent flow. See [Contributing](CONTRIBUTING.md) for development and verification commands.

OpenHoo's shared tools provide Conventional Commit checks, policy/security analysis, dependency checks, and Hooversion release automation. `VERSION` is authoritative; the release hook synchronizes all Rust, Tauri, and Node versions. GitHub Pages serves the documentation from this repository's `docs` directory. Source publication and a verified binary release are separate operations.

After successful `main` CI, Hooversion prepares a release branch when Conventional Commits require a version bump. Organization policy requires a maintainer to open the PR using the prepared workflow link, review it, and preserve the generated squash subject and body. The merge must pass exact-commit CI before its annotated tag can trigger packaging. Published releases are immutable; rerunning publication never replaces their assets.

## License and security

Cutterhoochee's own code and documentation are licensed under [Apache-2.0](LICENSE). Third-party components retain their licenses, including Inter's OFL and the native media tools' GPL terms. Their notices and source provenance must accompany binary distributions; the application license does not relicense those components.

Report vulnerabilities through the private process in [SECURITY.md](SECURITY.md). Contributors follow [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).

## Illustrations / Abbildungen

Screenshots show generated demonstration media, not personal recordings. GIFs are silent, reduced-rate workflow illustrations, not performance measurements. [Asset notes and descriptions / Hinweise und Beschreibungen der Abbildungen](docs/assets/README.md).
