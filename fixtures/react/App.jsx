import React, { useEffect, useState } from 'react';

export default function App() {
  const [items, setItems] = useState(['first']);
  const [registered, setRegistered] = useState(false);

  // The shape Margin uses: registration inside an effect on mount.
  useEffect(() => {
    let cancelled = false;
    (async () => {
      if (!document.modelContext) return;
      await document.modelContext.registerTool({
        name: 'add-item',
        description: 'Add an item to the list',
        inputSchema: { type: 'object', properties: { text: { type: 'string' } } },
        execute: async ({ text }) => {
          setItems((prev) => [...prev, text]);
          return { added: text };
        },
      });
      if (!cancelled) setRegistered(true);
    })();
    return () => { cancelled = true; };
  }, []);

  return (
    <div id="app">
      <h1>React hydration probe</h1>
      <p id="status">{registered ? 'registered' : 'not registered'}</p>
      <ul>{items.map((t, i) => <li key={i}>{t}</li>)}</ul>
    </div>
  );
}
