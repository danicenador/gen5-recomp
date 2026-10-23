/// Recognized retail Gen V titles. These identifiers remain separate from NDS
/// parsing so shared container code can later support other Nintendo DS games.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gen5Title {
    Black,
    White,
    Black2,
    White2,
}

impl Gen5Title {
    pub const fn cache_key(self) -> &'static str {
        match self {
            Self::Black => "pokemon-black",
            Self::White => "pokemon-white",
            Self::Black2 => "pokemon-black-2",
            Self::White2 => "pokemon-white-2",
        }
    }

    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Black => "Pokémon Black Version",
            Self::White => "Pokémon White Version",
            Self::Black2 => "Pokémon Black Version 2",
            Self::White2 => "Pokémon White Version 2",
        }
    }

    pub const fn generation(self) -> u8 {
        5
    }
}

/// Identify the Gen V game family from the first three game-code bytes.
/// The final byte is the region/revision designator and is retained by the ROM
/// header without restricting the importer to one territory.
pub fn identify(game_code: &[u8; 4]) -> Option<Gen5Title> {
    match &game_code[..3] {
        b"IRB" => Some(Gen5Title::Black),
        b"IRA" => Some(Gen5Title::White),
        b"IRE" => Some(Gen5Title::Black2),
        b"IRD" => Some(Gen5Title::White2),
        _ => None,
    }
}
