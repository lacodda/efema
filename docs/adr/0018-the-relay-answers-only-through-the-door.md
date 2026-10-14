# ADR 0018: The relay answers its own machine and HTTPS through the door, and nothing else

- Status: accepted
- Date: 2026-10-14

## Context

The relay has no TLS of its own, on purpose: a reverse proxy in front of it -
Caddy on the owner's Pi, `tls internal` - does that well, and one door for
every service on a machine is less to get wrong than one per service. With
access keys (ADR 0016) plain HTTP across a network is worse than before: every
request carries a key, and anyone on the way reads it.

The relay listens on loopback by default. But a default is not a guarantee:
`--listen 0.0.0.0:2451` "just to try it", a proxy that also forwards plain
HTTP, or a proxy in another container that reaches the relay over a network.
The promise "safe to leave on the internet" has to hold for those too, and be
shown by a test that makes the mistake, not claimed.

## Decision

- **A request is answered only if it was made on the relay's machine, or
  forwarded by a trusted proxy that took it over HTTPS.** The same test covers
  both: the connection comes from a trusted address. A trusted peer that sends
  no forwarding header (`Forwarded`, `X-Forwarded-*`) made the request itself;
  one that forwards has to say `https` - in `X-Forwarded-Proto` or in the
  `proto` of `Forwarded`, for every hop. A proxy that forwards without saying
  how the request reached it is refused: there is no telling.
- **Trusted is loopback, plus what the operator names** with `--trust-proxy`,
  an address or a network - for a proxy in another container or on another
  host. Two setups that both exist, so a setting rather than a guess.
- **Everything else is `403 https_required`**, before the key is looked at,
  on every path - `/health` included.
- **The client sends a key over plain HTTP only to its own machine.**
  `Relay::new` refuses `http://` to any host but loopback and `localhost`: the
  key would be on the network before the relay could refuse it.
- **`/metrics` answers only the relay's own machine.** Counts and times per
  space - streams, entries, bytes, keys, the last write, the last use of a
  key - in the text format Prometheus reads, never a stream's name or an
  entry's bytes. Through the door it is `403 forbidden`: the spaces of a relay
  are its operator's business, not that of whoever reaches the door. A second
  kind of key just for metrics would be a second system of rights.

## Consequences

- A relay put on every interface by mistake refuses every request from the
  network, and says why - shown by a test that runs the built binary on
  `0.0.0.0` and calls it at the machine's own network address.
- The proxy has to say how a request reached it. Caddy does by default; nginx
  needs `proxy_set_header X-Forwarded-Proto $scheme`. The guide says so.
- A proxy that forwards with no header at all looks exactly like the relay's
  own machine. That is the one setup this cannot tell apart, and the guide
  names it.
- Metrics from another machine go through the operator's own way onto the
  relay's - an SSH session, an agent next to it.
