// npm 11 reports an array; npm 12 keys results by package name. Each release
// call packs one directory, so ambiguous or incomplete results must fail.
export function parseNpmPackMetadata(output, packageName) {
  const report = JSON.parse(output);
  const metadata = Array.isArray(report)
    ? (report.length === 1 ? report[0] : undefined)
    : (report?.[packageName] ?? report);

  if (metadata?.name !== packageName
    || !Array.isArray(metadata.files)
    || metadata.files.length === 0
    || metadata.files.some((file) => typeof file?.path !== 'string' || file.path.length === 0)) {
    throw new Error(`Invalid npm pack metadata for ${packageName}: expected one package with a nonempty file list.`);
  }
  return metadata;
}
