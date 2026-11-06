//! Just enough of an SMBIOS parser to read the type 17 (Memory Device)
//! records: RAM generation, speed and module count. Linux feeds it
//! `/sys/firmware/dmi/tables/DMI` and Windows the table from
//! `GetSystemFirmwareTable('RSMB')`, so the type-code mapping lives in one
//! place.
//!
//! Each structure is a 4-byte header (type, length, handle), the rest of
//! its formatted area, then a string set ending in a double NUL.

#[derive(Debug, Default, PartialEq)]
pub struct MemoryModules {
    pub standard: Option<String>,
    pub speed_mts: Option<u32>,
    pub module_count: u32,
}

const MEMORY_DEVICE: u8 = 17;
const END_OF_TABLE: u8 = 127;

/// Parses a raw structure table (no entry point or Windows header).
pub fn memory_modules(table: &[u8]) -> MemoryModules {
    let mut out = MemoryModules::default();
    for s in structures(table).filter(|s| s[0] == MEMORY_DEVICE) {
        let size = word(s, 0x0C).unwrap_or(0);
        if size == 0 {
            continue; // empty slot
        }
        out.module_count += 1;
        if out.standard.is_none() {
            out.standard = s.get(0x12).and_then(|&code| memory_type_name(code)).map(str::to_string);
        }
        // The configured speed is what the module actually runs at (e.g.
        // DDR5-6000 with EXPO, or LPDDR5 rated 6400 running at 5200); the
        // plain speed field is its rating. 0 means unknown, and 0xFFFF
        // means the real value is in the 32-bit extended field.
        let speed = speed_field(s, 0x20, 0x58).or_else(|| speed_field(s, 0x15, 0x54));
        out.speed_mts = out.speed_mts.max(speed);
    }
    out
}

/// Strips the 8-byte `RawSMBIOSData` header Windows prepends.
pub fn strip_windows_header(raw: &[u8]) -> &[u8] {
    if raw.len() < 8 {
        return &[];
    }
    let len = u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]) as usize;
    &raw[8..raw.len().min(8 + len)]
}

/// Each structure's formatted area (header included).
fn structures(table: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut pos = 0;
    std::iter::from_fn(move || {
        let header = table.get(pos..pos + 4)?;
        let (ty, len) = (header[0], header[1] as usize);
        if len < 4 || ty == END_OF_TABLE {
            return None;
        }
        let formatted = table.get(pos..pos + len)?;
        let strings_end = table[pos + len..].windows(2).position(|w| w == [0, 0])?;
        pos += len + strings_end + 2;
        Some(formatted)
    })
}

fn word(s: &[u8], offset: usize) -> Option<u16> {
    s.get(offset..offset + 2).map(|b| u16::from_le_bytes([b[0], b[1]]))
}

fn dword(s: &[u8], offset: usize) -> Option<u32> {
    s.get(offset..offset + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn speed_field(s: &[u8], offset: usize, extended_offset: usize) -> Option<u32> {
    match word(s, offset)? {
        0 => None,
        0xFFFF => dword(s, extended_offset).filter(|&v| v != 0),
        v => Some(v as u32),
    }
}

/// SMBIOS spec 3.x, table "Memory Device — Type".
fn memory_type_name(code: u8) -> Option<&'static str> {
    Some(match code {
        0x0F => "SDRAM",
        0x12 => "DDR",
        0x13 => "DDR2",
        0x14 => "DDR2 FB-DIMM",
        0x18 => "DDR3",
        0x1A => "DDR4",
        0x1B => "LPDDR",
        0x1C => "LPDDR2",
        0x1D => "LPDDR3",
        0x1E => "LPDDR4",
        0x20 => "HBM",
        0x21 => "HBM2",
        0x22 => "DDR5",
        0x23 => "LPDDR5",
        0x24 => "HBM3",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal SMBIOS 3.x type 17 structure (0x5C bytes) plus one string.
    fn memory_device(size: u16, type_code: u8, speed: u16, configured: u16) -> Vec<u8> {
        let mut s = vec![0u8; 0x5C];
        s[0] = 17;
        s[1] = 0x5C;
        s[0x0C..0x0E].copy_from_slice(&size.to_le_bytes());
        s[0x12] = type_code;
        s[0x15..0x17].copy_from_slice(&speed.to_le_bytes());
        s[0x20..0x22].copy_from_slice(&configured.to_le_bytes());
        s.extend_from_slice(b"DIMM\0\0");
        s
    }

    fn table(parts: &[Vec<u8>]) -> Vec<u8> {
        let mut t: Vec<u8> = parts.concat();
        t.extend_from_slice(&[127, 4, 0, 0, 0, 0]);
        t
    }

    #[test]
    fn real_lpddr5_laptop() {
        // Captured from a real laptop's /sys/firmware/dmi/tables/DMI, keeping
        // only the type 17 records, with their strings replaced.
        let raw = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/smbios-lpddr5.bin"));
        let m = memory_modules(raw);
        assert_eq!(
            m,
            MemoryModules {
                standard: Some("LPDDR5".into()),
                speed_mts: Some(5200),
                module_count: 8
            }
        );
    }

    #[test]
    fn skips_empty_slots_and_other_structures() {
        let bios = {
            let mut s = vec![0u8; 0x18];
            s[0] = 0;
            s[1] = 0x18;
            s.extend_from_slice(b"Vendor\0Version\0\0");
            s
        };
        let t = table(&[
            bios,
            memory_device(0, 0x02, 0, 0),
            memory_device(0x4000, 0x22, 4800, 6000),
        ]);
        assert_eq!(
            memory_modules(&t),
            MemoryModules {
                standard: Some("DDR5".into()),
                speed_mts: Some(6000),
                module_count: 1
            }
        );
    }

    #[test]
    fn falls_back_to_rated_speed_and_extended_fields() {
        let mut ext = memory_device(0x2000, 0x1A, 0xFFFF, 0);
        ext[0x54..0x58].copy_from_slice(&3200u32.to_le_bytes());
        let t = table(&[ext, memory_device(0x2000, 0x1A, 2666, 0)]);
        assert_eq!(
            memory_modules(&t),
            MemoryModules {
                standard: Some("DDR4".into()),
                speed_mts: Some(3200),
                module_count: 2
            }
        );
    }

    #[test]
    fn short_old_style_structures_and_garbage_dont_panic() {
        // SMBIOS 2.3-length record: no configured speed field at all.
        let mut s = memory_device(0x400, 0x13, 800, 0);
        s.truncate(0x1B);
        s[1] = 0x1B;
        s.extend_from_slice(b"\0\0");
        assert_eq!(memory_modules(&table(&[s])).speed_mts, Some(800));
        assert_eq!(memory_modules(&[17, 0x5C, 0, 0, 1, 2]), MemoryModules::default());
        assert_eq!(memory_modules(&[]), MemoryModules::default());
    }

    #[test]
    fn windows_header() {
        let mut raw = vec![0, 3, 4, 0];
        raw.extend_from_slice(&3u32.to_le_bytes());
        raw.extend_from_slice(&[1, 2, 3, 9, 9]);
        assert_eq!(strip_windows_header(&raw), &[1, 2, 3]);
    }
}
