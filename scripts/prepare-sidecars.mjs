import {
  access,
  chmod,
  copyFile,
  mkdir,
  mkdtemp,
  readdir,
  readFile,
  realpath,
  rename,
  rm,
  symlink,
  writeFile,
} from "node:fs/promises";
import { constants as fsConstants } from "node:fs";
import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import { pipeline } from "node:stream/promises";
import { Readable } from "node:stream";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const binaryDir = resolve(root, "src-tauri", "binaries");
const noticeDir = resolve(root, "src-tauri", "resources", "notices");
const targetTriple = process.env.CUTTERHOOCHEE_TARGET_TRIPLE ?? "x86_64-unknown-linux-gnu";
const cacheDir = resolve(
  process.env.CUTTERHOOCHEE_SIDECAR_CACHE ?? join(tmpdir(), "cutterhoochee-sidecars"),
);
const sourceRoot = "https://github.com/ggerganov/whisper.cpp.git";
const whisperCommit = "306c88f4d1286aec1bf96e544632897886af5501";
const whisperRelease = "1.9.2";
const nodeVersion = "22.23.2";
const nodeArchive = "node-v22.23.2-linux-x64.tar.xz";
const nodeArchiveSha256 =
  "d60acfe00a2932254bb0ad20e01b0d74397a0875595de719654b214f4b03f307";
const nodeArchiveUrl = `https://nodejs.org/dist/v${nodeVersion}/${nodeArchive}`;
const nodeArchiveChecksumUrl = `https://nodejs.org/dist/v${nodeVersion}/SHASUMS256.txt`;
const sourceOffers = {
  ffmpeg: {
    path: "sidecars/sources/ffmpeg-2-9.0.1-1-arch-recipe.tar.gz",
    bundlePath: "resources/notices/source-offers/ffmpeg-2-9.0.1-1-arch-recipe.tar.gz",
    sha256: "f41502f3328609206546a19a06cac0eaf46924740a05662e25c17c90ebdc2acd",
    repository: "https://gitlab.archlinux.org/archlinux/packaging/packages/ffmpeg.git",
    commit: "c8af7cd",
    packageVersion: "2:9.0.1-1",
    upstream: "git+https://git.ffmpeg.org/ffmpeg.git?signed#tag=n9.0.1",
    patch: "0001-Add-av_stream_get_first_dts-for-Chromium.patch",
    buildScript: "PKGBUILD",
  },
  x264: {
    path: "sidecars/sources/x264-3-0.165.r3222.b35605a-2-arch-recipe.tar.gz",
    bundlePath: "resources/notices/source-offers/x264-3-0.165.r3222.b35605a-2-arch-recipe.tar.gz",
    sha256: "f74c7c724652886b4a95863bcf2df17d2b8450264f2cc7dcbc93af260d1f8a51",
    repository: "https://gitlab.archlinux.org/archlinux/packaging/packages/x264.git",
    commit: "9376684",
    packageVersion: "3:0.165.r3222.b35605a-2",
    upstream: "https://code.videolan.org/videolan/x264.git",
    sourceCommit: "b35605ace3ddf7c1a5d67a2eb553f034aef41d55",
    buildScript: "PKGBUILD",
  },
};
const interRelease = "4.1";
const interArchiveUrl = "https://github.com/rsms/inter/releases/download/v4.1/Inter-4.1.zip";
const interArchiveSha256 = "9883fdd4a49d4fb66bd8177ba6625ef9a64aa45899767dde3d36aa425756b11e";
const interFiles = {
  "src/src/assets/fonts/InterVariable.ttf":
    "4989b125924991b90d05b2d16e0e388c48f7d5bb8b30539bbf9c755278d0ccaf",
  "src/src/assets/fonts/InterVariable-Italic.ttf":
    "d6f1f6a172d9e588438db9f986fd5cfad7b30f644374080a8a9d4d91e344586f",
  "src/src/assets/fonts/Inter-Regular.woff2":
    "338239f6b590b8ced3bf857654d32da3fd3663294cd3003651ed57aa3abd7aa1",
  "src/src/assets/fonts/Inter-SemiBold.woff2":
    "5013f48d77ab627b1db7c2415914284ef09abc3f60a8e0d0d8f3cd1bfebefb5e",
};
const nativeInterFiles = {
  "src/src/assets/fonts/InterVariable.ttf": "resources/fonts/InterVariable.ttf",
  "src/src/assets/fonts/InterVariable-Italic.ttf": "resources/fonts/InterVariable-Italic.ttf",
};

const env = {
  ...process.env,
  // Ambient production settings must never remove build-time dependencies from
  // the ordinary agent deployment. Rust still launches the packaged sidecar in
  // production mode after this preparation step.
  NODE_ENV: "development",
  npm_config_production: "false",
  PNPM_CONFIG_PRODUCTION: "false",
};

function fail(message) {
  throw new Error(`[prepare-sidecars] ${message}`);
}

function run(command, args, options = {}) {
  const result = spawnSync(command, args, {
    cwd: options.cwd ?? root,
    env: options.env ?? env,
    stdio: options.stdio ?? "inherit",
    encoding: "utf8",
    shell: false,
  });
  if (result.error) {
    fail(`${command} could not be started: ${result.error.message}`);
  }
  if (result.status !== 0) {
    fail(`${command} ${args.join(" ")} exited with ${result.status ?? "a signal"}`);
  }
  return result;
}

