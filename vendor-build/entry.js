// The single symbol conduit needs from happy-dom. host-post.js constructs the
// Window from it and hoists the result onto globalThis.
import { Window } from 'happy-dom-without-node';
globalThis.__HappyWindow = Window;
