import React, { use, useEffect, useState, Suspense } from 'react';
import { hydrateRoot } from 'react-dom/client';
import { createFromReadableStream } from 'react-server-dom-webpack/client.browser';

// Mirrors Margin: inline classic scripts push flight rows onto a global, a
// module turns them into a stream, and the decoded promise is handed to the
// component tree. `self` vs `globalThis` matters here — that was a real bug.
function payloadStream() {
  const rows = self.__RSC_CHUNKS__ || [];
  const enc = new TextEncoder();
  return new ReadableStream({
    start(controller) {
      for (const row of rows) controller.enqueue(enc.encode(row));
      if (self.__RSC_DONE__) controller.close();
    },
  });
}

// A client component reachable from the flight payload by module id — the
// shape Margin uses for MarginApp, and the one piece the first fixture did
// not cover. Registration lives HERE, inside the client component, exactly
// as Margin does it.
function Widget() {
  const [registered, setRegistered] = useState(false);
  useEffect(() => {
    let cancelled = false;
    (async () => {
      if (!document.modelContext) return;
      await document.modelContext.registerTool({
        name: 'client-ref-tool',
        description: 'Registered by a client component resolved from the flight payload',
        execute: async () => ({ ok: true }),
      });
      if (!cancelled) setRegistered(true);
    })();
    return () => { cancelled = true; };
  }, []);
  return <p id="widget">{registered ? 'widget registered' : 'widget idle'}</p>;
}

globalThis.__RSC_MODULES__ = globalThis.__RSC_MODULES__ || {};
globalThis.__RSC_MODULES__['cmod'] = { Widget };

const elementPromise = createFromReadableStream(payloadStream());

function Shell() {
  const [registered, setRegistered] = useState(false);
  const tree = use(elementPromise);

  useEffect(() => {
    let cancelled = false;
    (async () => {
      if (!document.modelContext) return;
      await document.modelContext.registerTool({
        name: 'rsc-tool',
        description: 'Registered from an effect after RSC hydration',
        execute: async () => ({ ok: true }),
      });
      if (!cancelled) setRegistered(true);
    })();
    return () => { cancelled = true; };
  }, []);

  return (
    <>
      <p id="status">{registered ? 'registered' : 'not registered'}</p>
      {tree}
    </>
  );
}

globalThis.__rscRoot = hydrateRoot(
  document,
  <html lang="en">
    <head><title>rsc probe</title></head>
    <body><Suspense fallback={<p>loading</p>}><Shell /></Suspense></body>
  </html>,
  {
    onRecoverableError: (e) => { globalThis.__recoverable = String((e && e.message) || e); },
    onUncaughtError: (e) => { globalThis.__uncaught = String((e && e.message) || e); },
    onCaughtError: (e) => { globalThis.__caught = String((e && e.message) || e); },
  }
);
