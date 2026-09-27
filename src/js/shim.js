// conduit — document.modelContext, per the W3C WebMCP IDL.
//
// Layered on happy-dom's Event and EventTarget, so these are real DOM classes
// rather than approximations.
//
// Deliberate fidelity notes, because these are the parts people get wrong:
//   * executeTool() resolves to a DOMString — the JSON-*stringified* result,
//     not an MCP `{content:[...]}` block. The page's execute() returns `any`;
//     serialization happens here. Mapping to MCP content is the host's job.
//   * RegisteredTool carries a live `window` reference, so it can never cross
//     the host boundary. We hand the host a serializable projection and keep
//     the real objects in this context, addressed by name.
//   * Tool names are capped at 128 characters by the spec.
(function (globalThis) {
  "use strict";

  var MAX_NAME = 128;
  var BaseEvent = globalThis.Event;
  var BaseEventTarget = globalThis.EventTarget;

  class ToolActivatedEvent extends BaseEvent {
    constructor(type, init) {
      super(type, init);
      this.toolName = (init && init.toolName) || "";
    }
  }

  class ToolCancelEvent extends BaseEvent {
    constructor(type, init) {
      super(type, init);
      this.toolName = (init && init.toolName) || "";
    }
  }

  function normalizeAnnotations(a) {
    a = a || {};
    return {
      readOnlyHint: !!a.readOnlyHint,
      untrustedContentHint: !!a.untrustedContentHint,
      consequentialHint: !!a.consequentialHint,
      debugging: !!a.debugging,
    };
  }

  class ModelContext extends BaseEventTarget {
    constructor(origin) {
      super();
      this._tools = new Map();
      this._origin = origin;
      this.ontoolchange = null;
      this.ontoolactivated = null;
      this.ontoolcancel = null;
    }

    // The IDL exposes ontoolchange and friends as event handler attributes.
    // EventTarget does not invoke those on its own, so fan out here.
    _emit(event) {
      var handler = this["on" + event.type];
      if (typeof handler === "function") {
        try { handler.call(this, event); }
        catch (e) { globalThis.__conduit_report_error(e); }
      }
      this.dispatchEvent(event);
    }

    registerTool(tool, options) {
      var self = this;
      return new Promise(function (resolve, reject) {
        try {
          if (!tool || typeof tool !== "object") {
            throw new TypeError("registerTool: tool must be an object");
          }
          if (typeof tool.name !== "string" || !tool.name) {
            throw new TypeError("registerTool: `name` is required");
          }
          if (tool.name.length > MAX_NAME) {
            throw new TypeError(
              "registerTool: `name` exceeds " + MAX_NAME + " characters"
            );
          }
          if (typeof tool.description !== "string" || !tool.description) {
            throw new TypeError("registerTool: `description` is required");
          }
          if (typeof tool.execute !== "function") {
            throw new TypeError("registerTool: `execute` must be a function");
          }

          var opts = options || {};
          var record = {
            name: tool.name,
            title: typeof tool.title === "string" ? tool.title : undefined,
            description: tool.description,
            inputSchema: tool.inputSchema,
            execute: tool.execute,
            annotations: normalizeAnnotations(tool.annotations),
            exposedTo: Array.isArray(opts.exposedTo) ? opts.exposedTo.slice() : undefined,
            origin: self._origin,
          };

          self._tools.set(record.name, record);

          // Per spec, the tool is unregistered when the signal aborts.
          if (opts.signal && typeof opts.signal.addEventListener === "function") {
            if (opts.signal.aborted) {
              self._tools.delete(record.name);
            } else {
              opts.signal.addEventListener("abort", function () {
                if (self._tools.get(record.name) === record) {
                  self._tools.delete(record.name);
                  self._emit(new BaseEvent("toolchange"));
                }
              });
            }
          }

          self._emit(new BaseEvent("toolchange"));
          resolve(undefined);
        } catch (e) {
          reject(e);
        }
      });
    }

    getTools(options) {
      var self = this;
      return new Promise(function (resolve) {
        var from = options && Array.isArray(options.fromOrigins)
          ? options.fromOrigins : null;
        var out = [];
        self._tools.forEach(function (rec) {
          if (from && from.indexOf(rec.origin) === -1) return;
          out.push({
            name: rec.name,
            title: rec.title,
            description: rec.description,
            inputSchema: rec.inputSchema,
            // Live and intentionally non-serializable, exactly as the IDL says.
            window: globalThis.window,
            origin: rec.origin,
            annotations: rec.annotations,
          });
        });
        resolve(out);
      });
    }

    executeTool(tool, inputObject, options) {
      var self = this;
      var name = tool && tool.name;
      var rec = name ? self._tools.get(name) : null;

      if (!rec) {
        return Promise.reject(new Error("Unknown tool: " + String(name)));
      }

      var opts = options || {};
      var controller = new globalThis.AbortController();
      if (opts.signal) {
        if (opts.signal.aborted) {
          return Promise.reject(opts.signal.reason || new Error("AbortError"));
        }
        opts.signal.addEventListener("abort", function () {
          controller.abort(opts.signal.reason);
          self._emit(new ToolCancelEvent("toolcancel", { toolName: name }));
        });
      }

      self._emit(new ToolActivatedEvent("toolactivated", { toolName: name }));

      return Promise.resolve()
        .then(function () {
          return rec.execute(inputObject || {}, { signal: controller.signal });
        })
        .then(function (result) {
          // Spec: resolve with the stringified result.
          if (typeof result === "string") return result;
          try {
            return JSON.stringify(result === undefined ? null : result);
          } catch (e) {
            throw new TypeError(
              "Tool result is not JSON-serializable: " + String(e)
            );
          }
        });
    }
  }

  var origin = (globalThis.location && globalThis.location.origin) || "null";
  var modelContext = new ModelContext(origin);

  Object.defineProperty(globalThis.document, "modelContext", {
    value: modelContext, writable: false, enumerable: true, configurable: true,
  });
  // Deprecated in Chrome 150, still shipped during the origin trial. Pages in
  // the wild feature-detect either one, so provide both.
  try {
    Object.defineProperty(globalThis.navigator, "modelContext", {
      value: modelContext, writable: false, enumerable: true, configurable: true,
    });
  } catch (e) { /* some navigator implementations are sealed */ }

  globalThis.ToolActivatedEvent = ToolActivatedEvent;
  globalThis.ToolCancelEvent = ToolCancelEvent;

  // ------------------------------------------------------- host bridge
  // Everything below is conduit's, not the spec's. The host only ever sees
  // serializable data; live objects stay in this context.
  globalThis.__conduit_harvest = function () {
    var out = [];
    modelContext._tools.forEach(function (rec) {
      out.push({
        name: rec.name,
        title: rec.title || null,
        description: rec.description,
        inputSchema: rec.inputSchema || null,
        annotations: rec.annotations,
        origin: rec.origin,
        exposedTo: rec.exposedTo || null,
      });
    });
    return JSON.stringify(out);
  };

  globalThis.__conduit_execute = function (name, argsJson) {
    var args;
    try {
      args = argsJson ? JSON.parse(argsJson) : {};
    } catch (e) {
      return Promise.reject(new Error("Invalid JSON arguments: " + String(e)));
    }
    return modelContext.executeTool({ name: name }, args, {});
  };

  // Promise interop with the host is avoided deliberately: the host calls
  // this, drains the job queue, then reads `__conduit_result`. Keeping the
  // await on the JS side means no Rust-side promise plumbing to get wrong.
  globalThis.__conduit_result = null;
  globalThis.__conduit_run = function (name, argsJson) {
    globalThis.__conduit_result = { status: "pending" };
    try {
      globalThis.__conduit_execute(name, argsJson).then(
        function (v) { globalThis.__conduit_result = { status: "ok", value: v }; },
        function (e) {
          globalThis.__conduit_result = {
            status: "error", message: String((e && e.message) || e),
          };
        }
      );
    } catch (e) {
      globalThis.__conduit_result = {
        status: "error", message: String((e && e.message) || e),
      };
    }
  };

  // A module's body throws into its evaluation promise, never to the caller.
  // The host hands that promise here so a rejection lands in the same place
  // every other page error does, instead of vanishing.
  globalThis.__conduit_watch_module = function (p, label) {
    try {
      Promise.resolve(p).catch(function (e) {
        globalThis.__conduit_errors.push(
          label + ": " + String((e && e.message) || e)
        );
      });
    } catch (e) {
      globalThis.__conduit_errors.push(label + ": " + String(e));
    }
  };
})(globalThis);
