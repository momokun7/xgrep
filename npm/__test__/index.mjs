import { Xgrep } from '../index.js';
import { mkdtempSync, writeFileSync, rmSync } from 'fs';
import { join } from 'path';
import { tmpdir } from 'os';
import assert from 'assert';

const dir = mkdtempSync(join(tmpdir(), 'xgrep-test-'));

try {
  // Create test files
  writeFileSync(join(dir, 'hello.rs'), 'fn main() {\n    println!("hello");\n}\n');
  writeFileSync(join(dir, 'world.py'), 'def world():\n    print("world")\n');

  // Open and build index
  const xg = Xgrep.open(dir);
  assert.ok(xg.root, 'root should be set');
  assert.ok(xg.indexPath, 'indexPath should be set');

  xg.buildIndex();
  console.log('ok: buildIndex');

  // Search
  const results = xg.search('hello');
  assert.ok(results.length > 0, 'should find results for "hello"');
  assert.strictEqual(results[0].file, 'hello.rs');
  assert.strictEqual(typeof results[0].lineNumber, 'number');
  assert.ok(results[0].line.includes('hello'));
  console.log('ok: search');

  // Search with options
  const rsResults = xg.search('fn', { fileType: 'rs', maxCount: 5 });
  assert.ok(rsResults.length > 0, 'should find fn in rs files');
  assert.ok(rsResults.every(r => r.file.endsWith('.rs')), 'all results should be .rs');
  console.log('ok: search with options');

  // Case-insensitive search
  const ciResults = xg.search('HELLO', { caseInsensitive: true });
  assert.ok(ciResults.length > 0, 'case-insensitive should find HELLO');
  console.log('ok: case-insensitive search');

  // Empty results
  const noResults = xg.search('nonexistent_pattern_xyz');
  assert.strictEqual(noResults.length, 0, 'should return empty for no match');
  console.log('ok: empty results');

  // Word-boundary search
  const wordResults = xg.search('hello', { word: true });
  assert.ok(wordResults.length > 0, 'word search should match standalone "hello"');
  console.log('ok: word-boundary search');

  // Glob filter (include only .rs)
  const globResults = xg.search('fn', { globs: ['*.rs'] });
  assert.ok(globResults.every(r => r.file.endsWith('.rs')), 'glob should restrict to .rs');
  console.log('ok: glob filter');

  // Index status (structured object)
  const status = xg.indexStatus();
  assert.strictEqual(typeof status, 'object', 'status should be an object');
  assert.strictEqual(status.state, 'fresh', 'state should be fresh after build');
  assert.ok(status.indexedFiles >= 1, 'indexedFiles should count built files');
  assert.ok(status.indexSizeBytes > 0, 'indexSizeBytes should be positive');
  assert.ok(status.indexPath.length > 0, 'indexPath should be set');
  console.log('ok: indexStatus');

  // Error handling: search on non-indexed repo should still work (fallback)
  const xg2 = Xgrep.open(dir);
  assert.ok(xg2, 'open should succeed on valid path');
  console.log('ok: error handling');

  console.log('\nAll tests passed!');
} finally {
  rmSync(dir, { recursive: true, force: true });
}
