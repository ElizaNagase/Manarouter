// Tests for panel/gpu-setup-model.js — cerebriumBundlePayload, run under bare `node --test`.
//
// Pure data: no DOM, no network, no `document`. Every assertion is about the shape of the
// `POST /api/gpu/lanes/cerebrium/bundle` body the helper builds from a draft.
//
// The helper may land late: if `cerebriumBundlePayload` is not exported when this file runs,
// `node --test` reports a failed import rather than a silent pass, which is the defensive outcome.

import test from 'node:test';
import assert from 'node:assert/strict';

import * as model from '../gpu-setup-model.js';

const {
  CEREBRIUM_COMPUTE_IDS,
  cerebriumBundlePayload,
  emptyDraft,
} = model;

// ---------------------------------------------------------------------------
// required keys and defaults (empty draft)
// ---------------------------------------------------------------------------

test('an empty cerebrium draft includes required strings and defaults, and omits the optionals', () => {
  const d = emptyDraft('cerebrium');
  const body = cerebriumBundlePayload(d);
  // Required strings are always present, even when empty.
  assert.equal(body.app, '');
  assert.equal(body.project, '');
  assert.equal(body.repo, '');
  assert.equal(body.revision, '');
  assert.equal(body.compute, '');
  // Numeric defaults from emptyDraft: hostCpu 8, hostMemoryGb 64, gpuCount 1.
  assert.equal(body.gpu_count, 1);
  assert.equal(body.cpu, 8);
  assert.equal(body.memory_gb, 64);
  // kv_cache_dtype defaults to 'auto' and shutdownCooldownSecs to 60 in emptyDraft,
  // so both are present as defaults (cooldown_secs is stringified by the helper).
  assert.equal(body.kv_cache_dtype, 'auto');
  assert.equal(body.cooldown_secs, '60');
  // The other optionals carry no value in an empty draft and are omitted.
  for (const key of [
    'max_model_len', 'max_num_seqs', 'weight_quant',
    'max_replicas', 'vllm_pin',
  ]) {
    assert.equal(key in body, false, `${key} must be omitted on an empty draft`);
  }
  assert.deepEqual(Object.keys(body).sort(), [
    'app', 'compute', 'cooldown_secs', 'cpu', 'gpu_count', 'kv_cache_dtype', 'memory_gb', 'project', 'repo', 'revision',
  ]);
});

test('gpu_count defaults to 1, never below 1, and passes numeric and string input through', () => {
  const d = emptyDraft('cerebrium');
  assert.equal(cerebriumBundlePayload(d).gpu_count, 1);
  d.gpuCount = 4;
  assert.equal(cerebriumBundlePayload(d).gpu_count, 4);
  d.gpuCount = 0;
  assert.equal(cerebriumBundlePayload(d).gpu_count, 1);
  d.gpuCount = '2';
  assert.equal(cerebriumBundlePayload(d).gpu_count, 2);
  d.gpuCount = null;
  assert.equal(cerebriumBundlePayload(d).gpu_count, 1);
});

// ---------------------------------------------------------------------------
// repo-derived app name
// ---------------------------------------------------------------------------

test('the app name is derived from the repo slug when cerebriumApp is empty', () => {
  const d = emptyDraft('cerebrium');
  d.repo = 'my-org/My Model';
  assert.equal(cerebriumBundlePayload(d).app, 'my-org-my-model');
});

test('the app name falls back to slugify of a bare repo and stays the repo slug', () => {
  const d = emptyDraft('cerebrium');
  d.repo = 'Qwen/Qwen3.8-27B-Uncensored-FP8';
  assert.equal(cerebriumBundlePayload(d).app, 'qwen-qwen3-8-27b-uncensored-fp8');
});

test('an explicit cerebriumApp wins over the repo-derived name', () => {
  const d = emptyDraft('cerebrium');
  d.cerebriumApp = 'my-app';
  d.repo = 'some-org/other-repo';
  assert.equal(cerebriumBundlePayload(d).app, 'my-app');
  // An explicit empty app still derives from the repo (text('') is falsy).
  d.cerebriumApp = '';
  assert.equal(cerebriumBundlePayload(d).app, 'some-org-other-repo');
});

// ---------------------------------------------------------------------------
// trimming
// ---------------------------------------------------------------------------

