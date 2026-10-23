use super::gen5::Gen5Title;

pub mod map;

/// Edition boundary for Black 2 / White 2 specific behavior.
/// Shared Gen V rules belong in `gen5`; field data and behavior that differs
/// between the two sequels will be introduced here as the runtime grows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edition {
    Black2,
    White2,
}

impl Edition {
    pub const fn from_title(title: Gen5Title) -> Option<Self> {
        match title {
            Gen5Title::Black2 => Some(Self::Black2),
            Gen5Title::White2 => Some(Self::White2),
            Gen5Title::Black | Gen5Title::White => None,
        }
    }

    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Black2 => "Pokémon Black Version 2",
            Self::White2 => "Pokémon White Version 2",
        }
    }
}

/// White 2 is the first playable target in the planned development sequence.
pub const INITIAL_EDITION: Edition = Edition::White2;
