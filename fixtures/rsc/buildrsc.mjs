import * as esbuild from 'esbuild';
await esbuild.build({
  entryPoints: ['entry-rsc.jsx'], bundle: true, format: 'esm', target: 'es2020',
  outfile: 'out/rsc.js', minify: true, logLevel: 'warning',
  define: { 'process.env.NODE_ENV': '"production"' },
  conditions: ['browser'],
});
console.log('rsc bundle built');
