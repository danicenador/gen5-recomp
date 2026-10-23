use crate::Result;

/// Decode the English message bank format used by Gen V's text NARCs.
/// Text and control codes remain data; callers decide how to present variables.
pub fn parse_message_bank(bytes: &[u8]) -> Result<Vec<String>> {
    require(bytes, 0, 16, "message bank header")?;
    let section_count = usize::from(u16_at(bytes, 0)?);
    let line_count = usize::from(u16_at(bytes, 2)?);
    if section_count == 0 || line_count > 0x8000 {
        return Err(format!(
            "message bank has invalid section/line counts ({section_count}/{line_count})"
        ));
    }
    let section = usize::try_from(u32_at(bytes, 12)?)
        .map_err(|_| "message section offset does not fit this platform".to_owned())?;
    require(bytes, section, 4, "message section header")?;
    let descriptors_size = line_count
        .checked_mul(8)
        .ok_or_else(|| "message descriptor table size overflowed".to_owned())?;
    require(bytes, section + 4, descriptors_size, "message descriptors")?;

    let mut messages = Vec::with_capacity(line_count);
    for index in 0..line_count {
        let descriptor = section + 4 + index * 8;
        let relative = usize::try_from(u32_at(bytes, descriptor)?)
            .map_err(|_| "message offset does not fit this platform".to_owned())?;
        let word_count = usize::from(u16_at(bytes, descriptor + 4)?);
        let data_start = section
            .checked_add(relative)
            .ok_or_else(|| "message data offset overflowed".to_owned())?;
        let data_size = word_count
            .checked_mul(2)
            .ok_or_else(|| "message word count overflowed".to_owned())?;
        require(bytes, data_start, data_size, "encrypted message")?;
        let encrypted = (0..word_count)
            .map(|word| u16_at(bytes, data_start + word * 2))
            .collect::<Result<Vec<_>>>()?;
        messages.push(decode_message(&decrypt(&encrypted))?);
    }
    Ok(messages)
}

fn decrypt(encrypted: &[u16]) -> Vec<u16> {
    if encrypted.is_empty() {
        return Vec::new();
    }
    let mut output = encrypted.to_vec();
    let mut key = output[output.len() - 1] ^ 0xFFFF;
    for word in output.iter_mut().rev() {
        *word ^= key;
        key = key.rotate_right(3);
    }
    output
}

fn decode_message(words: &[u16]) -> Result<String> {
    let words = decompress(words)?;
    let mut output = String::new();
    let mut cursor = 0usize;
    while let Some(&word) = words.get(cursor) {
        cursor += 1;
        match word {
            0xFFFF => break,
            0xFFFE => output.push('\n'),
            0xF000 => {
                // A control packet is F000, function, argument count, then its
                // arguments. Field variables are resolved by the script VM;
                // this slice renders a readable stand-in for the name fields.
                let function = *words
                    .get(cursor)
                    .ok_or_else(|| "message control packet has no function".to_owned())?;
                let argument_count =
                    usize::from(*words.get(cursor + 1).ok_or_else(|| {
                        "message control packet has no argument count".to_owned()
                    })?);
                cursor = cursor
                    .checked_add(2 + argument_count)
                    .ok_or_else(|| "message control packet overflowed".to_owned())?;
                if cursor > words.len() {
                    return Err("message control arguments extend past the message".to_owned());
                }
                match function {
                    0x0100 => output.push_str("Pokémon"),
                    0x0101 => output.push_str("Pokémon"),
                    _ => {}
                }
            }
            0..=0x001F => {}
            _ => {
                if let Some(character) = char::from_u32(u32::from(word)) {
                    // Western BW2 messages occasionally carry nonprinting
                    // glyph data alongside a control packet. Avoid surfacing
                    // those as unrelated Unicode characters in the PC UI.
                    if character.is_ascii_graphic()
                        || character.is_ascii_whitespace()
                        || matches!(
                            character,
                            'é' | '♂' | '♀' | '…' | '・' | '×' | '’' | '“' | '”'
                        )
                    {
                        output.push(character);
                    }
                }
            }
        }
    }
    Ok(output)
}

fn decompress(words: &[u16]) -> Result<Vec<u16>> {
    if words.first() != Some(&0xF100) {
        return Ok(words.to_vec());
    }
    let mut bits = 0u64;
    let mut bit_count = 0u32;
    let mut output = Vec::new();
    for &word in &words[1..] {
        bits |= u64::from(word) << bit_count;
        bit_count += 16;
        while bit_count >= 9 {
            let character = (bits & 0x1FF) as u16;
            if character == 0x1FF {
                output.push(0xFFFF);
                return Ok(output);
            }
            output.push(character);
            bits >>= 9;
            bit_count -= 9;
        }
    }
    output.push(0xFFFF);
    Ok(output)
}

fn require(bytes: &[u8], start: usize, length: usize, what: &str) -> Result<()> {
    let end = start
        .checked_add(length)
        .ok_or_else(|| format!("{what} range overflowed"))?;
    if end > bytes.len() {
        return Err(format!("{what} extends past its containing message file"));
    }
    Ok(())
}

fn u16_at(bytes: &[u8], offset: usize) -> Result<u16> {
    require(bytes, offset, 2, "16-bit message field")?;
    Ok(u16::from_le_bytes([bytes[offset], bytes[offset + 1]]))
}

fn u32_at(bytes: &[u8], offset: usize) -> Result<u32> {
    require(bytes, offset, 4, "32-bit message field")?;
    Ok(u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ]))
}
