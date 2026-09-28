import React from 'react';
import { renderToString } from 'react-dom/server';
import { writeFileSync } from 'node:fs';
import App from './App.jsx';

// Server HTML must match what the client will render, or hydration mismatches.
const appHtml = renderToString(React.createElement(App));

writeFileSync('out/document.html',
`<!doctype html><html lang="en"><head><title>hydrate document</title></head><body>${appHtml}<script src="./document.js"></script></body></html>`);

writeFileSync('out/div.html',
`<!doctype html><html lang="en"><head><title>hydrate div</title></head><body><div id="root">${appHtml}</div><script src="./div.js"></script></body></html>`);

console.log('SSR HTML written');