function capture(command, args, options = {}) {
  const result = spawnSync(command, args, {
    cwd: options.cwd ?? root,
    env: options.env ?? env,
    stdio: ["ignore", "pipe", "pipe"],
    encoding: "utf8",
    shell: false,
  });
  if (result.error) {
    fail(`${command} could not be started: ${result.error.message}`);
  }
  if (result.status !== 0) {
    fail(`${command} ${args.join(" ")} exited with ${result.status ?? "a signal"}: ${
      result.stderr?.trim() || result.stdout?.trim() || "no diagnostic"
    }`);
  }
  return `${result.stdout ?? ""}\n${result.stderr ?? ""}`.trim();
}

async function exists(path, mode = fsConstants.F_OK) {
  try {
    await access(path, mode);
    return true;
  } catch {
    return false;
  }
}

async function hashFile(path) {
  const bytes = await readFile(path);
  return createHash("sha256").update(bytes).digest("hex");
}

async function writeAtomic(path, content, mode = undefined) {
  await mkdir(dirname(path), { recursive: true });
  const temporary = `${path}.tmp-${process.pid}`;
  await writeFile(temporary, content, { encoding: "utf8", mode });
  await rename(temporary, path);
}

async function copyAtomic(source, destination, executable = false) {
  await mkdir(dirname(destination), { recursive: true });
  const temporary = `${destination}.tmp-${process.pid}`;
  await copyFile(source, temporary);
  if (executable) await chmod(temporary, 0o755);
  await rename(temporary, destination);
}

async function downloadVerified(url, destination, expectedSha256) {
  if (await exists(destination)) {
    const current = await hashFile(destination);
    if (current === expectedSha256) return;
    await rm(destination, { force: true });
  }
  const response = await fetch(url, { redirect: "error" });
  if (!response.ok || !response.body) {
    fail(`download failed (${response.status}) for ${url}`);
  }
  const temporary = `${destination}.tmp-${process.pid}`;
  await mkdir(dirname(destination), { recursive: true });
  try {
    const stream = Readable.fromWeb(response.body);
    await pipeline(stream, (await import("node:fs")).createWriteStream(temporary, { mode: 0o600 }));
    const actual = await hashFile(temporary);
    if (actual !== expectedSha256) {
      fail(`checksum mismatch for ${url}: expected ${expectedSha256}, got ${actual}`);
    }
    await rename(temporary, destination);
  } finally {
    await rm(temporary, { force: true });
  }
}

function parseVersion(output, binary) {
  const match = output.match(new RegExp(`^${binary} version\\s+([^\\s]+)`, "m"));
  return match?.[1] ?? null;
}

function parseConfiguration(output) {
  return output.match(/^configuration:\s*(.+)$/m)?.[1]?.trim() ?? null;
}

async function findExecutable(name) {
  const candidates = [
    process.env[`CUTTERHOOCHEE_${name.toUpperCase()}`],
    `/usr/bin/${name}`,
    `/usr/local/bin/${name}`,
    ...((process.env.PATH ?? "").split(":").filter(Boolean).map((entry) => join(entry, name))),
  ].filter(Boolean);
  for (const candidate of candidates) {
    if (await exists(candidate, fsConstants.X_OK)) return realpath(candidate);
  }
  fail(`${name} is unavailable; set CUTTERHOOCHEE_${name.toUpperCase()} to an executable`);
}

async function packageMetadata(binaryPath) {
  if (!(await exists("/usr/bin/pacman", fsConstants.X_OK))) return null;
  let query;
  try {
    query = capture("pacman", ["-Qo", binaryPath], { stdio: "pipe" });
  } catch {
    return null;
  }
  const match = query.match(/ is owned by (.+)$/m);
  if (!match) return null;
  const packageName = match[1].trim().split(/\s+/)[0];
  let details;
  try {
    details = capture("pacman", ["-Qi", packageName], { stdio: "pipe" });
  } catch {
    return null;
  }
  const value = (label) => details.match(new RegExp(`^${label}\\s*:\\s*(.+)$`, "m"))?.[1]?.trim();
  return {
    manager: "pacman",
    package: packageName,
    version: value("Version") ?? null,
    license: value("Licenses") ?? null,
    sourceUrl: value("URL") ?? null,
  };
}

function normalizeFfmpegLicense(value) {
  if (!value) return null;
  const normalized = value.trim().replace(/\s+/g, "-");
  if (!/^GPL-(?:2\.0-(?:only|or-later)|3\.0-(?:only|or-later))$/.test(normalized)) {
    fail(`unsupported FFmpeg package license ${value}; refusing to infer a redistributable license`);
  }
  return normalized;
}

function resolveFfmpegLicense(packageInfo, binaryPath) {
  if (!packageInfo?.package || !packageInfo.version || !packageInfo.license || !packageInfo.sourceUrl) {
    fail(`verified FFmpeg package metadata is unavailable for ${binaryPath}; refuse a host-only sidecar`);
  }
  const packageLicense = normalizeFfmpegLicense(packageInfo.license);
  if (!packageLicense) {
    fail(`FFmpeg package license is unavailable for ${binaryPath}; refusing to infer a redistributable license`);
  }
  return packageLicense;
}

