//! Option-ROM image chain: PCI (`55 AA` + `PCIR`) and NVIDIA extended (`VN` + `NPDS`) images.

use crate::read::{u8_at, u16_at};
use serde::{Deserialize, Serialize};

/// Image code type of an x86 legacy option ROM.
pub const CODE_TYPE_LEGACY: u8 = 0x00;
/// Image code type of a UEFI option ROM.
pub const CODE_TYPE_EFI: u8 = 0x03;
/// NVIDIA data image (signed metadata, NPDS extension images).
pub const CODE_TYPE_NVIDIA_DATA: u8 = 0xe0;

const IMAGE_ALIGN: usize = 512;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ImageHeaderKind {
    Pcir,
    Npds,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct RomImage {
    pub start: usize,
    pub size: usize,
    pub header_kind: ImageHeaderKind,
    /// Offset of the PCIR/NPDS structure in the file.
    pub header: usize,
    pub vendor_id: u16,
    pub device_id: u16,
    pub code_type: u8,
    pub last: bool,
}

impl RomImage {
    pub fn end(&self) -> usize {
        self.start + self.size
    }
}

/// Every well-formed image header found on a 512-byte boundary, in file order.
///
/// Signature-like bytes inside a payload whose header pointer does not lead to a
/// matching structure, or whose bounds do not fit the file, are skipped.
pub fn scan(rom: &[u8]) -> Vec<RomImage> {
    let mut images = Vec::new();
    let mut start = 0;
    while start + 0x1a <= rom.len() {
        let kind = match &rom[start..start + 2] {
            [0x55, 0xaa] => Some((ImageHeaderKind::Pcir, b"PCIR")),
            b"VN" => Some((ImageHeaderKind::Npds, b"NPDS")),
            _ => None,
        };
        if let Some((header_kind, magic)) = kind
            && let Some(image) = parse_header(rom, start, header_kind, magic)
        {
            images.push(image);
        }
        start += IMAGE_ALIGN;
    }
    images
}

fn parse_header(
    rom: &[u8],
    start: usize,
    header_kind: ImageHeaderKind,
    magic: &[u8; 4],
) -> Option<RomImage> {
    let header = start + usize::from(u16_at(rom, start + 0x18)?);
    if header < start + 0x1a || rom.get(header..header + 4)? != magic {
        return None;
    }
    let header_size = usize::from(u16_at(rom, header + 0x0a)?);
    let size = usize::from(u16_at(rom, header + 0x10)?) * IMAGE_ALIGN;
    if header_size < 0x18
        || size == 0
        || start + size > rom.len()
        || header + header_size > start + size
    {
        return None;
    }
    Some(RomImage {
        start,
        size,
        header_kind,
        header,
        vendor_id: u16_at(rom, header + 4)?,
        device_id: u16_at(rom, header + 6)?,
        code_type: u8_at(rom, header + 0x14)?,
        last: u8_at(rom, header + 0x15)? & 0x80 != 0,
    })
}

/// The NVIDIA legacy image and the images contiguous after it.
#[derive(Debug, Clone)]
pub struct Chain {
    pub legacy: RomImage,
    pub efi: Option<RomImage>,
    /// Data images following EFI (GB20x: the signed metadata image, then the NPDS image).
    pub data: Vec<RomImage>,
}

impl Chain {
    pub fn find(images: &[RomImage]) -> Result<Self, String> {
        let mut legacy = images
            .iter()
            .filter(|image| image.vendor_id == 0x10de && image.code_type == CODE_TYPE_LEGACY);
        let legacy = match (legacy.next(), legacy.next()) {
            (Some(image), None) => image.clone(),
            (None, _) => return Err("no NVIDIA legacy option-ROM image".to_owned()),
            (Some(_), Some(_)) => return Err("more than one NVIDIA legacy image".to_owned()),
        };
        let at = |start: usize| images.iter().find(|image| image.start == start);

        let efi = at(legacy.end())
            .filter(|image| image.code_type == CODE_TYPE_EFI)
            .cloned();
        let mut cursor = efi.as_ref().map_or(legacy.end(), RomImage::end);
        let mut data = Vec::new();
        while let Some(image) = at(cursor) {
            if image.code_type != CODE_TYPE_NVIDIA_DATA || image.vendor_id != 0x10de {
                break;
            }
            data.push(image.clone());
            cursor = image.end();
            if image.header_kind == ImageHeaderKind::Npds && image.last {
                break;
            }
        }
        Ok(Self { legacy, efi, data })
    }

    /// End of the PCI-visible part (legacy + EFI): what the expansion-ROM BAR exposes.
    pub fn pci_end(&self) -> usize {
        self.efi.as_ref().map_or(self.legacy.end(), RomImage::end)
    }
}
