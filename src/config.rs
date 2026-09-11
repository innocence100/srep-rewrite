use crate::error::{Error, Result};

pub const MAX_UNCOMPRESSED: u64 = (1u64 << 63) - 1;
pub const MIN_BLOCK_SIZE: u64 = 1024;
pub const MAX_BLOCK_SIZE: u64 = 1 << 30;
pub const DEFAULT_BLOCK_SIZE: u64 = 8 * 1024 * 1024;
pub const MIN_MATCH_LEN: u64 = 2;
pub const MAX_MATCH_LEN: u64 = 1 << 30;
pub const DEFAULT_MIN_MATCH_M3: u64 = 512;
pub const DEFAULT_MIN_MATCH_CDC: u64 = 32;
pub const DEFAULT_TARGET_CHUNK: u64 = 4096;
pub const DEFAULT_REP_DISTANCE: u64 = 512 * 1024 * 1024;
pub const DEFAULT_REP_MIN_MATCH: u64 = 512;
pub const DEFAULT_MEMORY: u64 = 256 * 1024 * 1024;
pub const DEFAULT_TEMP_LIMIT: u64 = 4 * 1024 * 1024 * 1024;
pub const M1_SEED_SIZE: u64 = 48;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Method {
    M0Rep,
    M1RollingCdc,
    M2Order1Cdc,
    #[default]
    M3FixedDigest,
    M4Reread,
    M5Exhaustive,
}

impl Method {
    pub const fn wire_id(self) -> u8 {
        match self {
            Self::M0Rep => 0,
            Self::M1RollingCdc => 1,
            Self::M2Order1Cdc => 2,
            Self::M3FixedDigest => 3,
            Self::M4Reread => 4,
            Self::M5Exhaustive => 5,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::M0Rep => "m0",
            Self::M1RollingCdc => "m1",
            Self::M2Order1Cdc => "m2",
            Self::M3FixedDigest => "m3",
            Self::M4Reread => "m4",
            Self::M5Exhaustive => "m5",
        }
    }

    pub fn from_wire(id: u8) -> Result<Self> {
        match id {
            0 => Ok(Self::M0Rep),
            1 => Ok(Self::M1RollingCdc),
            2 => Ok(Self::M2Order1Cdc),
            3 => Ok(Self::M3FixedDigest),
            4 => Ok(Self::M4Reread),
            5 => Ok(Self::M5Exhaustive),
            _ => Err(Error::corrupt_header(format!("unknown method {id}"))),
        }
    }

    pub fn parse(name: &str) -> Result<Self> {
        match name {
            "m0" | "rep" => Ok(Self::M0Rep),
            "m1" | "rolling-cdc" => Ok(Self::M1RollingCdc),
            "m2" | "order1-cdc" => Ok(Self::M2Order1Cdc),
            "m3" | "fixed-digest" => Ok(Self::M3FixedDigest),
            "m4" | "reread" => Ok(Self::M4Reread),
            "m5" | "exhaustive" => Ok(Self::M5Exhaustive),
            _ => Err(Error::invalid_config(format!("unknown method '{name}'"))),
        }
    }

