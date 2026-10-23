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
      <span class="ef-install__prompt">$</span> efema-server keys add home write
      <span class="ef-install__comment"># efema_2a61… - a device's key to space home</span>

      <span class="ef-install__prompt">$</span> efema-server serve
      <span class="ef-install__comment"># listening on 127.0.0.1:2451</span></pre>
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

A device appends a **batch** of changes to a named **stream**, in the
**space** its access key opens. The relay gives each entry the next
**position** and links it into the stream's hash chain.
Every other device reads what came after its **cursor** - or waits until there
is something to read. Batches arrive whole and in one order for everyone,
and a cursor that no longer matches the relay's history is refused, not
silently read past.

## Or no server at all

An S3 bucket can stand where the relay stands: Amazon's, a MinIO of your own,
or another service that speaks S3. The devices keep the stream in it
themselves, in the same one order, under the same seal - every batch created
in its place only if nothing is there yet, so two devices can never write one
place twice. [Without a server](/efema/guides/without-a-server/) has how, and
what changes against a relay.

## What it does not do

- **It does not read your data.** Entries are opaque bytes: the relay stores
  and returns them without decoding anything. The client library seals them on
  the device before they leave it, under a key the relay never has.
- **It does not merge.** Two devices that changed the same thing both have
  their batch kept, in order. Deciding what the result is belongs to the app,
  which knows what its data means - the relay does not, by design.
- **It does not terminate TLS.** Put it behind the reverse proxy that already
  serves your other things - plain HTTP from another machine it refuses.

## Status

0.7.0 is the relay and the Rust client library,
[`efema`](https://docs.rs/efema), which syncs through a relay or through an S3
bucket with nothing to run ([Without a server](/efema/guides/without-a-server/)),
and a relay safe to leave reachable behind an HTTPS proxy: access keys to spaces, that read or that write; limits per key,
announced and followed; plain HTTP from outside refused. It installs with one
command and runs as a service on a Linux server
([On a server](/efema/guides/on-a-server/)). Every entry is compressed and
sealed on the device with a key the relay never has, kept there until the
relay has it and written once however often a broken network sends it again;
a connection gone silent is given up on in half a minute, a slow one is never
cut. Each of those promises is shown by tests, not only stated.
[Syncing an app](/efema/guides/syncing-an-app/) is where to start.
