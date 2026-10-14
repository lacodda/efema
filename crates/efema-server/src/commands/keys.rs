use anyhow::{Context, Result};
use efema_server::store::Store;

use crate::cli::{KeysAction, KeysArgs};
use crate::commands::table::{print_table, timestamp};

/// Lists, hands out and revokes access keys. Works on a running relay as on
/// a stopped one: a key handed out is good from the next request, a revoked
/// one opens nothing from the next request.
pub fn run(args: KeysArgs) -> Result<()> {
    match args.action {
        None => {
            let dir = args.data.dir()?;
            let store =
                Store::open_read_only(&dir).with_context(|| format!("cannot read the relay in {}", dir.display()))?;
            list(&store)
        }
        Some(KeysAction::Add { space, access, label, data }) => {
            let dir = data.dir()?;
            // The first key is often handed out before the relay ever ran:
            // the directory is made here as `serve` would make it.
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("cannot create the data directory {}", dir.display()))?;
            let store = Store::open(&dir).with_context(|| format!("cannot open the relay in {}", dir.display()))?;
            let key = store.add_key(&space, access, &label).context("cannot hand out a key")?;
            // The key alone on standard output, for a script to take; the
            // words around it on standard error, for a person.
            let what = if access.writes() { "reads and writes" } else { "reads" };
            eprintln!("Key {} {what} space {space}. This is the only time it is shown:", key.id());
            println!("{}", key.expose());
            Ok(())
        }
        Some(KeysAction::Revoke { id, data }) => {
            let dir = data.dir()?;
            let store = Store::open(&dir).with_context(|| format!("cannot open the relay in {}", dir.display()))?;
            let revoked = store.revoke(&id)?;
            let label = if revoked.label.is_empty() { String::new() } else { format!(" ({})", revoked.label) };
            println!("Revoked key {}{label} to space {}.", revoked.id, revoked.space);
            Ok(())
        }
    }
}

fn list(store: &Store) -> Result<()> {
    let keys = store.keys().context("cannot list the keys")?;
    if keys.is_empty() {
        println!("No keys yet: hand one out with `efema-server keys add <space> <read|write>`.");
        return Ok(());
    }
    let rows: Vec<[String; 6]> = keys
        .iter()
        .map(|k| {
            [
                k.id.clone(),
                k.space.clone(),
                k.access.to_string(),
                k.label.clone(),
                timestamp(k.created_at),
                k.used_at.map_or_else(|| "never".to_string(), timestamp),
            ]
        })
        .collect();
    print_table(["ID", "SPACE", "ACCESS", "LABEL", "CREATED (UTC)", "LAST USED (UTC)"], &rows, &[]);
    Ok(())
}