async function stageHostTool(name, targetName) {
  const source = await findExecutable(name);
  const output = capture(source, ["-version"], { stdio: "pipe" });
  const version = parseVersion(output, name);
  const configuration = parseConfiguration(output);
  if (!version || !configuration) {
    fail(`${source} did not report a parseable version and configuration`);
  }
  if (!configuration.includes("--enable-gpl")) {
    fail(`${source} was not built with --enable-gpl; refusing an unlicensed FFmpeg sidecar`);
  }
  const destination = join(binaryDir, targetName);
  await copyAtomic(source, destination, true);
  const sha256 = await hashFile(destination);
  const packageInfo = await packageMetadata(source);
  const licenseSpdx = resolveFfmpegLicense(packageInfo, source);
  return {
    status: "ready",
    target: targetTriple,
    path: `binaries/${targetName}`,
    sha256,
    version,
    source: {
      kind: "host-development-binary",
      executable: source,
      releaseUrl: `https://ffmpeg.org/releases/ffmpeg-${version.replace(/^n/, "")}.tar.xz`,
      projectUrl: "https://ffmpeg.org/",
      package: packageInfo,
    },
    build: {
      configuration,
      versionOutput: output,
    },
    license: {
      spdx: licenseSpdx,
      reason: "This FFmpeg build enables GPL components (--enable-gpl); the SPDX declaration is taken from the verified host package metadata.",
      notice: `resources/notices/FFmpeg-${licenseSpdx}.txt`,
      buildNotice: "resources/notices/FFmpeg-BUILD-NOTICE.txt",
      sourceNotice: "resources/notices/FFmpeg-SOURCE.txt",
      runtimeNotice: "resources/notices/FFmpeg-RUNTIME-LIBRARIES.txt",
    },
    runtime: {
      linkage: "pending-bundled-dynamic",
    },
  };
}

