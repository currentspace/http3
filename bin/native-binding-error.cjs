'use strict';

const manifest = require('../package.json');

// Called only after the generated loader has exhausted local and package bindings.
module.exports = function nativeBindingError({ platform, arch, musl, override, loadErrors, cause }) {
  const target = `${platform}-${arch}${platform === 'linux' ? (musl ? '-musl' : '-gnu') : ''}`;
  const platformLabel = `${platform} ${arch}${platform === 'linux' ? (musl ? ' (musl)' : ' (glibc)') : ''}`;
  const packageName = `${manifest.name}-${target}`;
  const prebuilds = Object.keys(manifest.optionalDependencies).filter(name => name.startsWith(`${manifest.name}-`));
  const supported = prebuilds.map(name => name.slice(manifest.name.length + 1)).join(', ');
  let message;

  if (override) {
    message = `Cannot load the native binding for ${platformLabel} from NAPI_RS_NATIVE_LIBRARY_PATH=${override}. ` +
      'Check that the configured file exists and is compatible with this platform. See cause for the loader errors.';
  } else if (!prebuilds.includes(packageName)) {
    message = `Cannot load the native binding for ${platformLabel}: no prebuilt native package is published for this platform. ` +
      `Published targets: ${supported}. Reinstalling dependencies will not add support. ` +
      "Use runtimeMode: 'wasm' explicitly, or a supported source build with NAPI_RS_NATIVE_LIBRARY_PATH pointing to its .node file. " +
      'See docs/SUPPORT_MATRIX.md in the repository for source-build qualification and WASM limitations.';
  } else {
    let packageMissing = false;
    try {
      require.resolve(packageName);
    } catch (error) {
      packageMissing = error.code === 'MODULE_NOT_FOUND' &&
        error.message.split('\n')[0] === `Cannot find module '${packageName}'`;
    }
    // A broken local binary or a missing dependency inside a binding is not a
    // missing sidecar. Keep its failure visible instead of prescribing reinstall.
    const bindingFailed = loadErrors.some(error => {
      if (error.code !== 'MODULE_NOT_FOUND') return true;
      const missing = /^Cannot find module '([^']+)'/u.exec(error.message)?.[1];
      return !missing || !(/^(?:\.\/http3\.[\w.-]+|@currentspace\/http3-[\w-]+)$/u.test(missing));
    });
    if (packageMissing && !bindingFailed) {
      message = `Cannot load the native binding for ${platformLabel}: the optional platform package ${packageName}@${manifest.version} is missing. ` +
        'Reinstall with optional dependencies enabled (npm install --include=optional), or install that exact package version explicitly. ' +
        'Keep the root and platform package versions aligned.';
    } else {
      message = `Cannot load the native binding for ${platformLabel}. Expected platform package: ${packageName}@${manifest.version}. ` +
        'A binding was found but failed to load or resolve; see cause for the original loader errors.';
    }
  }

  const error = new Error(message, { cause });
  error.code = 'ERR_HTTP3_RUNTIME_UNSUPPORTED';
  return error;
};
