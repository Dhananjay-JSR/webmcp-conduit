import * as esbuild from 'esbuild';
const common = { bundle: true, format: 'iife', target: 'es2020', minify: true,
                 logLevel: 'warning', define: { 'process.env.NODE_ENV': '"production"' } };
await esbuild.build({ ...common, entryPoints: ['entry-document.jsx'], outfile: 'out/document.js' });
await esbuild.build({ ...common, entryPoints: ['entry-div.jsx'], outfile: 'out/div.js' });
console.log('bundles built');
