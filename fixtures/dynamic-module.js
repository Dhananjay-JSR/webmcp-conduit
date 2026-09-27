document.modelContext.registerTool({
  name: "from-dynamic-import",
  description: "Registered by a dynamically imported module",
  execute: function(){ return { ok: true }; }
});
export const loaded = true;
