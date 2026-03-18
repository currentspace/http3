import { existsSync, mkdirSync, writeFileSync } from 'node:fs';
import os from 'node:os';
import { relative, resolve } from 'node:path';

export function sanitizeArtifactFragment(value) {
  return String(value)
    .trim()
    .replace(/[^a-zA-Z0-9._-]+/gu, '-')
    .replace(/^-+|-+$/gu, '')
    .toLowerCase() || 'run';
}

export function createArtifactStamp(date = new Date()) {
  return date.toISOString().replace(/[:]/gu, '-').replace(/\.\d{3}Z$/u, 'Z');
}

export function resolveResultsDir(rootDir, resultsDir) {
  if (!resultsDir) {
    return null;
  }
  return resolve(rootDir, resultsDir);
}

export function relativeToRoot(rootDir, absolutePath) {
  if (!absolutePath) {
    return null;
  }
  return relative(rootDir, absolutePath);
}

export function captureEnvironmentMetadata({
  runner,
  protocol,
  target,
  label = null,
  extra = {},
} = {}) {
  return {
    collectedAt: new Date().toISOString(),
    runner: runner ?? null,
    protocol: protocol ?? null,
    target: target ?? null,
    label,
    host: {
      platform: process.platform,
      release: os.release(),
      arch: process.arch,
      hostname: os.hostname(),
      cpuCount: os.cpus().length,
      totalMemoryBytes: os.totalmem(),
    },
    node: {
      version: process.version,
      execPath: process.execPath,
      pid: process.pid,
    },
    ci: Boolean(process.env.CI),
    container: {
      insideContainer: existsSync('/.dockerenv') || Boolean(process.env.container),
      dockerRuntimePlatform: process.env.DOCKER_RUNTIME_PLATFORM ?? null,
    },
    ...extra,
  };
}

export function writeJsonArtifact({
  rootDir,
  resultsDir,
  prefix,
  label = null,
  payload,
}) {
  const absoluteResultsDir = resolveResultsDir(rootDir, resultsDir);
  if (!absoluteResultsDir) {
    return null;
  }
  mkdirSync(absoluteResultsDir, { recursive: true });
  const filename = [
    createArtifactStamp(),
    sanitizeArtifactFragment(prefix),
    label ? sanitizeArtifactFragment(label) : null,
  ].filter(Boolean).join('-') + '.json';
  const absolutePath = resolve(absoluteResultsDir, filename);
  writeFileSync(absolutePath, `${JSON.stringify(payload, null, 2)}\n`);
  return {
    absolutePath,
    relativePath: relativeToRoot(rootDir, absolutePath),
  };
}

export function writeTextArtifact({
  rootDir,
  resultsDir,
  prefix,
  label = null,
  extension = 'txt',
  content,
}) {
  const absoluteResultsDir = resolveResultsDir(rootDir, resultsDir);
  if (!absoluteResultsDir) {
    return null;
  }
  mkdirSync(absoluteResultsDir, { recursive: true });
  const filename = [
    createArtifactStamp(),
    sanitizeArtifactFragment(prefix),
    label ? sanitizeArtifactFragment(label) : null,
  ].filter(Boolean).join('-') + `.${extension}`;
  const absolutePath = resolve(absoluteResultsDir, filename);
  writeFileSync(absolutePath, content);
  return {
    absolutePath,
    relativePath: relativeToRoot(rootDir, absolutePath),
  };
}
