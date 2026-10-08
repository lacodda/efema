---
title: streams
description: List the streams a relay keeps, or the entries of one - positions, epochs and sizes, never contents.
---

```console
$ efema-server streams [NAME] [--data <DIR>]
```

Reads the relay's database and prints what it keeps. Safe while the relay is
serving: it opens the database read-only and changes nothing - and on a
directory with no relay in it, it says so instead of creating one.

## Every stream

```console
$ efema-server streams --data ./relay
STREAM  ID        EPOCH  ENTRIES       SIZE  UPDATED (UTC)
notes   11c3cd7e  1            3    1.7 KiB  2026-10-08 18:36
photos  6964d409  2            3  144.0 KiB  2026-10-08 18:36
```

| Column | Meaning |
| --- | --- |
| `STREAM` | the stream's name |
| `ID` | the first eight hex digits of its identity, which changes only if the stream is started over |
| `EPOCH` | the stream's current epoch |
| `ENTRIES` | its last position - the number of entries, since positions start at 1 and have no gaps |
| `SIZE` | the entries' data, without the relay's own bookkeeping |
| `UPDATED (UTC)` | when its last batch was written |

## One stream

```console
$ efema-server streams notes --data ./relay
SEQ  EPOCH   SIZE  HASH
  1  1      640 B  0d72a730
  2  1      560 B  19cdc65a
  3  1      512 B  235f28b7
```

One line per entry: its position, the epoch it was written in, its size and the
first eight hex digits of its link in the stream's chain.

There is no option to show an entry's contents, and there will not be one. An
entry is opaque to the relay, and so to its listing.

## Options

| Option | Default | Meaning |
| --- | --- | --- |
| `NAME` | none | show this stream's entries instead of the list of streams |
| `--data <DIR>` | the local data directory | the relay to look at - see [Commands](/efema/reference/commands/#the-data-directory) |

## Related

- [`serve`](/efema/reference/serve/) - the relay this looks at.
- [Streams and cursors](/efema/concepts/streams-and-cursors/) - what positions,
  epochs and links are.