    pub const fn default_min_match(self) -> u64 {
        match self {
            Self::M1RollingCdc | Self::M2Order1Cdc => DEFAULT_MIN_MATCH_CDC,
            _ => DEFAULT_MIN_MATCH_M3,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Layout {
    #[default]
    Index,
    Future,
    Io,
}

impl Layout {
    pub const fn wire_id(self) -> u8 {
        match self {
            Self::Index => 1,
            Self::Future => 2,
            Self::Io => 3,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Index => "index",
            Self::Future => "future",
            Self::Io => "io",
        }
    }

    pub fn from_wire(id: u8) -> Result<Self> {
        match id {
            1 => Ok(Self::Index),
            2 => Ok(Self::Future),
            3 => Ok(Self::Io),
            _ => Err(Error::corrupt_header(format!("unknown layout {id}"))),
        }
    }

    pub fn parse(name: &str) -> Result<Self> {
        match name {
            "index" => Ok(Self::Index),
            "future" => Ok(Self::Future),
            "io" => Ok(Self::Io),
            _ => Err(Error::invalid_config(format!("unknown layout '{name}'"))),
        }
    }

    pub const fn is_index(self) -> bool {
        matches!(self, Self::Index)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Checksum {
    #[default]
    Xxh3,
    Blake3,
}

impl Checksum {
    pub const fn wire_id(self) -> u8 {
        match self {
            Self::Xxh3 => 1,
            Self::Blake3 => 2,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Xxh3 => "xxh3",
            Self::Blake3 => "blake3",
        }
    }

    pub const fn digest_len(self) -> u8 {
        match self {
            Self::Xxh3 => 16,
            Self::Blake3 => 32,
        }
    }

    pub const fn width(self) -> usize {
        self.digest_len() as usize
    }

    pub fn from_wire(id: u8) -> Result<Self> {
        match id {
            1 => Ok(Self::Xxh3),
            2 => Ok(Self::Blake3),
            _ => Err(Error::unknown_checksum(format!("unknown checksum id {id}"))),
        }
    }

    pub fn parse(name: &str) -> Result<Self> {
        match name {
            "xxh3" => Ok(Self::Xxh3),
            "blake3" => Ok(Self::Blake3),
            _ => Err(Error::unknown_checksum(format!(
                "unknown checksum '{name}'"
            ))),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepConfig {
    pub distance: u64,
    pub min_match: u64,
}

impl Default for RepConfig {
    fn default() -> Self {
        Self {
            distance: DEFAULT_REP_DISTANCE,
            min_match: DEFAULT_REP_MIN_MATCH,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceConfig {
    pub memory: u64,
    pub temp_dir: std::path::PathBuf,
    pub temp_limit: u64,
    pub output_limit: u64,
}

impl Default for ResourceConfig {
    fn default() -> Self {
        Self {
            memory: DEFAULT_MEMORY,
            temp_dir: std::env::temp_dir(),
            temp_limit: DEFAULT_TEMP_LIMIT,
            output_limit: MAX_UNCOMPRESSED,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompressionConfig {
    pub method: Method,
    pub layout: Layout,
    pub checksum: Checksum,
    pub block_size: u64,
    pub min_match: u64,
    pub seed_size: Option<u64>,
    pub target_chunk: Option<u64>,
    pub max_distance: Option<u64>,
    pub rep_overlay: Option<RepConfig>,
    pub resources: ResourceConfig,
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            method: Method::M3FixedDigest,
            layout: Layout::Index,
            checksum: Checksum::Xxh3,
            block_size: DEFAULT_BLOCK_SIZE,
            min_match: DEFAULT_MIN_MATCH_M3,
            seed_size: Some(DEFAULT_MIN_MATCH_M3),
            target_chunk: None,
            max_distance: None,
            rep_overlay: None,
            resources: ResourceConfig::default(),
        }
    }
}

impl CompressionConfig {
    pub fn for_method(method: Method) -> Self {
        let mut config = Self {
            method,
            min_match: method.default_min_match(),
            ..Self::default()
        };
        match method {
            Method::M0Rep | Method::M5Exhaustive => {
                config.seed_size = None;
                config.target_chunk = None;
            }
            Method::M1RollingCdc | Method::M2Order1Cdc => {
                config.seed_size = None;
                config.target_chunk = Some(DEFAULT_TARGET_CHUNK);
            }
            Method::M3FixedDigest | Method::M4Reread => {
                config.seed_size = Some(config.min_match);
                config.target_chunk = None;
            }
        }
        config
    }

    pub fn validate(&self) -> Result<()> {
        if !(MIN_BLOCK_SIZE..=MAX_BLOCK_SIZE).contains(&self.block_size) {
            return Err(Error::invalid_config(
                "block size must be between 1 KiB and 1 GiB",
            ));
        }
        if !(MIN_MATCH_LEN..=MAX_MATCH_LEN).contains(&self.min_match) {
            return Err(Error::invalid_config(
                "minimum match must be between 2 and 1 GiB",
            ));
        }
        if let Some(max_distance) = self.max_distance
            && max_distance > MAX_UNCOMPRESSED
        {
            return Err(Error::invalid_config(
                "maximum distance exceeds wire hard limit",
            ));
        }
        match self.method {
            Method::M0Rep => {
                if self.seed_size.is_some() {
                    return Err(Error::invalid_config("seed-size is invalid for m0"));
                }
                if self.target_chunk.is_some() {
                    return Err(Error::invalid_config("target-chunk is invalid for m0"));
                }
                if self.rep_overlay.is_some() {
                    return Err(Error::invalid_config("rep overlay is invalid for m0"));
                }
            }
            Method::M1RollingCdc | Method::M2Order1Cdc => {
                if self.seed_size.is_some() {
                    return Err(Error::invalid_config(
                        "seed-size is invalid for CDC methods",
                    ));
                }
                let Some(target) = self.target_chunk else {
                    return Err(Error::invalid_config(
                        "target-chunk is required for CDC methods",
                    ));
                };
                if !(32..=MAX_MATCH_LEN).contains(&target) || target < self.min_match {
                    return Err(Error::invalid_config(
                        "target-chunk must be 32..=1 GiB and at least the minimum match",
                    ));
                }
                if self.rep_overlay.is_some() {
                    return Err(Error::invalid_config(
                        "rep overlay is invalid for CDC methods",
                    ));
                }
            }
            Method::M3FixedDigest | Method::M4Reread => {
                let Some(seed) = self.seed_size else {
                    return Err(Error::invalid_config("seed-size is required for m3 and m4"));
                };
                if !(1..=MAX_MATCH_LEN).contains(&seed) {
                    return Err(Error::invalid_config(
                        "seed-size must be 1..=1 GiB for m3 and m4",
                    ));
                }
                if self.target_chunk.is_some() {
                    return Err(Error::invalid_config(
                        "target-chunk is invalid for this method",
                    ));
                }
            }
            Method::M5Exhaustive => {
                if self.seed_size.is_some() {
                    return Err(Error::invalid_config("seed-size is invalid for m5"));
                }
                if self.target_chunk.is_some() {
                    return Err(Error::invalid_config("target-chunk is invalid for m5"));
                }
                let _ = m5_seed_size(self.min_match)?;
            }
        }
        if let Some(overlay) = &self.rep_overlay {
            if !matches!(
                self.method,
                Method::M3FixedDigest | Method::M4Reread | Method::M5Exhaustive
            ) {
                return Err(Error::invalid_config(
                    "rep overlay is valid only for m3, m4, and m5",
                ));
            }
            if overlay.distance == 0 || overlay.distance > MAX_UNCOMPRESSED {
                return Err(Error::invalid_config(
                    "rep-distance must be positive and at most MAX_UNCOMPRESSED",
                ));
            }
            if !(MIN_MATCH_LEN..=MAX_MATCH_LEN).contains(&overlay.min_match) {
                return Err(Error::invalid_config(
                    "rep-min-match must be between 2 and 1 GiB",
                ));
            }
        }
        if self.resources.memory == 0 {
            return Err(Error::invalid_config("memory limit must be positive"));
        }
        if self.resources.temp_limit == 0 {
            return Err(Error::invalid_config("temporary limit must be positive"));
        }
        if self.resources.output_limit == 0 || self.resources.output_limit > MAX_UNCOMPRESSED {
            return Err(Error::invalid_config(
                "output limit must be between 1 and MAX_UNCOMPRESSED",
            ));
        }
        Ok(())
    }

    pub fn effective_min_match(&self) -> Result<u64> {
        self.validate()?;
        Ok(self.rep_overlay.as_ref().map_or(self.min_match, |overlay| {
            self.min_match.min(overlay.min_match)
        }))
    }

    pub fn header_seed_size(&self) -> Result<u64> {
        Ok(match self.method {
            Method::M0Rep => self.min_match,
            Method::M1RollingCdc => M1_SEED_SIZE,
            Method::M2Order1Cdc => 0,
            Method::M3FixedDigest | Method::M4Reread => self
                .seed_size
                .ok_or_else(|| Error::invalid_config("seed-size is required for m3 and m4"))?,
            Method::M5Exhaustive => m5_seed_size(self.min_match)?,
        })
    }

    pub fn header_target_chunk(&self) -> u64 {
        self.target_chunk.unwrap_or(0)
    }

    pub fn header_max_distance(&self) -> u64 {
        self.max_distance.unwrap_or(0)
    }

    pub fn semantic_flags(&self) -> u8 {
        u8::from(self.rep_overlay.is_some())
    }

    pub fn block_size_usize(&self) -> Result<usize> {
        usize::try_from(self.block_size)
            .map_err(|_| Error::invalid_config("block size exceeds platform limits"))
    }
}

/// Compatibility alias retained for existing callers during the Stage 1 refactor.
pub type Config = CompressionConfig;

pub fn m5_seed_size(minimum_match: u64) -> Result<u64> {
    if minimum_match < MIN_MATCH_LEN {
        return Err(Error::invalid_config("m5 minimum match must be at least 2"));
    }
    let plus_one = minimum_match
        .checked_add(1)
        .ok_or_else(|| Error::invalid_config("m5 minimum match overflows"))?;
    let k = plus_one.ilog2();
    if k == 0 {
        return Err(Error::invalid_config("m5 derived seed is zero"));
    }
    let exponent = k
        .checked_sub(1)
        .ok_or_else(|| Error::invalid_config("m5 seed exponent underflows"))?;
    let l = 1u64
        .checked_shl(exponent)
        .ok_or_else(|| Error::invalid_config("m5 derived seed overflows"))?;
    if l == 0 {
        return Err(Error::invalid_config("m5 derived seed is zero"));
    }
    Ok(l)
}

pub fn parse_size(value: &str) -> Result<u64> {
    let text = value;
    if text.is_empty() {
        return Err(Error::invalid_config("size cannot be empty"));
    }
    if text.chars().any(|c| c.is_ascii_whitespace()) {
        return Err(Error::invalid_config(format!("invalid size: {value}")));
    }
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    if split == 0 {
        return Err(Error::invalid_config(format!("invalid size: {value}")));
    }
    let (number, suffix) = text.split_at(split);
    let base: u128 = number
        .parse()
        .map_err(|_| Error::invalid_config(format!("invalid size: {value}")))?;
    let multiplier: u128 = match suffix.to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        _ => {
            return Err(Error::invalid_config(format!(
                "unknown size unit: {suffix}"
            )));
        }
    };
    let result = base
        .checked_mul(multiplier)
        .ok_or_else(|| Error::invalid_config("size overflows u64"))?;
    u64::try_from(result).map_err(|_| Error::invalid_config("size overflows u64"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_human_sizes() {
        assert_eq!(parse_size("4KiB").unwrap(), 4096);
        assert_eq!(parse_size("2m").unwrap(), 2 * 1024 * 1024);
        assert!(parse_size("3.5MiB").is_err());
        assert!(parse_size("").is_err());
    }

    #[test]
    fn default_config_is_m3_index_xxh3() {
        let config = CompressionConfig::default();
        config.validate().unwrap();
        assert_eq!(config.method, Method::M3FixedDigest);
        assert_eq!(config.layout, Layout::Index);
        assert_eq!(config.checksum, Checksum::Xxh3);
        assert_eq!(config.block_size, DEFAULT_BLOCK_SIZE);
        assert_eq!(config.min_match, 512);
        assert_eq!(config.seed_size, Some(512));
        assert_eq!(config.target_chunk, None);
        assert_eq!(config.max_distance, None);
        assert!(config.rep_overlay.is_none());
    }

    #[test]
    fn m5_seed_formula_matches_spec() {
        assert_eq!(m5_seed_size(2).unwrap(), 1);
        assert_eq!(m5_seed_size(3).unwrap(), 2);
        assert_eq!(m5_seed_size(7).unwrap(), 4);
        assert_eq!(m5_seed_size(8).unwrap(), 4);
        assert_eq!(m5_seed_size(15).unwrap(), 8);
        assert_eq!(m5_seed_size(16).unwrap(), 8);
        assert_eq!(m5_seed_size(511).unwrap(), 256);
        assert_eq!(m5_seed_size(512).unwrap(), 256);
    }

    #[test]
    fn rejects_method_option_conflicts() {
        let mut config = CompressionConfig::for_method(Method::M0Rep);
        config.seed_size = Some(512);
        assert!(config.validate().is_err());
        let overlay = CompressionConfig {
            method: Method::M1RollingCdc,
            min_match: 32,
            seed_size: None,
            target_chunk: Some(4096),
            rep_overlay: Some(RepConfig::default()),
            ..CompressionConfig::default()
        };
        assert!(overlay.validate().is_err());
    }
}
