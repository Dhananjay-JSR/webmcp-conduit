// IndexedDB. happy-dom does not implement it, and a local-first app — which
// is exactly the kind of app that ships WebMCP tools worth calling — stores
// its documents there and cannot boot without it.
//
// fake-indexeddb is the in-memory implementation the JS testing ecosystem
// uses. In-memory is not a compromise here: a harvest should leave nothing
// behind, so an ephemeral store is the correct behaviour rather than a
// degraded one.
import 'fake-indexeddb/auto';
