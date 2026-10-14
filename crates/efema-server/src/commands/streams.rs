use anyhow::{Context, Result, anyhow, bail};
use efema_proto::StreamName;
use efema_server::SpaceName;
use efema_server::store::Store;

use crate::cli::StreamsArgs;
use crate::commands::table::{print_table, size, timestamp};

/// Lists what the relay keeps, the way the relay sees it: names, positions,
/// epochs, sizes and hashes. There is no flag to show contents, and there will
/// not be one - an entry is opaque to the relay, and so to its listing.
pub fn run(args: StreamsArgs) -> Result<()> {
    let dir = args.data.dir()?;
    let store = Store::open_read_only(&dir).with_context(|| format!("cannot read the relay in {}", dir.display()))?;

    match args.stream {
        None => list_streams(&store),
        Some(path) => {
            let (space, name) = path
                .split_once('/')
                .ok_or_else(|| anyhow!("name the stream with its space: `<space>/<name>`, as the list shows it"))?;
            let space: SpaceName = space.parse().map_err(|e| anyhow!("{e}"))?;
            let name: StreamName = name.parse().map_err(|e| anyhow!("{e}"))?;
            list_entries(&store, &space, &name)
        }
    }
}

fn list_streams(store: &Store) -> Result<()> {
    let streams = store.streams().context("cannot list the streams")?;
    if streams.is_empty() {
        println!("No streams yet.");
        return Ok(());
    }
    let rows: Vec<[String; 6]> = streams
        .iter()
        .map(|s| {
            [
                format!("{}/{}", s.space, s.name),
                s.id.to_string()[..8].to_owned(),
                s.epoch.to_string(),
                s.head.to_string(),
                size(s.bytes),
                timestamp(s.updated_at),
            ]
        })
        .collect();
    print_table(["STREAM", "ID", "EPOCH", "ENTRIES", "SIZE", "UPDATED (UTC)"], &rows, &[3, 4]);
    Ok(())
}

fn list_entries(store: &Store, space: &SpaceName, name: &StreamName) -> Result<()> {
    let Some(entries) = store.entries(space, name).context("cannot list the entries")? else {
        bail!("there is no stream {space}/{name}");
    };
    let rows: Vec<[String; 4]> =
        entries.iter().map(|e| [e.seq.to_string(), e.epoch.to_string(), size(e.size), e.hash.short()]).collect();
    print_table(["SEQ", "EPOCH", "SIZE", "HASH"], &rows, &[0, 2]);
    Ok(())
}
