import * as esbuild from 'esbuild';
const mods = ['fs','path','vm','http','https','stream','crypto','zlib','url',
              'child_process','net','tls','os','perf_hooks','buffer','util',
              'events','string_decoder','querystring','worker_threads','timers','assert','punycode'];
const alias = {};
for (const m of mods) { alias[m] = `./stubs/${m}.js`; alias[`node:${m}`] = `./stubs/${m}.js`; }
alias['stream/web'] = './stubs/stream_web.js';
alias['node:stream/web'] = './stubs/stream_web.js';
// esbuild rejects a trailing slash in `alias`, and tr46 imports "punycode/".
const stubPunycode = {
  name: 'stub-punycode',
  setup(build) {
    build.onResolve({ filter: /^punycode\/?$/ }, () => ({
      path: new URL('./stubs/punycode.js', import.meta.url).pathname,
    }));
  },
};

const common = {
  bundle: true, format: 'iife', platform: 'neutral', target: 'es2020', alias,
  mainFields: ['module','main'], conditions: ['import','default'],
  resolveExtensions: ['.js','.mjs','.json'], minify: true,
  legalComments: 'none', logLevel: 'warning', plugins: [stubPunycode],
};

await esbuild.build({
  ...common,
  entryPoints: ['encoding-entry.js'],
  outfile: '../src/js/vendor/encoding.js',
});

await esbuild.build({
  ...common,
  entryPoints: ['platform-entry.js'],
  outfile: '../src/js/vendor/platform.js',
});

await esbuild.build({
  ...common,
  entryPoints: ['fetch-entry.js'],
  outfile: '../src/js/vendor/fetch.js',
});
console.log('BUILD OK');
