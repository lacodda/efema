---
title: Threat model
description: What an efema relay can see and do, what it cannot, and what this version does not protect yet.
---

A relay sits between your devices and holds everything they say to each other.
This page is the honest account of what that position allows - written before
the code that seals entries, so the promise is on record before the
implementation and not fitted to it afterwards.

## What the relay sees

Whoever runs the relay - you, a friend, a hosting company - can see:

- **stream names**, which travel in the URL;
- for every entry: its **position**, **size**, **epoch** and **link**;
- **when** each batch was written and each read and wait happened;
- the **network address** of every device that talks to it.

Pick stream names accordingly. `notes` says little; `bank-passwords` says what
it says to anyone who can read the relay's database or its logs.

## What it does not see

**The contents of entries.** The relay stores the bytes it was given and returns
them. Nothing in it decodes an entry, and its listing has no way to show one.

That alone protects nothing - an entry written in plain text is stored in plain
text. The protection comes from the client library (0.2.0), which **seals every
entry on the device** with a key that never leaves the devices: the relay
stores ciphertext, and so does every backup of the relay. The library has no
method that sends an entry unsealed; "without encryption" is not a mode efema
has.

## What this version does not protect yet

0.1.0 is the relay alone, and it is open:

- **No access control.** Anyone who can reach the port can read and write every
  stream. Access keys - separate ones for reading and writing - arrive in 0.5.0.
- **No TLS.** The relay speaks plain HTTP. It listens on loopback by default;
  anything beyond this machine goes through a reverse proxy that terminates TLS
  ([guide](/efema/guides/behind-a-reverse-proxy/)). The relay will not grow TLS
  of its own: the proxy in front of it already does that well.
- **No sealing.** Until 0.2.0 nothing seals for you. Whatever a client sends is
  what the relay keeps.

Run 0.1.0 on loopback, or on a network where everyone who can reach it may read
and write everything in it.

## What a dishonest relay can do

Sealing keeps a relay from reading entries. It does not make the relay
trustworthy in every other way. A relay that wants to misbehave can:

- **withhold** entries - from everyone, or from some devices;
- **delay** them, or answer waits late;
- **drop** a stream, or the whole database;
- **show different histories to different devices.** The hash chain catches a
  history that changed under a cursor by accident, but a relay that lies on
  purpose can compute a chain for whatever history it likes.

It cannot read sealed entries, and it cannot forge one: sealing authenticates
as well as encrypts, so an entry the relay made up or altered fails to unseal
on the device.

## What the relay protects against

Accidents, mostly - the kind that happen to honest operators:

- **A restore from an old backup**, or a disk that lost recent writes: cursors
  past the relay's head are refused with `cursor_ahead`, and cursors into a
  history that went another way with `cursor_diverged`.
- **A relay that was wiped and started again**: streams get new identities,
  and old cursors are refused with `stream_replaced`.
- **Two versions of an app writing different formats**: epochs keep an older
  version from mixing its entries in after a newer one moved the stream on.
- **A crash or a power cut mid-write**: a batch is committed whole, and an
  acknowledged write is on disk before the device hears about it.
