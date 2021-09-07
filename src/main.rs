use std::{collections::HashSet, fs::File, path::PathBuf};

use anyhow::Result;
use brickadia::read::SaveReader;
use chrono::Utc;
use omegga::{rpc, Omegga};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const ASEZ: &'static str = "autosave_ez";

#[derive(Serialize, Deserialize)]
struct Config {
    #[serde(rename = "clear-after-minutes")]
    clear_after: u32,
}

#[tokio::main]
async fn main() {
    let config: Config = serde_json::from_reader(
        File::open("config.json").expect("omegga did not emit a config file"),
    )
    .expect("failed to deserialize plugin config");

    let omegga = Omegga::new();
    let mut rx = omegga.spawn();

    while let Some(message) = rx.recv().await {
        match message {
            rpc::Message::Request {
                method,
                params: _params,
                id,
                ..
            } if method == "init" || method == "stop" => {
                omegga.write_response(id, None, None);

                // when the plugin initializes, connect to asez. we will expect a "connected" request later on
                omegga
                    .emit_plugin::<u8>(ASEZ.into(), "connect".into(), vec![])
                    .await
                    .unwrap();
            }
            rpc::Message::Request {
                method, params, id, ..
            } if method == "plugin:emit" => {
                let params = params.unwrap();
                let mut params = params.as_array().unwrap().into_iter();
                let event = params.next().unwrap().as_str().unwrap();
                let from = params.next().unwrap().as_str().unwrap();

                match (from, event) {
                    (ASEZ, "save") => {
                        let save_path = params.next().unwrap().as_str().unwrap();
                        let mut path = PathBuf::from("../..");
                        path.push(save_path);
                        if let Err(e) = check_save(&omegga, &config, path).await {
                            omegga.error(format!("failed to check save: {}", e));
                        }
                    }
                    _ => omegga.write_response(id, None, None),
                }
            }
            _ => (),
        }
    }
}

async fn check_save(omegga: &Omegga, config: &Config, path: PathBuf) -> Result<()> {
    let mut reader = SaveReader::new(File::open(path)?)?;
    let header1 = reader.read_header1()?;
    let header2 = reader.read_header2()?;

    // expect there to be no microbricks
    if !header2
        .brick_assets
        .iter()
        .any(|asset| asset.contains("Micro"))
    {
        // there are no microbricks! we can safely stop checking this save
        return Ok(());
    }

    // at this point, we know we have microbricks, so let's scan the save for them
    reader.skip_preview()?;
    let (bricks, _) = reader.read_bricks(&header1, &header2)?;

    let mut micro_owners = HashSet::new();
    let mut cleared_owners = HashSet::new();

    for brick in bricks.into_iter() {
        let asset = header2.brick_assets[brick.asset_name_index as usize].as_str();
        if asset.contains("Micro") {
            // this is a microbrick! figure out who owns it
            let owner = match brick.owner_index {
                0 => continue,
                n => &header2.brick_owners[n as usize - 1],
            };

            if micro_owners.contains(&owner.id) || cleared_owners.contains(&owner.id) {
                continue;
            }

            // check if a timestamp has already been set for them
            match omegga.store_get(format!("ts:{}", owner.id)).await? {
                Some(Value::String(s)) => {
                    // check if timer has expired
                    // if it has, clear bricks
                    // otherwise, warn the player
                    let ts: u64 = s.parse()?;
                    let now = Utc::now().timestamp() as u64;

                    if now >= ts + config.clear_after as u64 * 60 {
                        // clear bricks
                        omegga.broadcast(format!("Clearing the <color=\"ff0\">{}</> bricks of <color=\"ff0\">{}</>...", owner.bricks, owner.name));
                        omegga.clear_bricks(owner.id.to_string(), false);
                        cleared_owners.insert(owner.id);
                    } else {
                        // warn the player
                        micro_owners.insert(owner.id);
                        warn_player(omegga, owner.id);
                    }
                }
                _ => {
                    // set the timestamp and warn
                    let ts = Utc::now().timestamp() as u64;

                    // if the clear_after amount is 0, just immediately clear bricks
                    if config.clear_after == 0 {
                        omegga.broadcast(format!("Clearing the <color=\"ff0\">{}</> bricks of <color=\"ff0\">{}</>...", owner.bricks, owner.name));
                        omegga.clear_bricks(owner.id.to_string(), false);
                        cleared_owners.insert(owner.id);
                    } else {
                        micro_owners.insert(owner.id);
                        omegga.store_set(format!("ts:{}", owner.id), Value::String(ts.to_string()));
                        warn_player(omegga, owner.id);
                    }
                }
            }
        }
    }

    // at this point check if there are users with a timestamp that were not found in this scan
    let keys = omegga.store_keys().await?;
    for key in keys.iter().filter_map(|key| key.strip_prefix("ts:")) {
        // if we didn't pick them up,
        if !micro_owners.contains(&key.parse()?) {
            // get em outta here
            omegga.store_delete(format!("ts:{}", key)).await;
        }
    }

    Ok(())
}

fn warn_player(omegga: &Omegga, target: impl ToString) {
    omegga.whisper(target.to_string(), "<color=\"a00\">Microbricks are not allowed on this server!</> Please delete your microbricks or your bricks will be cleared.");
}
