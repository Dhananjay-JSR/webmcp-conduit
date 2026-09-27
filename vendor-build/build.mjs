import * as esbuild from 'esbuild';
import { copyFileSync } from 'node:fs';

// happy-dom-without-node is the Node-free build. It still references a few
// Node builtins on paths a WebMCP harvest never reaches; QuickJS tolerates
// the references as long as they are not called, and host-pre.js supplies the
// web globals (URL, TextEncoder, timers, performance) that it does need.
await esbuild.build({
  entryPoints: ['entry.js'],
  bundle: true,
  format: 'iife',
  platform: 'node',
  target: 'es2020',
  outfile: '../src/js/vendor/happy-dom.js',
  minify: true,
  legalComments: 'none',
  logLevel: 'warning',
});

copyFileSync(
  'node_modules/happy-dom-without-node/LICENSE',
  '../src/js/vendor/happy-dom.LICENSE'
);

console.log('wrote src/js/vendor/happy-dom.js');
