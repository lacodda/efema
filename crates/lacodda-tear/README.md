# lacodda-tear

> A TCP proxy for tests that tears connections where you say: after so many bytes either way, after a while, or all at once.

[![crates.io](https://img.shields.io/crates/v/lacodda-tear?style=flat-square)](https://crates.io/crates/lacodda-tear)
[![docs.rs](https://img.shields.io/docsrs/lacodda-tear?style=flat-square&label=docs.rs)](https://docs.rs/lacodda-tear)

Code that talks over a network has to survive the network breaking: a request
torn halfway up, an answer lost on its way back, a phone that drops off and
comes back. A mock of the server cannot show any of that - it never breaks the
way a socket does. `Tear` stands between the code under test and the real
server, on a real socket, passes every byte through, and breaks the connection
on cue. It is the test bench of the [lacodda line](https://github.com/lacodda):
the [efema](https://github.com/lacodda/efema) sync client shows with it that a
push survives a broken network.

```rust
use lacodda_tear::{Cut, Tear};

let tear = Tear::start(server_addr).await?;
// Point the client at tear.addr() instead of the server.

tear.cut(Cut::Up(2_000));   // a request torn after 2000 more bytes
tear.cut(Cut::Down(0));     // the next answer lost after the server sent it
tear.cut(Cut::Now);         // every open connection, now
tear.offline();             // the network gone: open ones torn, new ones reset
tear.online();
tear.stall();               // bytes held, connections open and silent
tear.flow();
assert_eq!(tear.torn(), 1);
```

Byte counts run across every connection from the moment a cut is armed, so a
client that reuses a pooled connection is cut just the same. A tear is a reset,
as a dropped mobile connection looks to both ends. The proxy knows nothing of
what it carries - HTTP, a database's protocol, anything over TCP.

## License

MIT (c) [Kirill Lakhtachev](https://lacodda.com)
