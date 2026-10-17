//! Bounds-checked little-endian reads.

pub fn u8_at(data: &[u8], at: usize) -> Option<u8> {
    data.get(at).copied()
}

pub fn u16_at(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(data.get(at..at + 2)?.try_into().ok()?))
}

pub fn i16_at(data: &[u8], at: usize) -> Option<i16> {
    Some(i16::from_le_bytes(data.get(at..at + 2)?.try_into().ok()?))
}

pub fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

pub fn f32_at(data: &[u8], at: usize) -> Option<f32> {
    Some(f32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?))
}
