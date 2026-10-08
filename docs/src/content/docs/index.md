---
title: efema
description: "A sync relay for local-first apps: it keeps changes in order and cannot read them"
template: splash
# The hero carries the product name; repeating it as the page title renders as
# "efema | efema" in the browser tab and in search results.
head:
  - tag: title
    content: efema - a sync relay for local-first apps
hero:
  tagline: "A sync relay for local-first apps. Your devices write their changes into a stream; the relay keeps them in order and hands each device what it missed - without being able to read any of it."
  image:
    html: |
      <div class="ef-install">
        <div class="ef-install__bar">
          <span class="ef-install__dot"></span>
          <span class="ef-install__dot"></span>
          <span class="ef-install__dot"></span>
          <span class="ef-install__os">install &amp; run</span>
        </div>
        <pre class="ef-install__body"><span class="ef-install__prompt">$</span> cargo install efema-server
      <span class="ef-install__prompt">$</span> efema-server serve
      <span class="ef-install__comment"># listening on 127.0.0.1:2451</span>

      <span class="ef-install__prompt">$</span> curl 127.0.0.1:2451/health
      ok</pre>
      </div>
  actions:
    - text: Get started
      link: /efema/getting-started/
      icon: right-arrow
    - text: How it works
      link: /efema/concepts/streams-and-cursors/
      variant: secondary
    - text: View on GitHub
      link: https://github.com/lacodda/efema
      icon: external
      variant: minimal
---

## What it does

A local-first app keeps its data on the device, and works without a network.
The moment it runs on a second device, the two need somewhere to leave their
changes for each other. efema is that place, and nothing more.

A device appends a **batch** of changes to a named **stream**. The relay gives
each entry the next **position** and links it into the stream's hash chain.
Every other device reads what came after its **cursor** - or waits until there
is something to read. Batches arrive whole and in one order for everyone,
and a cursor that no longer matches the relay's history is refused, not
silently read past.

## What it does not do

- **It does not read your data.** Entries are opaque bytes: the relay stores
  and returns them without decoding anything. The client library seals them on
  the device before they leave it.
- **It does not merge.** Two devices that changed the same thing both have
  their batch kept, in order. Deciding what the result is belongs to the app,
  which knows what its data means - the relay does not, by design.
- **It does not terminate TLS.** Put it behind the reverse proxy that already
  serves your other things.

## Status

0.1.0 is the relay and its wire format: streams, positions, cursors, epochs and
waiting for news. The Rust client library - with sealing built in, so there is
no way to send anything unsealed - comes next, in 0.2.0. Until then a client is
anything that speaks [the protocol](/efema/reference/protocol/).