function parseLddEntries(output) {
  const entries = [];
  for (const line of output.split(/\r?\n/)) {
    const resolved = line.match(/^\s*(\S+)\s+=>\s+(\/\S+)/);
    if (resolved) {
      entries.push({
        name: resolved[1].startsWith("/") ? resolved[1].split("/").pop() : resolved[1],
        path: resolved[2],
      });
      continue;
    }
    const direct = line.match(/^\s*(\/\S+)\s+\(/);
    if (direct) entries.push({ name: direct[1].split("/").pop(), path: direct[1] });
  }
  return entries;
}
function runtimeProvenance(source, packageInfo) {
  if (packageInfo?.package && packageInfo.version && packageInfo.license && packageInfo.sourceUrl) {
    return {
      kind: "distribution-package",
      manager: packageInfo.manager,
      package: packageInfo.package,
      version: packageInfo.version,
      license: packageInfo.license,
      sourceUrl: packageInfo.sourceUrl,
    };
  }
  if (source.startsWith(resolve(cacheDir)) && source.includes(whisperCommit)) {
    return {
      kind: "whisper.cpp-source-build",
      repository: sourceRoot,
      commit: whisperCommit,
      release: `v${whisperRelease}`,
      license: "MIT",
    };
  }
  return null;
}


async function stageRuntimeDependencies(toolPaths) {
  const roots = new Set(toolPaths.map((path) => resolve(path)));
  const visited = new Set();
  const sources = new Map();
  const aliases = new Map();
  const queue = [...toolPaths];
  while (queue.length > 0) {
    const candidate = queue.shift();
    if (!candidate || !candidate.startsWith("/")) continue;
    const source = await realpath(candidate).catch(() => null);
    if (!source || visited.has(source)) continue;
    visited.add(source);
    if (!(await exists(source, fsConstants.R_OK))) fail(`runtime dependency is unavailable: ${source}`);
    const output = capture("ldd", [source], { stdio: "pipe" });
    if (/\bnot found\b/.test(output)) fail(`runtime dependency is unresolved for ${source}`);
    if (!roots.has(source)) sources.set(source, output);
    for (const entry of parseLddEntries(output)) {
      const dependency = await realpath(entry.path).catch(() => null);
      if (!dependency) continue;
      if (!aliases.has(dependency)) aliases.set(dependency, new Set());
      aliases.get(dependency).add(entry.name);
      queue.push(dependency);
    }
  }
  const patchelf = await findExecutable("patchelf");
  const libraryDir = join(binaryDir, "lib");
  await rm(libraryDir, { recursive: true, force: true });
  await mkdir(libraryDir, { recursive: true });
  const destinationByName = new Map();
  const libraries = [];
  for (const source of sources.keys()) {
    const name = source.split("/").pop();
    if (!name) continue;
    const previous = destinationByName.get(name);
    if (previous && previous !== source) {
      fail(`runtime libraries collide on ${name}: ${previous} and ${source}`);
    }
    destinationByName.set(name, source);
    const destination = join(libraryDir, name);
    await copyAtomic(source, destination);
    run(patchelf, ["--force-rpath", "--set-rpath", "$ORIGIN", destination], { stdio: "pipe" });
    const sha256 = await hashFile(destination);
    const packageInfo = await packageMetadata(source);
    const provenance = runtimeProvenance(source, packageInfo);
    if (!provenance) fail(`runtime dependency lacks stable source/license provenance: ${source}`);
    libraries.push({ name, path: `binaries/lib/${name}`, sha256, provenance });
    for (const alias of aliases.get(source) ?? []) {
      if (!alias || alias === name) continue;
      const previousAlias = destinationByName.get(alias);
      if (previousAlias && previousAlias !== source) {
        fail(`runtime library aliases collide on ${alias}: ${previousAlias} and ${source}`);
      }
      destinationByName.set(alias, source);
      const aliasPath = join(libraryDir, alias);
      await symlink(name, aliasPath);
      libraries.push({ name: alias, path: `binaries/lib/${alias}`, sha256, provenance, aliasOf: name });
    }
  }
  for (const toolPath of toolPaths) {
    run(patchelf, ["--force-rpath", "--set-rpath", "$ORIGIN/lib", toolPath], { stdio: "pipe" });
  }
  return {
    linkage: "bundled-dynamic",
    libraryDirectory: "binaries/lib",
    loaderStrategy: "ELF-RPATH-$ORIGIN",
    libraries,
  };
}

async function extractNodeArchive(archivePath, destination) {
  const extractRoot = await mkdtemp(join(tmpdir(), "cutterhoochee-node-"));
  try {
    run("tar", ["-xJf", archivePath, "-C", extractRoot, `${nodeArchive.slice(0, -7)}/bin/node`]);
    const extracted = join(extractRoot, nodeArchive.slice(0, -7), "bin", "node");
    if (!(await exists(extracted, fsConstants.X_OK))) fail(`Node archive has no ${extracted}`);
    await copyAtomic(extracted, destination, true);
    const reported = capture(destination, ["--version"], { stdio: "pipe" }).trim();
    if (reported !== `v${nodeVersion}`) fail(`staged Node reports ${reported}, expected v${nodeVersion}`);
    return hashFile(destination);
  } finally {
    await rm(extractRoot, { recursive: true, force: true });
  }
}

async function stageNode() {
  const archiveCandidates = [
    process.env.CUTTERHOOCHEE_NODE_ARCHIVE,
    join(cacheDir, nodeArchive),
  ].filter(Boolean);
  let archivePath;
  for (const candidate of archiveCandidates) {
    if (await exists(candidate, fsConstants.R_OK)) {
      archivePath = candidate;
      break;
    }
  }
  if (!archivePath) {
    archivePath = join(cacheDir, nodeArchive);
    await downloadVerified(nodeArchiveUrl, archivePath, nodeArchiveSha256);
  }
  archivePath = resolve(archivePath);
  if (!(await exists(archivePath))) fail(`Node archive is missing: ${archivePath}`);
  const archiveSha256 = await hashFile(archivePath);
  if (archiveSha256 !== nodeArchiveSha256) {
    fail(`Node archive checksum mismatch: expected ${nodeArchiveSha256}, got ${archiveSha256}`);
  }
  const destination = join(binaryDir, `node-${targetTriple}`);
  const binarySha256 = await extractNodeArchive(archivePath, destination);
  return {
    archivePath,
    status: "ready",
    version: nodeVersion,
    target: targetTriple,
    path: `binaries/node-${targetTriple}`,
    sha256: binarySha256,
    source: {
      kind: "official-node-release-archive",
      archive: nodeArchive,
      archiveUrl: nodeArchiveUrl,
      archiveSha256,
      archiveChecksumSource: nodeArchiveChecksumUrl,
      archiveMember: `${nodeArchive.slice(0, -7)}/bin/node`,
    },
    license: {
      spdx: "MIT",
      notice: "resources/notices/Node-LICENSE",
      sourceNotice: "resources/notices/Node-SOURCE.txt",
      runtimeNotice: "resources/notices/FFmpeg-RUNTIME-LIBRARIES.txt",
    },
  };
}

async function ensureWhisperSource() {
  await mkdir(cacheDir, { recursive: true });
  const sourceDir = join(cacheDir, `whisper.cpp-${whisperCommit}`);
  if (!(await exists(join(sourceDir, ".git")))) {
    await rm(sourceDir, { recursive: true, force: true });
    run("git", ["clone", "--no-checkout", sourceRoot, sourceDir], { stdio: "inherit" });
  }
  const current = capture("git", ["rev-parse", "HEAD"], { cwd: sourceDir, stdio: "pipe" }).trim();
  if (current !== whisperCommit) {
    run("git", ["fetch", "--depth", "1", "origin", whisperCommit], { cwd: sourceDir, stdio: "inherit" });
    run("git", ["checkout", "--detach", whisperCommit], { cwd: sourceDir, stdio: "inherit" });
  }
  const checkedOut = capture("git", ["rev-parse", "HEAD"], { cwd: sourceDir, stdio: "pipe" }).trim();
  if (checkedOut !== whisperCommit) fail(`whisper.cpp checkout is ${checkedOut}, expected ${whisperCommit}`);
  return sourceDir;
}

async function findNamedFile(directory, filename) {
  let entries;
  try {
    entries = await readdir(directory, { withFileTypes: true });
  } catch {
    return null;
  }
  for (const entry of entries) {
    const candidate = join(directory, entry.name);
    if (entry.isFile() && entry.name === filename) return candidate;
    if (entry.isDirectory() && entry.name !== ".git") {
      const found = await findNamedFile(candidate, filename);
      if (found) return found;
    }
  }
  return null;
}

async function stageWhisper() {
  const sourceDir = await ensureWhisperSource();
  const buildDir = join(cacheDir, `whisper-build-${whisperCommit}`);
  const cmakeFlags = [
    "-DCMAKE_BUILD_TYPE=Release",
    "-DWHISPER_BUILD_TESTS=OFF",
    "-DWHISPER_BUILD_EXAMPLES=ON",
    "-DWHISPER_BUILD_SERVER=OFF",
    "-DWHISPER_BUILD_BENCHMARKS=OFF",
    "-DWHISPER_SDL2=OFF",
  ];
  const binary = await findNamedFile(buildDir, "whisper-cli").catch(() => null);
  if (!binary || !(await exists(binary, fsConstants.X_OK))) {
    run("cmake", ["-S", sourceDir, "-B", buildDir, ...cmakeFlags], { stdio: "inherit" });
    run(
      "cmake",
      ["--build", buildDir, "--target", "whisper-cli", "--config", "Release", "--parallel", String(Math.max(2, Math.min(8, (Number(process.env.CUTTERHOOCHEE_BUILD_JOBS) || 4))))],
      { stdio: "inherit" },
    );
  }
  const finalBinary = await findNamedFile(buildDir, "whisper-cli");
  if (!finalBinary || !(await exists(finalBinary, fsConstants.X_OK))) {
    fail(`CMake completed without a whisper-cli executable under ${buildDir}`);
  }
  const destinationName = `whisper-cli-${targetTriple}`;
  const destination = join(binaryDir, destinationName);
  await copyAtomic(finalBinary, destination, true);
  const sha256 = await hashFile(destination);
  const licenseSource = join(sourceDir, "LICENSE");
  if (!(await exists(licenseSource))) fail("whisper.cpp source has no LICENSE file");
  await copyFile(licenseSource, join(noticeDir, "whisper.cpp-LICENSE"));
  const ggmlLicense = join(sourceDir, "ggml", "LICENSE");
  if (await exists(ggmlLicense)) await copyFile(ggmlLicense, join(noticeDir, "ggml-LICENSE"));
  return {
    status: "ready",
    target: targetTriple,
    path: `binaries/${destinationName}`,
    sha256,
    version: whisperRelease,
    source: {
      repository: sourceRoot,
      commit: whisperCommit,
      release: `v${whisperRelease}`,
    },
    build: {
      system: "CMake",
      type: "Release",
      flags: cmakeFlags,
      target: "whisper-cli",
    },
    license: {
      spdx: "MIT",
      notice: "resources/notices/whisper.cpp-LICENSE",
      additionalNotices: (await exists(ggmlLicense)) ? ["resources/notices/ggml-LICENSE"] : [],
      sourceNotice: "resources/notices/whisper.cpp-SOURCE.txt",
      runtimeNotice: "resources/notices/FFmpeg-RUNTIME-LIBRARIES.txt",
    },
  };
}

async function writeNotices({ node, nodeArchivePath, ffmpeg, ffprobe, whisper, runtime, sources }) {
  await mkdir(noticeDir, { recursive: true });
  await copyAtomic(join(root, "LICENSE"), join(noticeDir, "Cutterhoochee-LICENSE.txt"));
  const nodeLicense = join(noticeDir, "Node-LICENSE");
  const nodeSource = join(noticeDir, "Node-SOURCE.txt");
  const temporary = await mkdtemp(join(tmpdir(), "cutterhoochee-node-notice-"));
  try {
    const member = `${nodeArchive.slice(0, -7)}/LICENSE`;
    run("tar", ["-xJf", nodeArchivePath, "-C", temporary, member], { stdio: "pipe" });
    const extractedLicense = join(temporary, nodeArchive.slice(0, -7), "LICENSE");
    if (!(await exists(extractedLicense))) fail("verified Node archive has no LICENSE notice");
    await copyFile(extractedLicense, nodeLicense);
  } finally {
    await rm(temporary, { recursive: true, force: true });
  }
  const fullLicensePath = resolve(root, "src-tauri", ffmpeg.license.notice);
  if (!(await exists(fullLicensePath))) fail(`FFmpeg license text is missing: ${ffmpeg.license.notice}`);
  await writeFile(
    nodeSource,
    [
      `Node.js ${node.version} official release archive`,
      `Archive: ${node.source.archiveUrl}`,
      `Archive SHA-256: ${node.source.archiveSha256}`,
      `Checksum source: ${node.source.archiveChecksumSource}`,
      `Archive member: ${node.source.archiveMember}`,
      "The archive also contains npm and bundled third-party notices; retain the archive's full notice tree when redistributing the sidecar.",
      "",
    ].join("\n"),
  );
  await writeFile(
    join(noticeDir, "FFmpeg-BUILD-NOTICE.txt"),
    [
      "FFmpeg sidecar build notice",
      "",
      `ffmpeg version: ${ffmpeg.version}`,
      `ffprobe version: ${ffprobe.version}`,
      `Both host binaries were compiled with --enable-gpl and are declared ${ffmpeg.license.spdx} by the verified host package metadata.`,
      `The full license text is in ${ffmpeg.license.notice}; corresponding FFmpeg source and host package provenance are in FFmpeg-SOURCE.txt.`,
      "",
    ].join("\n"),
  );
  await writeFile(
    join(noticeDir, "FFmpeg-SOURCE.txt"),
    [
      "FFmpeg corresponding-source offer",
      "",
      `ffmpeg: ${ffmpeg.source.releaseUrl}`,
      `ffprobe: ${ffprobe.source.releaseUrl}`,
      `ffmpeg version: ${ffmpeg.version}`,
      `ffprobe version: ${ffprobe.version}`,
      `ffmpeg host executable: ${ffmpeg.source.executable}`,
      `ffprobe host executable: ${ffprobe.source.executable}`,
      `ffmpeg package provenance: ${JSON.stringify(ffmpeg.source.package)}`,
      `ffprobe package provenance: ${JSON.stringify(ffprobe.source.package)}`,
      `Declared package license: ${ffmpeg.license.spdx}`,
      `Exact Arch FFmpeg recipe archive (bundled): ${sources.ffmpeg.bundlePath}`,
      `Exact Arch FFmpeg recipe SHA-256: ${sources.ffmpeg.sha256}`,
      `Arch recipe commit: ${sources.ffmpeg.commit} (${sources.ffmpeg.packageVersion})`,
      `Recipe upstream input: ${sources.ffmpeg.upstream}`,
      `Recipe patch: ${sources.ffmpeg.patch}`,
      `Exact Arch x264 recipe archive (bundled): ${sources.x264.bundlePath}`,
      `Exact Arch x264 recipe SHA-256: ${sources.x264.sha256}`,
      `x264 recipe commit: ${sources.x264.commit} (${sources.x264.packageVersion})`,
      `x264 source commit: ${sources.x264.sourceCommit}`,
      "The bundled recipe archives contain PKGBUILD, .SRCINFO, patches, checksums, and license files needed to retrieve/build the exact Arch package inputs. They are the durable corresponding-source offer for these host-linked binaries; apply the recorded patch to the signed upstream source input.",
      "",
    ].join("\n"),
  );
  await writeFile(
    join(noticeDir, "FFmpeg-RUNTIME-LIBRARIES.txt"),
    [
      "FFmpeg/ffprobe/whisper-cli runtime dependency notice",
      "",
      "The packaging step copies the ELF dependency closure into binaries/lib and sets DT_RPATH to $ORIGIN/lib on the tools and $ORIGIN on each copied library.",
      "Each copied library's SHA-256 and stable package/source provenance are recorded in resources/notices/sidecar-manifest.json. Preserve the corresponding distribution/source notices for those packages.",
      `Bundled library count: ${runtime.libraries.length}`,
      "",
    ].join("\n"),
  );
  await writeFile(
    join(noticeDir, "whisper.cpp-SOURCE.txt"),
    [
      "whisper.cpp corresponding-source notice",
      "",
      `Repository: ${whisper.source.repository}`,
      `Release: ${whisper.source.release}`,
      `Commit: ${whisper.source.commit}`,
      "The source checkout used for this binary is pinned to the commit above. ggml's separate notice is included when present.",
      "",
    ].join("\n"),
  );
}

async function verifyFonts() {
  const entries = [];
  for (const [projectPath, expected] of Object.entries(interFiles)) {
    const absolute = resolve(root, projectPath);
    if (!(await exists(absolute))) fail(`required Inter font is missing: ${projectPath}`);
    const sha256 = await hashFile(absolute);
    if (sha256 !== expected) fail(`Inter font checksum mismatch for ${projectPath}: expected ${expected}, got ${sha256}`);
    entries.push({ path: projectPath, sha256 });
  }
  const nativeFiles = [];
  for (const [sourcePath, stagedPath] of Object.entries(nativeInterFiles)) {
    const source = resolve(root, sourcePath);
    const staged = resolve(root, "src-tauri", stagedPath);
    await copyAtomic(source, staged);
    const sha256 = await hashFile(staged);
    if (sha256 !== interFiles[sourcePath]) {
      fail(`staged Inter font checksum mismatch for ${stagedPath}: expected ${interFiles[sourcePath]}, got ${sha256}`);
    }
    nativeFiles.push({ sourcePath, path: stagedPath, sha256 });
  }
  const licensePath = join(noticeDir, "Inter-LICENSE.txt");
  if (!(await exists(licensePath))) fail("Inter-LICENSE.txt is missing");
  return {
    release: interRelease,
    archiveUrl: interArchiveUrl,
    archiveSha256: interArchiveSha256,
    files: entries,
    nativeFiles,
    license: { spdx: "OFL-1.1", notice: "resources/notices/Inter-LICENSE.txt" },
  };
}
async function verifySourceOffers() {
  const verified = {};
  for (const [name, offer] of Object.entries(sourceOffers)) {
    const absolute = resolve(root, offer.path);
    if (!(await exists(absolute, fsConstants.R_OK))) {
      fail(`corresponding-source offer is missing for ${name}: ${offer.path}`);
    }
    const sha256 = await hashFile(absolute);
    if (sha256 !== offer.sha256) {
      fail(`corresponding-source offer checksum mismatch for ${name}: expected ${offer.sha256}, got ${sha256}`);
    }
    const bundled = resolve(root, "src-tauri", offer.bundlePath);
    await copyAtomic(absolute, bundled);
    verified[name] = { ...offer, bundledSha256: await hashFile(bundled) };
  }
  return verified;
}

async function assertArtifacts(manifest, requireAgent) {
  const binaryLicenses = [
    manifest.node?.license,
    manifest.binaries?.ffmpeg?.license,
    manifest.binaries?.ffprobe?.license,
    manifest.binaries?.whisperCli?.license,
  ];
  const paths = [
    manifest.node?.path,
    manifest.binaries?.ffmpeg?.path,
    manifest.binaries?.ffprobe?.path,
    manifest.binaries?.whisperCli?.path,
    manifest.node?.license?.notice,
    manifest.node?.license?.sourceNotice,
    manifest.node?.license?.runtimeNotice,
    manifest.binaries?.ffmpeg?.license?.notice,
    manifest.binaries?.ffmpeg?.license?.buildNotice,
    manifest.binaries?.ffmpeg?.license?.sourceNotice,
    manifest.binaries?.ffmpeg?.license?.runtimeNotice,
    manifest.binaries?.ffprobe?.license?.notice,
    manifest.binaries?.ffprobe?.license?.buildNotice,
    manifest.binaries?.ffprobe?.license?.sourceNotice,
    manifest.binaries?.ffprobe?.license?.runtimeNotice,
    manifest.binaries?.whisperCli?.license?.notice,
    manifest.binaries?.whisperCli?.license?.sourceNotice,
    manifest.binaries?.whisperCli?.license?.runtimeNotice,
    "resources/notices/Inter-LICENSE.txt",
    ...(manifest.fonts?.nativeFiles ?? []).map((file) => file.path),
    "resources/notices/sidecar-manifest.json",
  ];
  for (const license of binaryLicenses) {
    if (!license) fail("a required sidecar license record is missing");
    paths.push(...(license.additionalNotices ?? []));
  }
  if (manifest.binaries?.ffmpeg?.license?.spdx !== manifest.binaries?.ffprobe?.license?.spdx) {
    fail(`ffmpeg and ffprobe package licenses differ: ${manifest.binaries?.ffmpeg?.license?.spdx} vs ${manifest.binaries?.ffprobe?.license?.spdx}`);
  }
  for (const path of paths) {
    if (typeof path !== "string" || path.length === 0) fail("manifest contains an empty artifact obligation");
    const absolute = resolve(resolve(root, "src-tauri"), path);
    if (!(await exists(absolute, fsConstants.R_OK))) fail(`manifest obligation is missing: ${path}`);
  }
  for (const [name, offer] of Object.entries(manifest.sources ?? {})) {
    const absolute = resolve(root, offer.path);
    if (!(await exists(absolute, fsConstants.R_OK))) fail(`corresponding-source offer is missing for ${name}: ${offer.path}`);
    const actual = await hashFile(absolute);
    if (actual !== offer.sha256) fail(`corresponding-source offer checksum mismatch for ${name}`);
    const bundled = resolve(root, "src-tauri", offer.bundlePath);
    if (!(await exists(bundled, fsConstants.R_OK))) fail(`bundled source offer is missing for ${name}: ${offer.bundlePath}`);
    const bundledHash = await hashFile(bundled);
    if (bundledHash !== offer.bundledSha256) fail(`bundled source offer checksum mismatch for ${name}`);
  }
  if (!manifest.runtime || !Array.isArray(manifest.runtime.libraries) || manifest.runtime.libraries.length === 0) {
    fail("no runtime libraries were staged; refusing a host-only sidecar package");
  }
  for (const library of manifest.runtime.libraries) {
    if (!library.provenance?.kind || !library.provenance.license) {
      fail(`runtime library provenance is incomplete: ${library.path}`);
    }
    const absolute = resolve(resolve(root, "src-tauri"), library.path);
    if (!(await exists(absolute, fsConstants.R_OK))) fail(`runtime library is missing: ${library.path}`);
    const actual = await hashFile(absolute);
    if (actual !== library.sha256) fail(`runtime library checksum mismatch: ${library.path}`);
  }
  for (const [projectPath, expected] of Object.entries(interFiles)) {
    const actual = await hashFile(resolve(root, projectPath));
    if (actual !== expected) fail(`font changed after staging: ${projectPath}`);
  }
  const nativeFiles = manifest.fonts?.nativeFiles;
  if (!Array.isArray(nativeFiles) || nativeFiles.length !== Object.keys(nativeInterFiles).length) {
    fail("staged native Inter font records are missing");
  }
  for (const file of nativeFiles) {
    if (
      !file
      || typeof file.sourcePath !== "string"
      || typeof file.path !== "string"
      || typeof file.sha256 !== "string"
      || nativeInterFiles[file.sourcePath] !== file.path
      || interFiles[file.sourcePath] !== file.sha256
    ) {
      fail("staged native Inter font mapping is invalid");
    }
    const source = resolve(root, file.sourcePath);
    const staged = resolve(root, "src-tauri", file.path);
    const sourceHash = await hashFile(source);
    if (sourceHash !== file.sha256) fail(`source Inter font checksum mismatch: ${file.sourcePath}`);
    const stagedHash = await hashFile(staged);
    if (stagedHash !== file.sha256) fail(`staged Inter font checksum mismatch: ${file.path}`);
  }
  if (requireAgent) {
    const agentPaths = [
      ["entrypoint", manifest.agent?.entrypoint],
      ["package metadata", manifest.agent?.package],
      ["importer runtime dependencies", manifest.agent?.runtimeDependencies],
      ["compiled shared package", manifest.agent?.sharedPackage],
      ["root runtime dependencies", manifest.agent?.rootRuntimeDependencies],
    ];
    for (const [label, path] of agentPaths) {
      if (typeof path !== "string" || path.length === 0 || !(await exists(resolve(root, "src-tauri", path)))) {
        fail(`agent deployment is incomplete; missing ${label}: ${path ?? "(unset)"}`);
      }
    }
  }
}
function smokeStagedTools(stagedTools) {
  const smokeEnv = { ...env, PATH: "/usr/bin:/bin" };
  delete smokeEnv.LD_LIBRARY_PATH;
  for (const [toolPath, args] of [
    [stagedTools.node, ["--version"]],
    [stagedTools.ffmpeg, ["-version"]],
    [stagedTools.ffprobe, ["-version"]],
    [stagedTools.whisperCli, ["--help"]],
  ]) {
    capture(toolPath, args, { env: smokeEnv, stdio: "pipe" });
  }
}

await mkdir(binaryDir, { recursive: true });
await mkdir(noticeDir, { recursive: true });
const previousManifestPath = resolve(root, "sidecars", "manifest.json");
const sources = await verifySourceOffers();
const nodeWithArchive = await stageNode();
const { archivePath: nodeArchivePath, ...node } = nodeWithArchive;
const ffmpeg = await stageHostTool("ffmpeg", `ffmpeg-${targetTriple}`);
const ffprobe = await stageHostTool("ffprobe", `ffprobe-${targetTriple}`);
if (ffmpeg.license.spdx !== ffprobe.license.spdx) {
  fail(`ffmpeg and ffprobe package licenses differ: ${ffmpeg.license.spdx} vs ${ffprobe.license.spdx}`);
}
const whisperCli = await stageWhisper();
const stagedTools = {
  node: resolve(root, "src-tauri", node.path),
  ffmpeg: resolve(root, "src-tauri", ffmpeg.path),
  ffprobe: resolve(root, "src-tauri", ffprobe.path),
  whisperCli: resolve(root, "src-tauri", whisperCli.path),
};
const runtime = await stageRuntimeDependencies(Object.values(stagedTools));
for (const tool of [node, ffmpeg, ffprobe, whisperCli]) {
  tool.runtime = {
    linkage: runtime.linkage,
    libraryDirectory: runtime.libraryDirectory,
    loaderStrategy: runtime.loaderStrategy,
  };
  tool.sha256 = await hashFile(resolve(root, "src-tauri", tool.path));
}
smokeStagedTools(stagedTools);
const fonts = await verifyFonts();
await writeNotices({ node, nodeArchivePath, ffmpeg, ffprobe, whisper: whisperCli, runtime, sources });
const manifest = {
  schemaVersion: 2,
  status: "ready",
  target: targetTriple,
  generatedBy: "scripts/prepare-sidecars.mjs",
  node,
  binaries: { ffmpeg, ffprobe, whisperCli },
  runtime,
  sources,
  speechModel: {
    status: "not-downloaded",
    downloadRequiresExplicitConsent: true,
    repository: "ggerganov/whisper.cpp",
    revision: "5359861c739e955e79d9a303bcbc70fb988958b1",
    file: "ggml-small.bin",
    bytes: 487601967,
    sha256: "1be3a9b2063867b937e64e2ec7483364a79917e157fa98c5d94b5c1fffea987b",
    url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1/ggml-small.bin",
    activation: "download-to-temp-verify-sha256-then-atomic-promote",
    note: "The model is intentionally not staged by packaging. The app must ask before downloading it.",
  },
  fonts,
  agent: {
    entrypoint: "resources/agent/agent/dist/main.js",
    package: "resources/agent/agent/package.json",
    deployment: "pnpm install --prod --frozen-lockfile --node-linker=hoisted",
    runtimeDependencies: "resources/agent/agent/node_modules",
    sharedPackage: "resources/agent/shared",
    rootRuntimeDependencies: "resources/agent/node_modules",
  },
  requirements: {
    noPathFallback: true,
    releaseRequiresAllBinaryNotices: true,
    ffmpegRuntime: "bundled-elf-closure-with-origin-rpath",
    runtimeNote: "The copied closure includes required host dynamic libraries with $ORIGIN RPATH; the system ELF loader baseline remains required and must be qualified on the target distribution.",
  },
};
const requireAgent = process.env.CUTTERHOOCHEE_REQUIRE_AGENT === "1";
const serializedManifest = `${JSON.stringify(manifest, null, 2)}\n`;
await writeAtomic(resolve(noticeDir, "sidecar-manifest.json"), serializedManifest);
await assertArtifacts(manifest, requireAgent);
// Keep the generated manifest deterministic: it records hashes and pinned
// provenance, never a wall-clock build time or machine secret.
await writeAtomic(previousManifestPath, serializedManifest);
process.stdout.write(
  `Prepared ${targetTriple} sidecars: Node ${node.version}, FFmpeg ${ffmpeg.version}, whisper.cpp ${whisperCli.version}; model remains consent-gated.\n`,
);
