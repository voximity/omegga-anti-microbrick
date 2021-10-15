use std::{
    collections::HashSet,
    fs::{File, OpenOptions},
    path::PathBuf,
    time::Duration,
};

use anyhow::Result;
use brickadia::{read::SaveReader, save::SaveData, write::SaveWriter};
use chrono::Utc;
use omegga::{events::Event, resources::Player, Omegga};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub const ASEZ: &str = "autosave_ez";
pub const SAVES_LOC: &str = "../../data/Saved/Builds";
pub const SAVE_LOC: &str = "_anti_microbrick.brs";

const PUBLIC_ID: Uuid = Uuid::from_bytes([0xff; 16]);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AuthPlayer {
    name: String,
    id: String,
}

#[derive(Serialize, Deserialize)]
struct Config {
    authorized: Vec<AuthPlayer>,

    #[serde(rename = "clear-after-minutes")]
    clear_after: f32,

    #[serde(rename = "max-violations")]
    max_violations: u32,

    #[serde(rename = "ban-time")]
    ban_time: f32,

    #[serde(rename = "max-bans")]
    max_bans: u32,
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
            Event::Init { id, .. } => {
                omegga.write_response(
                    id,
                    Some(serde_json::json!({"registeredCommands": ["am"]})),
                    None,
                );

                // when the plugin initializes, connect to asez. we will expect a "connected" request later on
                omegga
                    .emit_plugin::<u8>(ASEZ.into(), "connect".into(), vec![])
                    .await
                    .unwrap();
            }
            Event::Stop { id, .. } => {
                omegga.write_response(id, None, None);
            }
            Event::Command {
                player,
                command,
                args,
            } => {
                if command != "am" {
                    continue;
                }

                if !config
                    .authorized
                    .iter()
                    .any(|p| p.name.eq_ignore_ascii_case(player.as_str()))
                {
                    continue;
                }

                let subcommand = &args[0];
                let players = match omegga.get_players().await {
                    Ok(p) => p,
                    Err(_) => continue,
                };

                match subcommand.as_str() {
                    "clean" => {
                        let target = args.into_iter().skip(1).collect::<String>().to_lowercase();
                        let target = match players
                            .iter()
                            .find(|p| p.name.to_lowercase().starts_with(&target))
                        {
                            Some(p) => p,
                            None => {
                                omegga
                                    .whisper(player, format!("Please specify a player to clean."));
                                continue;
                            }
                        };

                        omegga.store_delete(format!("ts:{}", target.id)).await;
                        omegga
                            .store_delete(format!("violations:{}", target.id))
                            .await;
                        omegga.store_delete(format!("bans:{}", target.id)).await;

                        omegga.whisper(
                            player,
                            format!("Cleared <b>{}</>'s record, if any.", target.name),
                        );
                    }
                    "wipe" => match args.get(1) {
                        Some(s) if s.as_str() == "yes" => {
                            omegga.store_wipe();
                            omegga.whisper(player, "OK, all records wiped.");
                        }
                        _ => {
                            omegga.whisper(player, format!("<b>Are you sure you wish to wipe all records?</> Please run <code>/am wipe yes</> to confirm."));
                        }
                    },
                    x => omegga.whisper(player, format!("Invalid subcommand <code>/am {}</>.", x)),
                }
            }
            Event::PluginEmit {
                id,
                event,
                from,
                args,
            } => match (from.as_str(), event.as_str()) {
                (ASEZ, "save") => {
                    let save_path = args[0].as_str().unwrap();
                    let mut path = PathBuf::from("../..");
                    path.push(save_path);
                    omegga.write_response(id, None, None);
                    if let Err(e) = check_save(&omegga, &config, path).await {
                        omegga.error(format!("failed to check save: {}", e));
                    }
                }
                _ => omegga.write_response(id, None, None),
            },
            _ => (),
        }
    }
}

