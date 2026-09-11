#!/usr/bin/env node

import {
  existsSync,
  readFileSync,
  renameSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const VERSION_FILE = join(ROOT, "VERSION");

const SEMVER_IDENTIFIER =
  "(?:0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*)";
const BUILD_IDENTIFIER = "[0-9A-Za-z-]+";
const SEMVER_TEXT =
  `(?:0|[1-9][0-9]*)\\.(?:0|[1-9][0-9]*)\\.(?:0|[1-9][0-9]*)` +
  `(?:-${SEMVER_IDENTIFIER}(?:\\.${SEMVER_IDENTIFIER})*)?` +
  `(?:\\+${BUILD_IDENTIFIER}(?:\\.${BUILD_IDENTIFIER})*)?`;
const SEMVER = new RegExp(`^${SEMVER_TEXT}$`);

const PACKAGE_FILES = [
  ["package.json", "cutterhoochee-workspace"],
  ["agent/package.json", "@cutterhoochee/agent"],
  ["shared/package.json", "@cutterhoochee/shared"],
  ["src/package.json", "@cutterhoochee/app"],
];

function fail(message) {
  throw new Error(message);
}

function readText(path) {
  try {
    return readFileSync(path, "utf8");
  } catch (error) {
    fail(`cannot read ${relative(ROOT, path)}: ${error.message}`);
  }
}

function readVersion() {
  const contents = readText(VERSION_FILE);
  const version = contents.trim();
  if (contents !== version && contents !== `${version}\n`) {
    fail("VERSION must contain exactly one unprefixed semantic version and an optional trailing newline");
  }
  if (!SEMVER.test(version)) {
    fail("VERSION must contain a valid semantic version");
  }
  return version;
}

function replaceExactly(text, pattern, replacement, description) {
  const matches = [...text.matchAll(pattern)];
  if (matches.length !== 1) {
    fail(`${description} must occur exactly once (found ${matches.length})`);
  }
  const match = matches[0];
  return text.slice(0, match.index) + replacement(match) + text.slice(match.index + match[0].length);
}

function synchronizePackage(path, expectedName, version) {
  const text = readText(path);
  let parsed;
  try {
    parsed = JSON.parse(text);
  } catch (error) {
    fail(`${relative(ROOT, path)} is not valid JSON: ${error.message}`);
  }
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
    fail(`${relative(ROOT, path)} must contain a JSON object`);
  }
  if (parsed.name !== expectedName) {
    fail(`${relative(ROOT, path)} must identify package ${JSON.stringify(expectedName)}`);
  }
  if (typeof parsed.version !== "string") {
    fail(`${relative(ROOT, path)} must contain a top-level string version`);
  }

  return replaceExactly(
    text,
    /^([ ]{2}"version"\s*:\s*")[^"\n]*("[ \t]*,?[ \t]*)$/gm,
    (match) => `${match[1]}${version}${match[2]}`,
    `${relative(ROOT, path)} top-level version field`,
  );
}

function synchronizeTauriConfig(path, version) {
  const text = readText(path);
  let parsed;
  try {
    parsed = JSON.parse(text);
  } catch (error) {
    fail(`${relative(ROOT, path)} is not valid JSON: ${error.message}`);
  }
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
    fail(`${relative(ROOT, path)} must contain a JSON object`);
  }
  if (typeof parsed.version !== "string") {
    fail(`${relative(ROOT, path)} must contain a top-level string version`);
  }

  return replaceExactly(
    text,
    /^([ ]{2}"version"\s*:\s*")[^"\n]*("[ \t]*,?[ \t]*)$/gm,
    (match) => `${match[1]}${version}${match[2]}`,
    `${relative(ROOT, path)} top-level version field`,
  );
}

function synchronizeCargoManifest(path, version) {
  const text = readText(path);
  const packageHeaders = [...text.matchAll(/^\[package\][ \t]*$/gm)];
  if (packageHeaders.length !== 1) {
    fail(`${relative(ROOT, path)} must contain exactly one [package] section (found ${packageHeaders.length})`);
  }
  const packageStart = packageHeaders[0].index;
  const afterHeader = packageStart + packageHeaders[0][0].length;
  const nextSection = /^[ \t]*\[[^\]]+\][ \t]*$/gm;
  nextSection.lastIndex = afterHeader;
  const nextMatch = nextSection.exec(text);
  const packageEnd = nextMatch?.index ?? text.length;
  const section = text.slice(packageStart, packageEnd);
  const nameMatches = [...section.matchAll(/^name\s*=\s*"([^"]+)"[ \t]*$/gm)];
  if (nameMatches.length !== 1 || nameMatches[0][1] !== "cutterhoochee") {
    fail(`${relative(ROOT, path)} [package] must identify cutterhoochee exactly once`);
  }
  const versionMatches = [...section.matchAll(/^(version\s*=\s*")[^"\n]*("[ \t]*)$/gm)];
  if (versionMatches.length !== 1) {
    fail(`${relative(ROOT, path)} [package] version field must occur exactly once (found ${versionMatches.length})`);
  }
  const match = versionMatches[0];
  const absoluteStart = packageStart + match.index;
  const absoluteEnd = absoluteStart + match[0].length;
  return text.slice(0, absoluteStart) +
    `${match[1]}${version}${match[2]}` +
    text.slice(absoluteEnd);
}

function synchronizeCargoLock(path, version) {
  const text = readText(path);
  const packageHeaders = [...text.matchAll(/^\[\[package\]\][ \t]*$/gm)];
  const matchingBlocks = [];
  for (let index = 0; index < packageHeaders.length; index += 1) {
    const start = packageHeaders[index].index;
    const end = packageHeaders[index + 1]?.index ?? text.length;
    const block = text.slice(start, end);
    const names = [...block.matchAll(/^name\s*=\s*"([^"]+)"[ \t]*$/gm)];
    if (names.length === 1 && names[0][1] === "cutterhoochee") {
      matchingBlocks.push({ start, end, block });
    }
  }
  if (matchingBlocks.length !== 1) {
    fail(`${relative(ROOT, path)} must contain exactly one cutterhoochee package entry (found ${matchingBlocks.length})`);
  }

  const { start, end, block } = matchingBlocks[0];
  const versionMatches = [...block.matchAll(/^version\s*=\s*"([^"]+)"[ \t]*$/gm)];
  if (versionMatches.length !== 1) {
    fail(`${relative(ROOT, path)} cutterhoochee package version must occur exactly once (found ${versionMatches.length})`);
  }
  const match = versionMatches[0];
  const updatedBlock = block.slice(0, match.index) +
    `version = "${version}"` +
    block.slice(match.index + match[0].length);
  return text.slice(0, start) + updatedBlock + text.slice(end);
}

function writeAtomically(path, contents) {
  const temporary = `${path}.${process.pid}.tmp`;
  try {
    writeFileSync(temporary, contents, "utf8");
    renameSync(temporary, path);
  } catch (error) {
    if (existsSync(temporary)) {
      unlinkSync(temporary);
    }
    fail(`cannot update ${relative(ROOT, path)}: ${error.message}`);
  }
}

function main() {
  const args = process.argv.slice(2);
  if (args.length > 1 || (args.length === 1 && args[0] !== "--check")) {
    fail("usage: node scripts/release/sync-version.mjs [--check]");
  }
  const checkOnly = args[0] === "--check";
  const version = readVersion();
  const targets = [
    ...PACKAGE_FILES.map(([path, name]) => ({
      path: join(ROOT, path),
      synchronize: (file) => synchronizePackage(file, name, version),
    })),
    {
      path: join(ROOT, "src-tauri/Cargo.toml"),
      synchronize: (file) => synchronizeCargoManifest(file, version),
    },
    {
      path: join(ROOT, "src-tauri/Cargo.lock"),
      synchronize: (file) => synchronizeCargoLock(file, version),
    },
    {
      path: join(ROOT, "src-tauri/tauri.conf.json"),
      synchronize: (file) => synchronizeTauriConfig(file, version),
    },
  ];

  const updates = [];
  for (const target of targets) {
    const current = readText(target.path);
    const updated = target.synchronize(target.path);
    if (current !== updated) {
      updates.push({ path: target.path, updated });
    }
  }

  if (checkOnly && updates.length > 0) {
    const drifted = updates.map(({ path }) => relative(ROOT, path)).join(", ");
    fail(`release version drift: ${drifted}`);
  }
  if (!checkOnly) {
    for (const { path, updated } of updates) {
      writeAtomically(path, updated);
    }
  }
  if (updates.length > 0) {
    console.log(`${checkOnly ? "Detected" : "Synchronized"} release version ${version}: ${updates.map(({ path }) => relative(ROOT, path)).join(", ")}`);
  }
}

try {
  main();
} catch (error) {
  console.error(`sync-version: ${error.message}`);
  process.exitCode = 1;
}
