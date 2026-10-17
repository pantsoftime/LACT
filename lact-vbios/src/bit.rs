//! BIOS Information Table (NVIDIA's BIT, see open-gpu-doc `BIOS-Information-Table.md`).

use crate::{
    image::Chain,
    read::{u16_at, u32_at},
};
use std::collections::BTreeMap;

const BIT_SIGNATURE: &[u8] = b"\xff\xb8BIT\x00";

#[derive(Debug, Clone, Copy)]
pub struct Token {
    pub version: u8,
    pub size: u16,
    /// File offset of the token data.
    pub data: usize,
}

#[derive(Debug, Clone)]
pub struct Bit {
    pub tokens: BTreeMap<char, Token>,
}

impl Bit {
    /// Parses the single BIT inside the legacy image. Token pointers are relative to the
    /// legacy image start and must stay inside it.
    pub fn parse(rom: &[u8], chain: &Chain) -> Result<Self, String> {
        let legacy = &chain.legacy;
        let image = &rom[legacy.start..legacy.end()];
        let mut found = image
            .windows(BIT_SIGNATURE.len())
            .enumerate()
            .filter(|(_, window)| *window == BIT_SIGNATURE)
            .map(|(offset, _)| offset);
        let at = match (found.next(), found.next()) {
            (Some(at), None) => at,
            (None, _) => return Err("no BIT in the legacy image".to_owned()),
            (Some(_), Some(_)) => return Err("more than one BIT in the legacy image".to_owned()),
        };

        let header = image.get(at..at + 12).ok_or("truncated BIT header")?;
        let (header_size, token_size, count) = (
            usize::from(header[8]),
            usize::from(header[9]),
            usize::from(header[10]),
        );
        if header_size < 12
            || token_size < 6
            || count == 0
            || at + header_size + token_size * count > image.len()
        {
            return Err("invalid BIT dimensions".to_owned());
        }
        if image[at..at + header_size]
            .iter()
            .fold(0u8, |sum, b| sum.wrapping_add(*b))
            != 0
        {
            return Err("BIT header checksum mismatch".to_owned());
        }

        let mut tokens = BTreeMap::new();
        for i in 0..count {
            let entry = at + header_size + i * token_size;
            let id = char::from(image[entry]);
            let version = image[entry + 1];
            let size = u16_at(image, entry + 2).ok_or("truncated BIT token")?;
            let pointer = usize::from(u16_at(image, entry + 4).ok_or("truncated BIT token")?);
            if pointer + usize::from(size) > image.len() {
                return Err(format!("BIT token '{id}' points outside the legacy image"));
            }
            let token = Token {
                version,
                size,
                data: legacy.start + pointer,
            };
            if tokens.insert(id, token).is_some() {
                return Err(format!("duplicate BIT token '{id}'"));
            }
        }
        Ok(Self { tokens })
    }

    /// The version string nvidia-smi shows, from the BIOSDATA ('B') token:
    /// a u32 version and an OEM byte, printed as `98.02.2E.80.C9`.
    pub fn version(&self, rom: &[u8]) -> Option<String> {
        let token = self.tokens.get(&'B')?;
        let version = u32_at(rom, token.data)?;
        let oem = *rom.get(token.data + 4)?;
        let [a, b, c, d] = version.to_be_bytes();
        Some(format!("{a:02X}.{b:02X}.{c:02X}.{d:02X}.{oem:02X}"))
    }

    /// Resolves a 32-bit table pointer stored at `offset` inside a version-2 pointer
    /// token ('P' performance, 'C' clocks).
    ///
    /// Pointers address a "virtual BIOS" that runs on from the legacy image straight
    /// into the data images: the EFI image is not part of it, so pointers past the
    /// legacy image skip it.
    pub fn table(
        &self,
        rom: &[u8],
        chain: &Chain,
        token: char,
        offset: usize,
    ) -> Result<usize, String> {
        let entry = self
            .tokens
            .get(&token)
            .ok_or_else(|| format!("BIT token '{token}' is absent"))?;
        if entry.version != 2 {
            return Err(format!(
                "BIT token '{token}' has version {}, expected 2",
                entry.version
            ));
        }
        if offset + 4 > usize::from(entry.size) {
            return Err(format!(
                "pointer {offset:#x} is outside BIT token '{token}'"
            ));
        }
        let pointer = u32_at(rom, entry.data + offset).ok_or("truncated BIT pointer")? as usize;
        if pointer == 0 {
            return Err(format!("table '{token}'+{offset:#x} is absent"));
        }
        let mut at = chain.legacy.start + pointer;
        if pointer >= chain.legacy.size {
            at += chain.efi.as_ref().map_or(0, |efi| efi.size);
        }
        if at + 16 > rom.len() {
            return Err(format!(
                "table '{token}'+{offset:#x} lies outside the image (truncated ROM?)"
            ));
        }
        Ok(at)
    }
}
