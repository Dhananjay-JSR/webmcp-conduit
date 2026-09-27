// happy-dom bundles the `ws` package for WebSocket, which needs real Node
// streams. A WebMCP harvest never opens a socket, so it is cut out entirely.
export class WebSocket {
  constructor(){ throw new Error('WebSocket is not available in conduit'); }
}
export class WebSocketServer { constructor(){ throw new Error('unavailable'); } }
export class Receiver {}
export class Sender {}
export default WebSocket;