test('optional fields are trimmed, and blank-or-whitespace optionals are omitted', () => {
  const d = emptyDraft('cerebrium');
  d.maxModelLen = '  262144  ';
  d.maxNumSeqs = '  54  ';
  d.kvCacheDtype = '  fp8  ';
  d.weightQuant = '  fp8  ';
  d.shutdownCooldownSecs = '  60  ';
  d.maxReplicas = '  3  ';
  d.vllmPin = '  0.24.0  ';
  const body = cerebriumBundlePayload(d);
  assert.equal(body.max_model_len, '262144');
  assert.equal(body.max_num_seqs, '54');
  assert.equal(body.kv_cache_dtype, 'fp8');
  assert.equal(body.weight_quant, 'fp8');
  assert.equal(body.cooldown_secs, '60');
  assert.equal(body.max_replicas, '3');
  assert.equal(body.vllm_pin, '0.24.0');
});

test('blank optionals are omitted even when present on the draft', () => {
  const d = emptyDraft('cerebrium');
  d.maxModelLen = '   ';
  d.maxNumSeqs = '';
  d.weightQuant = undefined;
  d.shutdownCooldownSecs = '\t';
  d.maxReplicas = '';
  d.vllmPin = null;
  // kv_cache_dtype always carries emptyDraft's 'auto' default, so it stays; the rest are blank.
  const body = cerebriumBundlePayload(d);
  assert.equal(body.kv_cache_dtype, 'auto');
  for (const key of ['max_model_len', 'max_num_seqs', 'weight_quant', 'cooldown_secs', 'max_replicas', 'vllm_pin']) {
    assert.equal(key in body, false, `${key} must be omitted when blank`);
  }
});

// ---------------------------------------------------------------------------
// compute id passthrough for all 13 ids
// ---------------------------------------------------------------------------

test('every Cerebrium compute id passes through unchanged as compute', () => {
  assert.equal(CEREBRIUM_COMPUTE_IDS.length, 13, 'expected 13 Cerebrium compute ids');
  for (const compute of CEREBRIUM_COMPUTE_IDS) {
    const d = emptyDraft('cerebrium');
    d.cerebriumCompute = compute;
    assert.equal(cerebriumBundlePayload(d).compute, compute, `compute ${compute} must pass through`);
  }
});

test('an empty compute is still included as an empty string (required field)', () => {
  const d = emptyDraft('cerebrium');
  d.cerebriumCompute = '';
  assert.equal(cerebriumBundlePayload(d).compute, '');
});

// ---------------------------------------------------------------------------
// cpu / memory_gb numeric passthrough
// ---------------------------------------------------------------------------

test('cpu and memory_gb pass through numerically with documented defaults', () => {
  const d = emptyDraft('cerebrium');
  d.hostCpu = 4;
  d.hostMemoryGb = 32;
  const body = cerebriumBundlePayload(d);
  assert.equal(body.cpu, 4);
  assert.equal(body.memory_gb, 32);
});

test('cpu and memory_gb fall back to 8 and 64 when unset', () => {
  const d = emptyDraft('cerebrium');
  delete d.hostCpu;
  delete d.hostMemoryGb;
  const body = cerebriumBundlePayload(d);
  assert.equal(body.cpu, 8);
  assert.equal(body.memory_gb, 64);
});

// ---------------------------------------------------------------------------
// no token secrets
// ---------------------------------------------------------------------------

test('the payload never carries account credentials or token secrets', () => {
  const d = emptyDraft('cerebrium');
  d.accountId = 'mana-modal';
  d.tokenId = 'wk-secret';
  d.tokenSecret = 'ws-secret';
  d.hfToken = 'hf-secret';
  const body = cerebriumBundlePayload(d);
  for (const key of ['accountId', 'tokenId', 'tokenSecret', 'hfToken', 'account', 'token', 'secret', 'credentials']) {
    assert.equal(key in body, false, `${key} must not be in the payload`);
  }
  const flat = JSON.stringify(body);
  assert.equal(flat.includes('wk-secret'), false);
  assert.equal(flat.includes('ws-secret'), false);
  assert.equal(flat.includes('hf-secret'), false);
  // The deployment-relevant fields are still present.
  assert.equal(body.repo, '');
  assert.equal(body.gpu_count, 1);
});

// ---------------------------------------------------------------------------
// defensive: no mutation of the input draft
// ---------------------------------------------------------------------------

test('cerebriumBundlePayload does not mutate the draft it receives', () => {
  const d = emptyDraft('cerebrium');
  d.repo = 'org/repo';
  d.hostCpu = 4;
  const before = JSON.stringify(d);
  cerebriumBundlePayload(d);
  assert.equal(JSON.stringify(d), before, 'the draft must be unchanged after building the payload');
});
