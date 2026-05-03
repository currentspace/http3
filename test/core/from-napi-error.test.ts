/**
 * Audit finding #30. The Rust napi error path now stamps a structured
 * tag onto the message; `fromNapiError` should recover the category +
 * metadata into a typed `Http3Error`.
 */

import { describe, it } from 'node:test';
import assert from 'node:assert';
import { fromNapiError } from '../../lib/error-map.js';
import {
  ERR_HTTP3_FAST_PATH_UNAVAILABLE,
  ERR_HTTP3_RUNTIME_UNSUPPORTED,
  ERR_HTTP3_INVALID_STATE,
  ERR_HTTP3_SESSION_ERROR,
  ERR_HTTP3_STREAM_ERROR,
} from '../../lib/errors.js';

describe('fromNapiError', () => {
  it('returns null for non-Error inputs', () => {
    assert.equal(fromNapiError('plain string'), null);
    assert.equal(fromNapiError(null), null);
    assert.equal(fromNapiError(undefined), null);
    assert.equal(fromNapiError({ message: '[h3:io] not an Error' }), null);
  });

  it('returns null for untagged Error messages', () => {
    assert.equal(fromNapiError(new Error('plain napi error message')), null);
  });

  it('parses fast-path-unavailable with driver/syscall/errno', () => {
    const tagged = new Error('[h3:fast-path|driver=io_uring|syscall=sendmsg|errno=22] ERR_HTTP3_FAST_PATH_UNAVAILABLE driver=io_uring syscall=sendmsg errno=22 reason_code=fast-path-unavailable: invalid argument');
    const err = fromNapiError(tagged);
    assert.ok(err);
    assert.equal(err.code, ERR_HTTP3_FAST_PATH_UNAVAILABLE);
    assert.equal(err.driver, 'io_uring');
    assert.equal(err.syscall, 'sendmsg');
    assert.equal(err.errno, 22);
    assert.equal(err.reasonCode, 'fast-path-unavailable');
  });

  it('parses runtime-io with reason code', () => {
    const tagged = new Error('[h3:runtime-io|driver=kqueue|syscall=kevent|reason=bad-fd|errno=9] ERR_HTTP3_RUNTIME_UNSUPPORTED ...');
    const err = fromNapiError(tagged);
    assert.ok(err);
    assert.equal(err.code, ERR_HTTP3_RUNTIME_UNSUPPORTED);
    assert.equal(err.driver, 'kqueue');
    assert.equal(err.reasonCode, 'bad-fd');
    assert.equal(err.errno, 9);
  });

  it('parses invalid-state', () => {
    const tagged = new Error('[h3:invalid-state] invalid state: oops');
    const err = fromNapiError(tagged);
    assert.ok(err);
    assert.equal(err.code, ERR_HTTP3_INVALID_STATE);
    assert.equal(err.message, 'invalid state: oops');
  });

  it('parses config', () => {
    const tagged = new Error('[h3:config] config error: missing key');
    const err = fromNapiError(tagged);
    assert.ok(err);
    assert.equal(err.code, ERR_HTTP3_INVALID_STATE);
    assert.match(err.message, /config error/);
  });

  it('parses connection-not-found', () => {
    const tagged = new Error('[h3:not-found|handle=42] connection not found: handle=42');
    const err = fromNapiError(tagged);
    assert.ok(err);
    assert.equal(err.code, ERR_HTTP3_INVALID_STATE);
  });

  it('parses quic + h3 categories', () => {
    const quicErr = fromNapiError(new Error('[h3:quic] QUIC error: Done'));
    assert.ok(quicErr);
    assert.equal(quicErr.code, ERR_HTTP3_SESSION_ERROR);

    const h3Err = fromNapiError(new Error('[h3:h3] HTTP/3 error: Done'));
    assert.ok(h3Err);
    assert.equal(h3Err.code, ERR_HTTP3_STREAM_ERROR);
  });

  it('parses io with errno', () => {
    const err = fromNapiError(new Error('[h3:io|errno=13] IO error: permission denied'));
    assert.ok(err);
    assert.equal(err.errno, 13);
  });
});
