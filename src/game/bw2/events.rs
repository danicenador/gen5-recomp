use crate::Result;
use crate::formats::narc::NarcArchive;
use std::fs;
use std::path::Path;

const EVENT_ARCHIVE: &str = "a/1/2/6";
const NPC_RECORD_SIZE: usize = 36;
const BG_RECORD_SIZE: usize = 20;
const WARP_RECORD_SIZE: usize = 20;
const TRIGGER_RECORD_SIZE: usize = 22;

#[derive(Debug, Clone, Copy)]
pub struct FieldNpc {
    pub id: u16,
    pub model: u16,
    pub script_id: u16,
    pub x: usize,
    pub z: usize,
    pub elevation: i32,
    pub width: u16,
    pub height: u16,
}

pub fn load_zone_npcs(cache_root: &Path, event_bank: u16) -> Result<Vec<FieldNpc>> {
    let romfs = if cache_root.join("romfs").is_dir() {
        cache_root.join("romfs")
    } else {
        cache_root.to_path_buf()
    };
    let bytes = fs::read(romfs.join(EVENT_ARCHIVE))
        .map_err(|error| format!("could not read BW2 zone-event archive: {error}"))?;
    let archive = NarcArchive::parse(&bytes)?;
    let event = archive.member(usize::from(event_bank))?;
    parse_zone_npcs(event)
}

fn parse_zone_npcs(bytes: &[u8]) -> Result<Vec<FieldNpc>> {
    if bytes.len() < 8 {
        return Err("BW2 zone-event entry is shorter than its header".to_owned());
    }
    let init_offset = usize::try_from(u32_at(bytes, 0)?)
        .map_err(|_| "zone-event init offset does not fit this platform".to_owned())?;
    let bg_count = usize::from(bytes[4]);
    let npc_count = usize::from(bytes[5]);
    let warp_count = usize::from(bytes[6]);
    let trigger_count = usize::from(bytes[7]);
    let bg_bytes = bg_count
        .checked_mul(BG_RECORD_SIZE)
        .ok_or_else(|| "zone-event background record size overflowed".to_owned())?;
    let npc_bytes = npc_count
        .checked_mul(NPC_RECORD_SIZE)
        .ok_or_else(|| "zone-event NPC record size overflowed".to_owned())?;
    let warp_bytes = warp_count
        .checked_mul(WARP_RECORD_SIZE)
        .ok_or_else(|| "zone-event warp record size overflowed".to_owned())?;
    let trigger_bytes = trigger_count
        .checked_mul(TRIGGER_RECORD_SIZE)
        .ok_or_else(|| "zone-event trigger record size overflowed".to_owned())?;
    let records_size = bg_bytes
        .checked_add(npc_bytes)
        .and_then(|size| size.checked_add(warp_bytes))
        .and_then(|size| size.checked_add(trigger_bytes))
        .ok_or_else(|| "zone-event record size overflowed".to_owned())?;
    let records_end = 8usize
        .checked_add(records_size)
        .ok_or_else(|| "zone-event record end overflowed".to_owned())?;
    let expected_init_offset = 4usize
        .checked_add(records_size)
        .ok_or_else(|| "zone-event init offset overflowed".to_owned())?;
    if init_offset != expected_init_offset || records_end > bytes.len() {
        return Err(format!(
            "zone-event records end at 0x{records_end:X}, init offset is 0x{init_offset:X}"
        ));
    }

    let npc_start = 8 + bg_bytes;
    let mut npcs = Vec::with_capacity(npc_count);
    for index in 0..npc_count {
        let start = npc_start + index * NPC_RECORD_SIZE;
        let id = u16_at(bytes, start)?;
        let model = u16_at(bytes, start + 2)?;
        let script_id = u16_at(bytes, start + 10)?;
        let width = u16_at(bytes, start + 20)?;
        let height = u16_at(bytes, start + 22)?;
        let is_rail = u32_at(bytes, start + 24)?;
        if is_rail != 0 {
            continue;
        }
        let x = usize::from(u16_at(bytes, start + 28)?);
        let z = usize::from(u16_at(bytes, start + 30)?);
        let elevation = i32_at(bytes, start + 32)?;
        npcs.push(FieldNpc {
            id,
            model,
            script_id,
            x,
            z,
            elevation,
            width,
            height,
        });
    }
    Ok(npcs)
}

fn u16_at(bytes: &[u8], offset: usize) -> Result<u16> {
    let value = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| "zone-event 16-bit field extends past its entry".to_owned())?;
    Ok(u16::from_le_bytes([value[0], value[1]]))
}

fn u32_at(bytes: &[u8], offset: usize) -> Result<u32> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| "zone-event 32-bit field extends past its entry".to_owned())?;
    Ok(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

fn i32_at(bytes: &[u8], offset: usize) -> Result<i32> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| "zone-event signed field extends past its entry".to_owned())?;
    Ok(i32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}
