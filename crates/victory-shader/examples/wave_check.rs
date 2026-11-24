//! Translates saved console shader containers (`VICTORY_SHADER_SAVE`'s `.cdxil` files) as for a
//! host of 32-lane waves and prints, for each, whether it translated and the wave size the host
//! shader asks for (a 32-lane host refuses `WaveSize(64)`). With `VICTORY_WAVE_WHY=1` the
//! translator says why a program keeps 64-lane waves.
//!
//! `cargo run -p victory-shader --example wave_check -- [--wide] <file.cdxil>...` (`--wide`:
//! each with the wave-wide form allowed, as `VICTORY_WAVE_WIDE_ONLY` allows it per shader).

fn wave_of(dxil: &[u8]) -> Option<(u32, u32)> {
    // DXBC: magic, 16-byte hash, version, size, part count, then part offsets; a part is its
    // fourcc, its size and its data. PSV0's data: the runtime info's size, then the runtime
    // info, whose dwords 4 and 5 are the least and most wave lanes expected.
    let n = u32::from_le_bytes(dxil.get(28..32)?.try_into().ok()?) as usize;
    for k in 0..n {
        let at = 32 + 4 * k;
        let offset = u32::from_le_bytes(dxil.get(at..at + 4)?.try_into().ok()?) as usize;
        if dxil.get(offset..offset + 4)? == b"PSV0" {
            let data = offset + 8;
            let word = |i: usize| -> Option<u32> {
                let at = data + 4 + 4 * i;
                Some(u32::from_le_bytes(dxil.get(at..at + 4)?.try_into().ok()?))
            };
            return Some((word(4)?, word(5)?));
        }
    }
    None
}

fn main() {
    victory_shader::emit::set_host_max_wave(32);
    let wide = std::env::args().any(|a| a == "--wide");
    for path in std::env::args().skip(1).filter(|a| a != "--wide") {
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                println!("{path}: unreadable ({e})");
                continue;
            }
        };
        let done = if wide {
            victory_shader::emit::with_wave_wide(|| victory_shader::emit::translate(&bytes))
        } else {
            victory_shader::emit::translate(&bytes)
        };
        match done {
            Ok(shader) => match wave_of(&shader.dxil) {
                Some((min, max)) if min == max => println!("{path}: translated, wave {min}"),
                Some(_) => println!("{path}: translated, any wave"),
                None => println!("{path}: translated, no PSV0 found"),
            },
            Err(e) => println!("{path}: does not translate: {e}"),
        }
    }
}
