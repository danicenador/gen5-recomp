use crate::Result;
use crate::formats::narc::NarcArchive;
use crate::game::bw2::events::{self, FieldNpc, FieldWarp};
use crate::game::bw2::map::{self, ZoneMap};
use crate::game::bw2::terrain::{TerrainLayer, TerrainTile, load_zone_terrain};
use crate::game::gen5::script_vm::{FieldScriptState, FieldScriptVm};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const SCRIPT_ARCHIVE: &str = "a/0/5/6";
const MESSAGE_ARCHIVE: &str = "a/0/0/3";
const ASPERTIA_OUTLOOK_SCRIPT: u16 = 11;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Direction {
    North,
    East,
    South,
    West,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Player {
    pub x: usize,
    pub z: usize,
    pub facing: Direction,
    pub steps: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoveResult {
    Moved,
    Transitioned { from_zone: u32, to_zone: u32 },
    TransitionFailed(String),
    BlockedByEdge,
    BlockedByTerrain(TerrainTile),
    BlockedByScript,
}

pub struct FieldSession {
    pub zone: ZoneMap,
    pub terrain: TerrainLayer,
    pub player: Player,
    pub npcs: Vec<FieldNpc>,
    warps: Vec<FieldWarp>,
    cache_root: PathBuf,
    script_bank: Vec<u8>,
    message_bank: Vec<u8>,
    script_state: FieldScriptState,
    script_vm: Option<FieldScriptVm>,
}

impl FieldSession {
    pub fn load(cache_root: &Path, zone: ZoneMap, start: Option<(usize, usize)>) -> Result<Self> {
        let terrain = load_zone_terrain(cache_root, &zone)?;
        let npcs = events::load_zone_npcs(cache_root, zone.event_bank)?;
        let warps = events::load_zone_warps(cache_root, zone.event_bank)?;
        let script_bank = load_archive_member(cache_root, SCRIPT_ARCHIVE, zone.scripts_bank)?;
        let message_bank = load_archive_member(cache_root, MESSAGE_ARCHIVE, zone.text_bank)?;
        let (x, z, facing) = match start {
            Some((x, z)) => {
                let tile = terrain
                    .tile(x, z)
                    .ok_or_else(|| format!("start position ({x}, {z}) is outside the terrain"))?;
                if !tile.is_walkable() {
                    return Err(format!(
                        "start position ({x}, {z}) is blocked by terrain type 0x{:08X}",
                        tile.tile_type
                    ));
                }
                (x, z, Direction::South)
            }
            None => preferred_dialogue_spawn(zone.zone_id, &terrain, &npcs)
                .or_else(|| find_spawn(&terrain).map(|(x, z)| (x, z, Direction::South)))
                .ok_or_else(|| "terrain contains no recognized walkable tiles".to_owned())?,
        };
        Ok(Self {
            zone,
            terrain,
            npcs,
            warps,
            cache_root: cache_root.to_path_buf(),
            script_bank,
            message_bank,
            script_state: FieldScriptState::default(),
            script_vm: None,
            player: Player {
                x,
                z,
                facing,
                steps: 0,
            },
        })
    }

    pub fn move_player(&mut self, direction: Direction) -> MoveResult {
        if self.script_vm.is_some() {
            return MoveResult::BlockedByScript;
        }
        self.player.facing = direction;
        let (dx, dz) = match direction {
            Direction::North => (0isize, -1isize),
            Direction::East => (1, 0),
            Direction::South => (0, 1),
            Direction::West => (-1, 0),
        };
        let next_x = self.player.x.checked_add_signed(dx);
        let next_z = self.player.z.checked_add_signed(dz);
        let (Some(x), Some(z)) = (next_x, next_z) else {
            return MoveResult::BlockedByEdge;
        };
        let Some(tile) = self.terrain.tile(x, z) else {
            return MoveResult::BlockedByEdge;
        };
        // Door and map-exit tiles use a dedicated movement code that can look
        // blocked to ordinary walking. Let the zone's warp event claim the
        // tile before applying the regular terrain collision rule.
        if let Some(warp) = self
            .warps
            .iter()
            .copied()
            .find(|warp| warp.destination_zone.is_some() && warp.contains_tile(x, z))
        {
            let from_zone = self.zone.zone_id;
            return match self.transition_through(warp, direction) {
                Ok(to_zone) => MoveResult::Transitioned { from_zone, to_zone },
                Err(error) => MoveResult::TransitionFailed(error),
            };
        }
        if !tile.is_walkable() {
            return MoveResult::BlockedByTerrain(tile);
        }
        self.player.x = x;
        self.player.z = z;
        self.player.steps += 1;
        MoveResult::Moved
    }

    fn transition_through(&mut self, source: FieldWarp, direction: Direction) -> Result<u32> {
        let destination_zone_id = u32::from(
            source
                .destination_zone
                .ok_or_else(|| "warp has no fixed destination zone".to_owned())?,
        );
        let destination_zone = map::load_zone(&self.cache_root, destination_zone_id)?;
        let destination_warps =
            events::load_zone_warps(&self.cache_root, destination_zone.event_bank)?;
        let destination_warp = destination_warps
            .iter()
            .copied()
            .find(|warp| warp.id == source.destination_warp)
            .ok_or_else(|| {
                format!(
                    "zone {destination_zone_id} has no warp {}",
                    source.destination_warp
                )
            })?;
        let arrival = destination_warp.arrival_tile();
        let terrain = load_zone_terrain(&self.cache_root, &destination_zone)?;
        let (x, z) = nearest_walkable_spawn(&terrain, arrival).ok_or_else(|| {
            format!(
                "destination warp {} in zone {destination_zone_id} has no nearby walkable tile",
                source.destination_warp
            )
        })?;
        let mut destination = Self::load(&self.cache_root, destination_zone, Some((x, z)))?;
        destination.player.facing = direction;
        destination.player.steps = self.player.steps + 1;
        destination.script_state = self.script_state.clone();
        *self = destination;
        Ok(destination_zone_id)
    }

    /// Execute the script attached to the NPC in front of the player, or
    /// advance the active script when it is waiting for a button press.
    pub fn press_action(&mut self) -> Result<()> {
        if let Some(vm) = &mut self.script_vm {
            let result = vm.press_action();
            self.script_state = vm.state().clone();
            result?;
            if vm.is_ended() {
                self.script_vm = None;
            }
            return Ok(());
        }

        let (x, z) = match self.player.facing {
            Direction::North => (self.player.x, self.player.z.saturating_sub(1)),
            Direction::East => (self.player.x.saturating_add(1), self.player.z),
            Direction::South => (self.player.x, self.player.z.saturating_add(1)),
            Direction::West => (self.player.x.saturating_sub(1), self.player.z),
        };
        let Some(npc) = self.npcs.iter().find(|npc| npc.x == x && npc.z == z) else {
            return Ok(());
        };
        let vm = FieldScriptVm::start(
            &self.script_bank,
            npc.script_id,
            &self.message_bank,
            &self.script_state,
        )?;
        self.script_state = vm.state().clone();
        self.script_vm = (!vm.is_ended()).then_some(vm);
        Ok(())
    }

    pub fn dialogue(&self) -> Option<&str> {
        self.script_vm.as_ref().and_then(FieldScriptVm::dialogue)
    }
}

fn load_archive_member(cache_root: &Path, archive_path: &str, index: u16) -> Result<Vec<u8>> {
    let romfs = if cache_root.join("romfs").is_dir() {
        cache_root.join("romfs")
    } else {
        cache_root.to_path_buf()
    };
    let bytes = fs::read(romfs.join(archive_path)).map_err(|error| {
        format!(
            "could not read archive '{}' from the ROM cache: {error}",
            archive_path
        )
    })?;
    let archive = NarcArchive::parse(&bytes)?;
    Ok(archive.member(usize::from(index))?.to_vec())
}

fn preferred_dialogue_spawn(
    zone_id: u32,
    terrain: &TerrainLayer,
    npcs: &[FieldNpc],
) -> Option<(usize, usize, Direction)> {
    if zone_id != crate::game::bw2::map::ASPERTIA_CITY_ZONE {
        return None;
    }
    let npc = npcs
        .iter()
        .find(|npc| npc.script_id == ASPERTIA_OUTLOOK_SCRIPT)?;
    [
        (npc.x, npc.z.saturating_add(1), Direction::North),
        (npc.x.saturating_sub(1), npc.z, Direction::East),
        (npc.x.saturating_add(1), npc.z, Direction::West),
        (npc.x, npc.z.saturating_sub(1), Direction::South),
    ]
    .into_iter()
    .find(|(x, z, _)| terrain.tile(*x, *z).is_some_and(TerrainTile::is_walkable))
}

pub fn run_terminal(cache_root: &Path, zone: ZoneMap, start: Option<(usize, usize)>) -> Result<()> {
    let mut session = FieldSession::load(cache_root, zone, start)?;
    println!("{} — native field movement", session.zone.display_name());
    println!(
        "Terrain: {}×{} | movement codes: 0, 2, 3, 4, 6, 10, 14",
        session.terrain.width, session.terrain.height
    );
    println!(
        "Move with W/A/S/D; Enter or E talks/continues; Q quits. Coordinates start at (0, 0).\n"
    );
    print_map(&session);

    let stdin = io::stdin();
    loop {
        print!("move> ");
        io::stdout()
            .flush()
            .map_err(|error| format!("could not write movement prompt: {error}"))?;
        let mut command = String::new();
        if stdin
            .read_line(&mut command)
            .map_err(|error| format!("could not read movement input: {error}"))?
            == 0
        {
            break;
        }
        let mut quit = false;
        let mut had_input = false;
        let trimmed = command.trim();
        if trimmed.is_empty() {
            run_terminal_action(&mut session)?;
            had_input = true;
        }
        for key in trimmed.chars() {
            let direction = match key.to_ascii_lowercase() {
                'w' => Some(Direction::North),
                'a' => Some(Direction::West),
                's' => Some(Direction::South),
                'd' => Some(Direction::East),
                'q' => {
                    quit = true;
                    break;
                }
                'e' => {
                    run_terminal_action(&mut session)?;
                    had_input = true;
                    None
                }
                _ => None,
            };
            let Some(direction) = direction else {
                continue;
            };
            had_input = true;
            match session.move_player(direction) {
                MoveResult::Moved => {
                    println!("Moved to ({}, {}).", session.player.x, session.player.z)
                }
                MoveResult::Transitioned { from_zone, to_zone } => println!(
                    "Transitioned from zone {from_zone} to {} ({}), at ({}, {}).",
                    to_zone,
                    session.zone.display_name(),
                    session.player.x,
                    session.player.z
                ),
                MoveResult::TransitionFailed(error) => {
                    eprintln!("Could not transition through warp: {error}")
                }
                MoveResult::BlockedByEdge => println!("Blocked by map edge."),
                MoveResult::BlockedByTerrain(tile) => println!(
                    "Blocked by terrain type 0x{:08X} at ({}, {}).",
                    tile.tile_type,
                    match direction {
                        Direction::East => session.player.x + 1,
                        Direction::West => session.player.x.saturating_sub(1),
                        _ => session.player.x,
                    },
                    match direction {
                        Direction::South => session.player.z + 1,
                        Direction::North => session.player.z.saturating_sub(1),
                        _ => session.player.z,
                    },
                ),
                MoveResult::BlockedByScript => println!("A field script is active."),
            }
        }
        if quit {
            break;
        }
        if !had_input {
            println!("Use W/A/S/D to move, Enter or E to talk, or Q to quit.");
        }
        print_map(&session);
    }
    println!(
        "Field session ended after {} successful steps at ({}, {}).",
        session.player.steps, session.player.x, session.player.z
    );
    Ok(())
}

fn run_terminal_action(session: &mut FieldSession) -> Result<()> {
    let had_dialogue = session.dialogue().is_some();
    session.press_action()?;
    match session.dialogue() {
        Some(dialogue) => println!("\n{dialogue}\n\nPress Enter or E to continue."),
        None if had_dialogue => println!("Dialogue closed."),
        None => println!("No nearby NPC has an action script."),
    }
    Ok(())
}

fn find_spawn(terrain: &TerrainLayer) -> Option<(usize, usize)> {
    let center_x = usize::from(terrain.width) / 2;
    let center_z = usize::from(terrain.height) / 2;
    (0..usize::from(terrain.height))
        .flat_map(|z| (0..usize::from(terrain.width)).map(move |x| (x, z)))
        .filter(|(x, z)| terrain.tile(*x, *z).is_some_and(TerrainTile::is_walkable))
        .min_by_key(|(x, z)| x.abs_diff(center_x) + z.abs_diff(center_z))
}

fn nearest_walkable_spawn(
    terrain: &TerrainLayer,
    origin: (usize, usize),
) -> Option<(usize, usize)> {
    let (origin_x, origin_z) = origin;
    let radius = 8usize;
    let min_x = origin_x.saturating_sub(radius);
    let min_z = origin_z.saturating_sub(radius);
    let max_x = origin_x
        .saturating_add(radius + 1)
        .min(usize::from(terrain.width));
    let max_z = origin_z
        .saturating_add(radius + 1)
        .min(usize::from(terrain.height));
    (min_z..max_z)
        .flat_map(|z| (min_x..max_x).map(move |x| (x, z)))
        .filter(|(x, z)| terrain.tile(*x, *z).is_some_and(TerrainTile::is_walkable))
        .min_by_key(|(x, z)| x.abs_diff(origin_x) + z.abs_diff(origin_z))
}

fn print_map(session: &FieldSession) {
    for z in 0..usize::from(session.terrain.height) {
        for x in 0..usize::from(session.terrain.width) {
            if (session.player.x, session.player.z) == (x, z) {
                print!("@");
            } else if session
                .terrain
                .tile(x, z)
                .is_some_and(TerrainTile::is_walkable)
            {
                print!(".");
            } else {
                print!("#");
            }
        }
        println!();
    }
    println!(
        "position ({}, {}), facing {:?}, steps {}",
        session.player.x, session.player.z, session.player.facing, session.player.steps
    );
}
