// Session persistence: snapshot and restore the storage a page treats as its
// own — localStorage and IndexedDB.
//
// sessionStorage is deliberately absent. In a browser it dies with the tab, so
// carrying it across runs would emulate something browsers do not do.
//
// Cookies are not here either: the ones that matter for a signed-in session are
// HttpOnly, which means script cannot see them by design. They live in the
// host's HTTP layer instead.
(function () {
  "use strict";

  // ---------------------------------------------------------------- values
  //
  // IndexedDB stores structured-clone values, not JSON. Dates, typed arrays and
  // Maps all survive a real browser restart, so they have to survive ours;
  // JSON.stringify would silently flatten every one of them. Tagged wrappers
  // keep them distinguishable from a plain object that happens to look alike.

  var TAG = "__conduit_t";

  function encode(value) {
    if (value === undefined) return { [TAG]: "undefined" };
    if (value === null) return null;

    var type = typeof value;
    if (type === "boolean" || type === "string") return value;
    if (type === "number") {
      // JSON has no NaN or Infinity; they round-trip to null without this.
      if (Number.isFinite(value)) return value;
      return { [TAG]: "number", v: String(value) };
    }
    if (type === "bigint") return { [TAG]: "bigint", v: value.toString() };

    if (value instanceof Date) {
      return { [TAG]: "Date", v: value.getTime() };
    }
    if (value instanceof RegExp) {
      return { [TAG]: "RegExp", src: value.source, flags: value.flags };
    }
    if (value instanceof ArrayBuffer) {
      return { [TAG]: "ArrayBuffer", v: bytesToBase64(new Uint8Array(value)) };
    }
    if (ArrayBuffer.isView(value)) {
      var ctor = value.constructor && value.constructor.name;
      var bytes = new Uint8Array(
        value.buffer,
        value.byteOffset,
        value.byteLength
      );
      return { [TAG]: "TypedArray", kind: ctor, v: bytesToBase64(bytes) };
    }
    if (value instanceof Map) {
      var entries = [];
      value.forEach(function (v, k) {
        entries.push([encode(k), encode(v)]);
      });
      return { [TAG]: "Map", v: entries };
    }
    if (value instanceof Set) {
      var items = [];
      value.forEach(function (v) {
        items.push(encode(v));
      });
      return { [TAG]: "Set", v: items };
    }
    if (Array.isArray(value)) return value.map(encode);

    if (type === "object") {
      // Blob and File carry bytes we would have to read asynchronously, and a
      // half-restored file is worse than a loud refusal.
      var name = value.constructor && value.constructor.name;
      if (name === "Blob" || name === "File") {
        throw new Error(
          "cannot snapshot a " + name + ": binary blobs are not supported yet"
        );
      }
      var out = {};
      for (var key in value) {
        if (Object.prototype.hasOwnProperty.call(value, key)) {
          out[key] = encode(value[key]);
        }
      }
      return out;
    }

    throw new Error("cannot snapshot a value of type " + type);
  }

  function decode(value) {
    if (value === null || typeof value !== "object") return value;
    if (Array.isArray(value)) return value.map(decode);

    var tag = value[TAG];
    if (tag === undefined) {
      var out = {};
      for (var key in value) {
        if (Object.prototype.hasOwnProperty.call(value, key)) {
          out[key] = decode(value[key]);
        }
      }
      return out;
    }

    switch (tag) {
      case "undefined":
        return undefined;
      case "number":
        return Number(value.v);
      case "bigint":
        return BigInt(value.v);
      case "Date":
        return new Date(value.v);
      case "RegExp":
        return new RegExp(value.src, value.flags);
      case "ArrayBuffer":
        return base64ToBytes(value.v).buffer;
      case "TypedArray": {
        var bytes = base64ToBytes(value.v);
        var Ctor = globalThis[value.kind];
        if (typeof Ctor !== "function") return bytes;
        return new Ctor(
          bytes.buffer,
          0,
          bytes.byteLength / (Ctor.BYTES_PER_ELEMENT || 1)
        );
      }
      case "Map":
        return new Map(
          value.v.map(function (pair) {
            return [decode(pair[0]), decode(pair[1])];
          })
        );
      case "Set":
        return new Set(value.v.map(decode));
      default:
        return value;
    }
  }

  var B64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

  function bytesToBase64(bytes) {
    var out = "";
    for (var i = 0; i < bytes.length; i += 3) {
      var b0 = bytes[i],
        b1 = bytes[i + 1],
        b2 = bytes[i + 2];
      out += B64[b0 >> 2];
      out += B64[((b0 & 3) << 4) | ((b1 || 0) >> 4)];
      out += i + 1 < bytes.length ? B64[((b1 & 15) << 2) | ((b2 || 0) >> 6)] : "=";
      out += i + 2 < bytes.length ? B64[b2 & 63] : "=";
    }
    return out;
  }

  function base64ToBytes(str) {
    var clean = String(str).replace(/=+$/, "");
    var bytes = new Uint8Array((clean.length * 3) >> 2);
    var acc = 0,
      bits = 0,
      out = 0;
    for (var i = 0; i < clean.length; i++) {
      acc = (acc << 6) | B64.indexOf(clean[i]);
      bits += 6;
      if (bits >= 8) {
        bits -= 8;
        bytes[out++] = (acc >> bits) & 0xff;
      }
    }
    return bytes;
  }

  // ------------------------------------------------------------- indexeddb

  function request(req) {
    return new Promise(function (resolve, reject) {
      req.onsuccess = function () {
        resolve(req.result);
      };
      req.onerror = function () {
        reject(req.error || new Error("IndexedDB request failed"));
      };
    });
  }

  function openExisting(name, version) {
    return new Promise(function (resolve, reject) {
      var req = indexedDB.open(name, version);
      req.onsuccess = function () {
        resolve(req.result);
      };
      req.onerror = function () {
        reject(req.error || new Error("could not open database " + name));
      };
    });
  }

  async function exportDatabases() {
    if (typeof indexedDB === "undefined" || !indexedDB.databases) return [];

    var listed = await indexedDB.databases();
    var databases = [];

    for (var i = 0; i < listed.length; i++) {
      var info = listed[i];
      if (!info || !info.name) continue;

      var db = await openExisting(info.name, info.version);
      var stores = [];

      var names = Array.prototype.slice.call(db.objectStoreNames);
      for (var j = 0; j < names.length; j++) {
        var tx = db.transaction(names[j], "readonly");
        var store = tx.objectStore(names[j]);

        var indexes = Array.prototype.slice.call(store.indexNames).map(
          function (indexName) {
            var index = store.index(indexName);
            return {
              name: index.name,
              keyPath: index.keyPath,
              unique: !!index.unique,
              multiEntry: !!index.multiEntry,
            };
          }
        );

        // Keys are fetched alongside values because an out-of-line store keeps
        // its keys outside the record, and putting the values back without them
        // would renumber everything.
        //
        // Both requests are issued *and* subscribed to before either is
        // awaited, which two separate hazards demand.
        //
        // An IndexedDB transaction commits as soon as control returns to the
        // event loop with nothing outstanding, so issuing the second request
        // after awaiting the first throws TransactionInactiveError. And
        // `request()` attaches onsuccess when it is called, so calling it after
        // the first await misses an event that has already fired — the promise
        // then never settles and the snapshot hangs.
        var keysPromise = request(store.getAllKeys());
        var valuesPromise = request(store.getAll());
        var keys = await keysPromise;
        var values = await valuesPromise;

        var records = [];
        for (var k = 0; k < values.length; k++) {
          records.push({ key: encode(keys[k]), value: encode(values[k]) });
        }

        stores.push({
          name: store.name,
          keyPath: store.keyPath,
          autoIncrement: !!store.autoIncrement,
          indexes: indexes,
          records: records,
        });
      }

      db.close();
      databases.push({ name: info.name, version: info.version, stores: stores });
    }

    return databases;
  }

  async function importDatabases(databases) {
    for (var i = 0; i < databases.length; i++) {
      var spec = databases[i];

      // The schema is recreated inside onupgradeneeded because that is the only
      // place IndexedDB permits it.
      var db = await new Promise(function (resolve, reject) {
        var req = indexedDB.open(spec.name, spec.version);
        req.onupgradeneeded = function () {
          var opened = req.result;
          spec.stores.forEach(function (store) {
            if (opened.objectStoreNames.contains(store.name)) return;
            var created = opened.createObjectStore(store.name, {
              keyPath: store.keyPath === null ? undefined : store.keyPath,
              autoIncrement: store.autoIncrement,
            });
            store.indexes.forEach(function (index) {
              created.createIndex(index.name, index.keyPath, {
                unique: index.unique,
                multiEntry: index.multiEntry,
              });
            });
          });
        };
        req.onsuccess = function () {
          resolve(req.result);
        };
        req.onerror = function () {
          reject(req.error || new Error("could not restore " + spec.name));
        };
      });

      for (var j = 0; j < spec.stores.length; j++) {
        var store = spec.stores[j];
        if (!store.records.length) continue;

        var tx = db.transaction(store.name, "readwrite");
        var target = tx.objectStore(store.name);
        for (var k = 0; k < store.records.length; k++) {
          var record = store.records[k];
          var value = decode(record.value);
          // An in-line key lives in the record itself; passing it separately is
          // a DataError.
          if (store.keyPath === null || store.keyPath === undefined) {
            target.put(value, decode(record.key));
          } else {
            target.put(value);
          }
        }
        await new Promise(function (resolve, reject) {
          tx.oncomplete = resolve;
          tx.onerror = function () {
            reject(tx.error || new Error("restoring " + store.name));
          };
        });
      }

      db.close();
    }
  }

  // ---------------------------------------------------------------- driver
  //
  // Both halves are async, and the host has no async entry point into the
  // engine — it starts work and then settles the event loop. So these report
  // completion through a global the host polls, rather than a returned promise.

  function finish(error) {
    globalThis.__conduit_session_error = error ? String(error.message || error) : "";
    globalThis.__conduit_session_done = true;
  }

  globalThis.__conduit_session_export = function () {
    globalThis.__conduit_session_done = false;
    globalThis.__conduit_session_out = "";

    (async function () {
      var local = {};
      try {
        for (var i = 0; i < localStorage.length; i++) {
          var key = localStorage.key(i);
          local[key] = localStorage.getItem(key);
        }
      } catch (e) {
        // A page that blocked storage access is not a reason to lose the rest.
      }

      globalThis.__conduit_session_out = JSON.stringify({
        localStorage: local,
        databases: await exportDatabases(),
      });
    })().then(
      function () {
        finish(null);
      },
      function (e) {
        finish(e);
      }
    );
  };

  globalThis.__conduit_session_import = function (json) {
    globalThis.__conduit_session_done = false;

    (async function () {
      var state = JSON.parse(json);

      if (state.localStorage) {
        for (var key in state.localStorage) {
          if (Object.prototype.hasOwnProperty.call(state.localStorage, key)) {
            localStorage.setItem(key, state.localStorage[key]);
          }
        }
      }

      if (state.databases && state.databases.length) {
        await importDatabases(state.databases);
      }
    })().then(
      function () {
        finish(null);
      },
      function (e) {
        finish(e);
      }
    );
  };
})();
