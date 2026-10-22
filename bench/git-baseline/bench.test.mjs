import assert from 'node:assert/strict';
import { test } from 'node:test';
import { fastImportStream, markdown, parseArgs, stats } from './lib.mjs';

test('parseArgs defaults and validation', () => {
  assert.deepEqual(parseArgs([]).sizes, ['small', 'medium']);
  assert.equal(parseArgs(['--runs', '3', '--sizes', 'small,small']).sizes.length, 1);
  assert.throws(() => parseArgs(['--sizes', 'huge']));
  assert.throws(() => parseArgs(['--runs', '0']));
  assert.throws(() => parseArgs(['--runs', '1000']));
  assert.throws(() => parseArgs(['--runs', '-1']));
  assert.throws(() => parseArgs(['--bogus']));
  assert.throws(() => parseArgs(['--out']));
});

test('stats', () => {
  const s = stats([3, 1, 2]);
  assert.equal(s.median, 2);
  assert.equal(s.min, 1);
  assert.equal(s.max, 3);
  assert.equal(stats([5]).stddev, 0);
});

test('fast-import stream is deterministic and ends with done', () => {
  const a = fastImportStream(20, 3);
  assert.equal(a, fastImportStream(20, 3));
  assert.ok(a.endsWith('done\n'));
});

test('markdown has empty asc column', () => {
  const doc = { meta: { git: 'git x', runs: 3, sizes: ['small'] }, results: [{ size: 'small', op: 'status', median: 4.2 }] };
  const md = markdown(doc);
  assert.match(md, /\| status \| 4\.2 \|  \|/);
  assert.match(md, /\| log \| n\/a \|  \|/);
});
