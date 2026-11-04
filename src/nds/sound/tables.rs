//! Lookup tables of the NitroSDK sound driver and the ARM7 BIOS.
//!
//! The driver's own tables are the ones in White 2's ARM7 binary (checked
//! byte for byte). The BIOS pitch and volume tables are computed from the
//! curves they sample, which matches the BIOS to within a step.

use std::sync::OnceLock;

/// Order in which the driver tries channels for a new note.
pub const CHANNEL_ALLOCATION_ORDER: [u8; 16] = [4, 5, 6, 7, 2, 0, 3, 1, 8, 9, 10, 11, 14, 12, 15, 13];

/// Attack rates 109..=127 (index `127 - attack`).
pub const ATTACK_COEFFICIENTS: [u8; 19] = [
    0, 1, 5, 14, 26, 38, 51, 63, 73, 84, 92, 100, 109, 116, 123, 127, 132, 137, 143,
];

/// Right shift of each channel volume divider setting.
pub const VOLUME_SHIFT: [u8; 4] = [0, 1, 2, 4];

/// A quarter sine wave for the LFO, 0..=127.
pub const LFO_SINE: [i8; 33] = [
    0, 6, 12, 19, 25, 31, 37, 43, 49, 54, 60, 65, 71, 76, 81, 85, 90, 94, 98, 102, 106, 109, 112,
    115, 117, 120, 122, 123, 125, 126, 126, 127, 127,
];

/// Attenuation (in 1/10 dB) of a 0..=127 volume, squared.
pub const DECIBEL_SQUARE: [i16; 128] = [
    -32768, -722, -721, -651, -601, -562, -530, -503, -480, -460, -442, -425, -410, -396, -383,
    -371, -360, -349, -339, -330, -321, -313, -305, -297, -289, -282, -276, -269, -263, -257,
    -251, -245, -239, -234, -229, -224, -219, -214, -210, -205, -201, -196, -192, -188, -184,
    -180, -176, -173, -169, -165, -162, -158, -155, -152, -149, -145, -142, -139, -136, -133,
    -130, -127, -125, -122, -119, -116, -114, -111, -109, -106, -103, -101, -99, -96, -94, -91,
    -89, -87, -85, -82, -80, -78, -76, -74, -72, -70, -68, -66, -64, -62, -60, -58, -56, -54,
    -52, -50, -49, -47, -45, -43, -42, -40, -38, -36, -35, -33, -31, -30, -28, -27, -25, -23,
    -22, -20, -19, -17, -16, -14, -13, -11, -10, -8, -7, -6, -4, -3, -1, 0,
];

/// Lowest channel volume in 1/10 dB.
pub const VOLUME_DB_MIN: i32 = -723;

/// BIOS `GetPitchTable`: (2^(i/768) - 1) * 65536 for one octave.
pub fn pitch_table() -> &'static [u16; 768] {
    static TABLE: OnceLock<[u16; 768]> = OnceLock::new();
    TABLE.get_or_init(|| {
        std::array::from_fn(|index| {
            ((2f64.powf(index as f64 / 768.0) - 1.0) * 65536.0).round() as u16
        })
    })
}

/// BIOS `GetVolumeTable`: the 0..=127 channel volume for an attenuation
/// of `index - 723` tenths of a decibel. Below -6, -12 and -24 dB the
/// driver also selects the channel's /2, /4 and /16 divider, so each range
/// is scaled back up by that factor.
pub fn volume_table() -> &'static [u8; 724] {
    static TABLE: OnceLock<[u8; 724]> = OnceLock::new();
    TABLE.get_or_init(|| {
        std::array::from_fn(|index| {
            let decibels = index as i32 + VOLUME_DB_MIN;
            let boost = if decibels < -240 {
                16.0
            } else if decibels < -120 {
                4.0
            } else if decibels < -60 {
                2.0
            } else {
                1.0
            };
            let amplitude = 10f64.powf(f64::from(decibels) / 200.0) * boost;
            (amplitude * 127.0).round().min(127.0) as u8
        })
    })
}

/// IMA-ADPCM step sizes.
pub const ADPCM_STEPS: [u16; 89] = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45, 50, 55, 60, 66,
    73, 80, 88, 97, 107, 118, 130, 143, 157, 173, 190, 209, 230, 253, 279, 307, 337, 371, 408,
    449, 494, 544, 598, 658, 724, 796, 876, 963, 1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066,
    2272, 2499, 2749, 3024, 3327, 3660, 4026, 4428, 4871, 5358, 5894, 6484, 7132, 7845, 8630,
    9493, 10442, 11487, 12635, 13899, 15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794,
    32767,
];

/// IMA-ADPCM step index change for the low three bits of a nibble.
pub const ADPCM_INDEX: [i8; 8] = [-1, -1, -1, -1, 2, 4, 6, 8];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bios_tables_have_their_endpoints() {
        assert_eq!(pitch_table()[0], 0);
        assert_eq!(pitch_table()[1], 59);
        assert_eq!(volume_table()[723], 127);
        assert_eq!(volume_table()[0], 0);
        // Just below -6 dB the /2 divider takes over at full scale again.
        assert!(volume_table()[(723 - 61) as usize] > 120);
    }
}
