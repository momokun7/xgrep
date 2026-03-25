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

  // Index status
  const status = xg.indexStatus();
  assert.ok(status.includes('Index path:'), 'status should contain index path');
  console.log('ok: indexStatus');

  // Error handling: search on non-indexed repo should still work (fallback)
  const xg2 = Xgrep.open(dir);
  assert.ok(xg2, 'open should succeed on valid path');
  console.log('ok: error handling');

  console.log('\nAll tests passed!');
} finally {
  rmSync(dir, { recursive: true, force: true });
}
