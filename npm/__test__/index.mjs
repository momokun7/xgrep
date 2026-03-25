import { Xgrep } from '../index.js';
import { mkdtempSync, writeFileSync, rmSync } from 'fs';
import { join } from 'path';
import { tmpdir } from 'os';
import { execSync } from 'child_process';

const dir = mkdtempSync(join(tmpdir(), 'xgrep-test-'));

try {
  // Create test files
  writeFileSync(join(dir, 'hello.rs'), 'fn main() {\n    println!("hello");\n}\n');
  writeFileSync(join(dir, 'world.py'), 'def world():\n    print("world")\n');

  // Open and build index
  const xg = Xgrep.open(dir);
  console.log('root:', xg.root);
  console.log('indexPath:', xg.indexPath);

  xg.buildIndex();
  console.log('index built');

  // Search
  const results = xg.search('hello');
  console.log(`found ${results.length} results`);
  for (const r of results) {
    console.log(`  ${r.file}:${r.lineNumber}: ${r.line}`);
  }

  // Search with options
  const rsResults = xg.search('fn', { fileType: 'rs', maxCount: 5 });
  console.log(`rs-only: ${rsResults.length} results`);

  // Index status
  const status = xg.indexStatus();
  console.log('status:', status.split('\n')[0]);

  console.log('\nAll tests passed!');
} finally {
  rmSync(dir, { recursive: true, force: true });
}
