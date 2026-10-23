use crate::Result;
use crate::game::bw2::map::{ASPERTIA_CITY_ZONE, ZoneMap};
use crate::game::bw2::terrain::{TerrainLayer, TerrainTile, load_zone_terrain};
use std::io::{self, Write};
use std::path::Path;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveResult {
    Moved,
    BlockedByEdge,
    BlockedByTerrain(TerrainTile),
}

pub struct FieldSession {
    pub zone: ZoneMap,
    pub terrain: TerrainLayer,
    pub player: Player,
}

impl FieldSession {
    pub fn load(cache_root: &Path, zone: ZoneMap, start: Option<(usize, usize)>) -> Result<Self> {
        if zone.zone_id != ASPERTIA_CITY_ZONE {
            return Err(format!(
                "field movement currently has a terrain permission table for Aspertia City (zone {ASPERTIA_CITY_ZONE}); got zone {}",
                zone.zone_id
            ));
        }
        let terrain = load_zone_terrain(cache_root, &zone)?;
        let (x, z) = match start {
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
                (x, z)
            }
            None => find_spawn(&terrain)
                .ok_or_else(|| "terrain contains no recognized walkable tiles".to_owned())?,
        };
        Ok(Self {
            zone,
            terrain,
            player: Player {
                x,
                z,
                facing: Direction::South,
                steps: 0,
            },
        })
    }

    pub fn move_player(&mut self, direction: Direction) -> MoveResult {
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
        if !tile.is_walkable() {
            return MoveResult::BlockedByTerrain(tile);
        }
        self.player.x = x;
        self.player.z = z;
        self.player.steps += 1;
        MoveResult::Moved
    }
}

pub fn run_terminal(cache_root: &Path, zone: ZoneMap, start: Option<(usize, usize)>) -> Result<()> {
    let mut session = FieldSession::load(cache_root, zone, start)?;
    println!("{} — native field movement", session.zone.display_name());
    println!(
        "Terrain: {}×{} | recognized ground: 0x{:08X}",
        session.terrain.width,
        session.terrain.height,
        crate::game::bw2::terrain::ASPERTIA_WALKABLE_TYPE
    );
    println!("Move with W/A/S/D and Enter; Q quits. Coordinates start at (0, 0).\n");
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
        let mut had_move = false;
        for key in command.trim().chars() {
            let direction = match key.to_ascii_lowercase() {
                'w' => Some(Direction::North),
                'a' => Some(Direction::West),
                's' => Some(Direction::South),
                'd' => Some(Direction::East),
                'q' => {
                    quit = true;
                    break;
                }
                _ => None,
            };
            let Some(direction) = direction else {
                continue;
            };
            had_move = true;
            match session.move_player(direction) {
                MoveResult::Moved => {
                    println!("Moved to ({}, {}).", session.player.x, session.player.z)
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
            }
        }
        if quit {
            break;
        }
        if !had_move {
            println!("Use W/A/S/D to move or Q to quit.");
        }
        print_map(&session);
    }
    println!(
        "Field session ended after {} successful steps at ({}, {}).",
        session.player.steps, session.player.x, session.player.z
    );
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
