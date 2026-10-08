---
title: Behind a reverse proxy
description: Serve the relay over HTTPS through Caddy or nginx, and the three settings a proxy needs for it.
---

The relay speaks plain HTTP and listens on loopback. To reach it from other
devices, put it behind the reverse proxy you already run: the proxy terminates
TLS, the relay stays on `127.0.0.1`.

Keep the relay itself on loopback when you do. A relay that listens on all
interfaces next to a proxy is a relay reachable without TLS.

## Caddy

```caddyfile
relay.example.com {
	reverse_proxy 127.0.0.1:2451
}
```

Caddy obtains the certificate, streams request and response bodies, and keeps
idle upstream requests open long enough for a wait. Nothing else is needed.

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
        # A wait lasts up to 60 seconds; nginx gives up on an upstream after 60.
        proxy_read_timeout 90s;
        proxy_buffering off;
    }
}
```

## What any proxy needs

Three things, whichever proxy it is:

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
```
