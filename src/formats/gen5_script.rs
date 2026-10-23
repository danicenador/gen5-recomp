use crate::Result;

/// Resolve a script number in a Generation V script bank to its absolute byte
/// offset. Script headers store offsets relative to the end of each pointer.
pub fn script_offset(bank: &[u8], script_id: u16) -> Result<usize> {
    let mut cursor = 0usize;
    let mut offsets = Vec::new();
    loop {
        let pointer_end = cursor
            .checked_add(4)
            .ok_or_else(|| "script header offset overflowed".to_owned())?;
        if pointer_end + 2 > bank.len() {
            return Err("script bank has no complete header terminator".to_owned());
        }
        let relative = u32_at(bank, cursor)?;
        let target = pointer_end
            .checked_add(
                usize::try_from(relative)
                    .map_err(|_| "script relative offset does not fit this platform".to_owned())?,
            )
            .ok_or_else(|| "script target offset overflowed".to_owned())?;
        if target >= bank.len() {
            return Err(format!(
                "script header entry {} points outside its {} byte bank",
                offsets.len(),
                bank.len()
            ));
        }
        offsets.push(target);
        if u16_at(bank, pointer_end)? == 0xFD13 {
            break;
        }
        cursor = pointer_end;
    }
    offsets.get(usize::from(script_id)).copied().ok_or_else(|| {
        format!(
            "script {script_id} is outside the {} entry bank",
            offsets.len()
        )
    })
}

fn u16_at(bytes: &[u8], offset: usize) -> Result<u16> {
    let pair = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| "script field extends past the bank".to_owned())?;
    Ok(u16::from_le_bytes([pair[0], pair[1]]))
}

fn u32_at(bytes: &[u8], offset: usize) -> Result<u32> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| "script pointer extends past the bank".to_owned())?;
    Ok(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}
