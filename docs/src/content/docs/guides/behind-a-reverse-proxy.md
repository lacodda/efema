---
title: Behind a reverse proxy
description: Serve the relay over HTTPS through Caddy or nginx, and the four settings a proxy needs for it.
---

The relay speaks plain HTTP and listens on loopback. To reach it from other
devices, put it behind the reverse proxy you already run: the proxy terminates
TLS, the relay stays on `127.0.0.1`.

Keep the relay itself on loopback when you do. A relay that listens on all
interfaces next to a proxy would be reachable without TLS - it refuses every
such request, with `https_required`, but the access key in it has already
crossed the network in the clear.

The relay answers a request from the proxy only if the proxy says it took it
over HTTPS: `X-Forwarded-Proto: https`, or `proto=https` in `Forwarded`. A
proxy that forwards plain HTTP, or forwards without saying, is refused.

## Caddy

```caddyfile
relay.example.com {
	reverse_proxy 127.0.0.1:2451
}
```

Caddy obtains the certificate, streams request and response bodies, keeps idle
upstream requests open long enough for a wait, and says how each request
reached it. Nothing else is needed.

On a home network with a private name (`relay.home.arpa`, say), use Caddy's own
certificate authority and install its root on your devices:

```caddyfile
relay.home.arpa {
	tls internal
	reverse_proxy 127.0.0.1:2451
}
```

## nginx

```nginx
server {
    listen 443 ssl;
    server_name relay.example.com;
    # ssl_certificate and ssl_certificate_key as for your other sites

    # A batch is up to 16 MiB; nginx refuses anything over 1 MiB by default.
    client_max_body_size 16m;

    location / {
        proxy_pass http://127.0.0.1:2451;
        proxy_http_version 1.1;
        # Without it the relay cannot tell HTTPS from plain HTTP, and
        # refuses the request.
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        # A wait lasts up to 60 seconds; nginx gives up on an upstream after 60.
        proxy_read_timeout 90s;
        proxy_buffering off;
    }
}
```

## A proxy on another host

A proxy in another container or on another machine reaches the relay from an
address that is not loopback, and the relay takes it for a stranger. Name it:

```console
$ efema-server serve --listen 0.0.0.0:2451 --trust-proxy 172.18.0.0/16
```

Only requests from those addresses that say `https` are answered - anything
else that reaches the port is refused.

## What any proxy needs

Four things, whichever proxy it is:

- **Saying how the request came.** `X-Forwarded-Proto` (or `Forwarded`) on
  every request. A proxy that sends no forwarding header at all looks to the
  relay like a program on its own machine - the one setup the relay cannot
  tell from a local client; so do not configure one that way.

- **Bodies up to 16 MiB.** That is the relay's own limit on a request; a proxy
  with a lower one turns large batches into errors before the relay sees them.
- **Idle upstream reads of more than 60 seconds.** A wait holds its request
  open for up to a minute with nothing to say; a proxy that cuts it sooner turns
  every quiet minute into an error.
- **No caching.** The relay marks every answer `Cache-Control: no-store`. A
  proxy that caches anyway would hand a reader a page that ended where the
  stream did not.

## Check it

From another device:

```console
$ curl https://relay.example.com/health
ok
$ curl -s -o /dev/null -w '%{http_code}\n' http://relay.example.com:2451/health
403
```

The first is the door; the second, if the port answers at all, is the relay
refusing plain HTTP from outside. `/metrics` answers only on the relay's own
machine, so through the door it is `403` too.