async fn check_save(omegga: &Omegga, config: &Config, path: PathBuf) -> Result<()> {
    let mut reader = SaveReader::new(File::open(path)?)?;
    let header1 = reader.read_header1()?;
    let header2 = reader.read_header2()?;

    let players = omegga.get_players().await?;

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
    let (mut bricks, components) = reader.read_bricks(&header1, &header2)?;

    let mut micro_owners = HashSet::new();
    let mut cleared_owners = HashSet::new();

    for brick in bricks.iter() {
        let asset = header2.brick_assets[brick.asset_name_index as usize].as_str();
        if asset.contains("Micro") {
            // this is a microbrick! figure out who owns it
            let owner = match brick.owner_index {
                0 => continue,
                n => &header2.brick_owners[n as usize - 1],
            };

            if micro_owners.contains(&owner.id)
                || cleared_owners.contains(&owner.id)
                || owner.id == PUBLIC_ID
            {
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

                    if now >= ts + (config.clear_after * 60.) as u64 {
                        // clear bricks
                        omegga.broadcast(format!(
                            "Clearing <color=\"ff0\">{}</>'s microbricks...",
                            owner.name
                        ));
                        cleared_owners.insert(owner.id);
                    } else {
                        // warn the player
                        micro_owners.insert(owner.id);
                        warn_player(omegga, &players, owner.id);
                    }
                }
                _ => {
                    // set the timestamp and warn
                    let ts = Utc::now().timestamp() as u64;

                    // if the clear_after amount is 0, just immediately clear bricks
                    if config.clear_after == 0. {
                        omegga.broadcast(format!(
                            "Clearing <color=\"ff0\">{}</>'s microbricks...",
                            owner.name
                        ));
                        cleared_owners.insert(owner.id);
                    } else {
                        micro_owners.insert(owner.id);
                        omegga.store_set(format!("ts:{}", owner.id), Value::String(ts.to_string()));
                        warn_player(omegga, &players, owner.id);
                    }
                }
            }
        }
    }

    // clear violator bricks
    for id in cleared_owners.iter() {
        omegga.clear_bricks(id.to_string(), true);

        let key = format!("violations:{}", id);
        let mut violations: i64 = omegga
            .store_get(key.clone())
            .await?
            .map(|v| v.as_i64().unwrap())
            .unwrap_or(0);
        violations += 1;

        omegga.log(format!(
            "Clearing bricks of {} ({} violations)",
            id, violations
        ));

        omegga.store_set(key, violations.into());

        if violations as u32 > config.max_violations {
            // we've hit max violations: start banning the user
            let key = format!("bans:{}", id);
            let mut bans: i64 = omegga
                .store_get(key.clone())
                .await?
                .map(|v| v.as_i64().unwrap())
                .unwrap_or(0);
            bans += 1;

            omegga.store_set(key, bans.into());

            if bans as u32 > config.max_bans {
                // permanently ban
                omegga.writeln(format!(
                    "Chat.Command /Ban {} {} \"Microbricks are not allowed on this server.\"",
                    id, "-1",
                ));
            } else {
                // temporarily ban
                omegga.writeln(format!(
                    "Chat.Command /Ban {} {} \"Microbricks are not allowed on this server. This ban will be permanent in {} more violations.\"",
                    id,
                    config.ban_time,
                    config.max_bans - bans as u32,
                ));
            }
        } else {
            omegga.whisper(
                id.to_string(),
                format!(
                    "<b>You currently have {} microbrick violations. After {}, you will be temporarily banned.</>",
                    violations,
                    config.max_violations
                )
            );
        }
    }

    // now, we should have a list of users whose bricks are cleared
    // filter out bricks that were NOT placed by someone in this microbrick array
    bricks.retain(|b| {
        b.owner_index > 0
            && cleared_owners.contains(&header2.brick_owners[b.owner_index as usize - 1].id)
    });

    // now keep only bricks without "Micro" in their asset name
    bricks.retain(|b| !header2.brick_assets[b.asset_name_index as usize].contains("Micro"));

    // now we've filtered out the bricks, so we can load everything back in as is
    let save_data = SaveData {
        header1,
        header2,
        bricks,
        components,
        ..Default::default()
    };

    SaveWriter::new(
        OpenOptions::new()
            .write(true)
            .create(true)
            .open(format!("{}/{}", SAVES_LOC, SAVE_LOC))?,
        save_data,
    )
    .write()?;

    // artificial delay: we are literally too fast for brickadia
    tokio::time::sleep(Duration::from_secs(1)).await;

    // load it into the game
    omegga.load_bricks(SAVE_LOC, true, (0, 0, 0)).await?;

    // at this point check if there are users with a timestamp that were not found in this scan
    let keys = omegga.store_keys().await?;
    for key in keys.iter().filter_map(|key| key.strip_prefix("ts:")) {
        // if we didn't pick them up,
        let parsed = key.parse()?;
        if cleared_owners.contains(&parsed) || !micro_owners.contains(&parsed) {
            // get em outta here
            omegga.store_delete(format!("ts:{}", key)).await;
        }
    }

    Ok(())
}

fn warn_player(omegga: &Omegga, players: &[Player], target: impl ToString) {
    let target = target.to_string();

    if !players
        .iter()
        .any(|p| p.name.eq_ignore_ascii_case(target.as_str()))
    {
        return;
    }

    omegga.whisper(target, "<size=\"30\"><color=\"a00\">Microbricks are not allowed on this server!</> Please delete your microbricks or <b>they will be cleared</>.</>");
}
