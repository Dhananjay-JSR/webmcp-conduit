// Variant A: hydrate the whole document — what Margin's RSC runtime does.
import React from 'react';
import { hydrateRoot } from 'react-dom/client';
import App from './App.jsx';

globalThis.__variant = 'document';
globalThis.__root = hydrateRoot(
  document,
  <html lang="en"><head><title>hydrate document</title></head><body><App /></body></html>,
  { onRecoverableError: (e) => { globalThis.__recoverable = String(e && e.message || e); },
    onUncaughtError: (e) => { globalThis.__uncaught = String(e && e.message || e); } }
);
