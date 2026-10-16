use crate::{Error, Result};

pub(crate) const FALLBACK: u64 = 256 * 1024 * 1024;
pub(crate) const TEMP_BASE: u64 = 32 * 1024 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MemoryProbe {
    Known(u64),
    Invalid,
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HardProbe {
    pub identified: bool,
    pub known_min: Option<u64>,
    pub indeterminate: bool,
}

impl HardProbe {
    #[cfg(any(test, not(target_os = "linux")))]
    pub(crate) const fn absent() -> Self {
        Self {
            identified: false,
            known_min: None,
            indeterminate: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InputKind {
    Stdin,
    NonRegular,
    Regular,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct InputFacts {
    pub kind: InputKind,
    pub logical_len: Option<u64>,
}

impl InputFacts {
    pub(crate) const fn stdin() -> Self {
        Self {
            kind: InputKind::Stdin,
            logical_len: None,
        }
    }
    pub(crate) fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        if metadata.file_type().is_file() {
            Self {
                kind: InputKind::Regular,
                logical_len: Some(metadata.len()),
            }
        } else {
            Self {
                kind: InputKind::NonRegular,
                logical_len: None,
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Origin {
    Auto,
    Explicit,
    Fallback,
}
impl std::fmt::Display for Origin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Auto => "auto",
            Self::Explicit => "explicit",
            Self::Fallback => "fallback",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    PhysicalUnavailable,
    PhysicalInvalid,
    Indeterminate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ResourceValue {
    pub bytes: u64,
    pub origin: Origin,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Resolved {
    pub memory: ResourceValue,
    pub temp_limit: ResourceValue,
    pub warnings: Vec<String>,
}

pub(crate) fn warning_for(failure: Failure, bytes: u64) -> String {
    let reason = match failure {
        Failure::PhysicalUnavailable => {
            "automatic memory detection failed (physical memory unavailable)"
        }
        Failure::PhysicalInvalid => {
            "automatic memory detection failed (invalid physical memory reading)"
        }
        Failure::Indeterminate => "container memory limit could not be determined",
    };
    format!("{reason}; using conservative fallback memory budget {bytes} B for this operation")
}

fn memory_from_probes(physical: MemoryProbe, hard: HardProbe) -> (u64, Option<Failure>) {
    let phys_half = match physical {
        MemoryProbe::Known(n) if n > 0 => Some(n / 2),
        _ => None,
    };
    let physical_failure = match physical {
        MemoryProbe::Invalid | MemoryProbe::Known(0) => Some(Failure::PhysicalInvalid),
        _ => Some(Failure::PhysicalUnavailable),
    };
    if !hard.identified {
        return phys_half.map_or((FALLBACK, physical_failure), |v| (v, None));
    }
    if hard.indeterminate {
        let mut value = FALLBACK;
        if let Some(n) = hard.known_min {
            value = value.min(n / 2);
        }
        if let Some(n) = phys_half {
            value = value.min(n);
        }
        return (value, Some(Failure::Indeterminate));
    }
    if let Some(n) = hard.known_min {
        return match phys_half {
            Some(p) => (p.min(n / 2), None),
            None => (FALLBACK.min(n / 2), physical_failure),
        };
    }
    phys_half.map_or((FALLBACK, physical_failure), |v| (v, None))
}

fn temp_from_input(input: InputFacts) -> Result<u64> {
    if input.kind != InputKind::Regular {
        return Ok(TEMP_BASE);
    }
    match input.logical_len.unwrap_or(0).checked_mul(10) {
        Some(derived) => Ok(TEMP_BASE.max(derived)),
        None => Err(Error::invalid_config(
            "input is too large to derive a temporary limit; specify --temp-limit",
        )),
    }
}

pub(crate) fn resolve_compress_with(
    probe: &dyn super::HostProbe,
    explicit_memory: Option<u64>,
    explicit_temp_limit: Option<u64>,
    input: InputFacts,
) -> Result<Resolved> {
    let (memory, memory_failure) = if let Some(n) = explicit_memory {
        (n, None)
    } else {
        memory_from_probes(probe.physical(), probe.hard())
    };
    let temp = match explicit_temp_limit {
        Some(n) => n,
        None => temp_from_input(input)?,
    };
    let mut warnings = Vec::new();
    if let Some(failure) = memory_failure {
        warnings.push(warning_for(failure, memory));
    }
    Ok(Resolved {
        memory: ResourceValue {
            bytes: memory,
            origin: if explicit_memory.is_some() {
                Origin::Explicit
            } else if memory_failure.is_some() {
                Origin::Fallback
            } else {
                Origin::Auto
            },
        },
        temp_limit: ResourceValue {
            bytes: temp,
            origin: if explicit_temp_limit.is_some() {
                Origin::Explicit
            } else {
                Origin::Auto
            },
        },
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::InputKind;
    use super::*;
    struct Probe(MemoryProbe, HardProbe);
    impl super::super::HostProbe for Probe {
        fn physical(&self) -> MemoryProbe {
            self.0
        }
        fn hard(&self) -> HardProbe {
            self.1
        }
    }
    struct PanicProbe;
    impl super::super::HostProbe for PanicProbe {
        fn physical(&self) -> MemoryProbe {
            panic!("explicit memory probed")
        }
        fn hard(&self) -> HardProbe {
            panic!("explicit memory probed")
        }
    }
    #[test]
    fn memory_resolution_bounds_physical_and_hard_limits() {
        let absent = HardProbe::absent();
        assert_eq!(memory_from_probes(MemoryProbe::Known(64), absent).0, 32);
        assert_eq!(
            memory_from_probes(
                MemoryProbe::Known(64),
                HardProbe {
                    identified: true,
                    known_min: Some(20),
                    indeterminate: false
                }
            )
            .0,
            10
        );
        assert_eq!(
            memory_from_probes(
                MemoryProbe::Known(64),
                HardProbe {
                    identified: true,
                    known_min: Some(0),
                    indeterminate: false
                }
            )
            .0,
            0
        );
        assert_eq!(
            memory_from_probes(
                MemoryProbe::Unavailable,
                HardProbe {
                    identified: true,
                    known_min: Some(200),
                    indeterminate: false
                }
            )
            .0,
            100
        );
        assert_eq!(
            memory_from_probes(
                MemoryProbe::Known(64),
                HardProbe {
                    identified: true,
                    known_min: Some(80),
                    indeterminate: true
                }
            )
            .0,
            32
        );
    }

    #[test]
    fn physical_and_hard_resolution_table_covers_below_equal_above_and_extremes() {
        let cases = [
            (MemoryProbe::Known(64), Some(80), 32),
            (MemoryProbe::Known(64), Some(64), 32),
            (MemoryProbe::Known(64), Some(40), 20),
            (MemoryProbe::Known(65), Some(65), 32),
            (MemoryProbe::Known(u64::MAX), Some(u64::MAX), u64::MAX / 2),
            (MemoryProbe::Known(u64::MAX), None, u64::MAX / 2),
            (MemoryProbe::Known(1), None, 0),
            (MemoryProbe::Known(0), Some(80), 40),
        ];
        for (physical, hard_limit, expected) in cases {
            let hard = HardProbe {
                identified: true,
                known_min: hard_limit,
                indeterminate: false,
            };
            assert_eq!(
                memory_from_probes(physical, hard).0,
                expected,
                "{physical:?}/{hard_limit:?}"
            );
        }
        for physical in [MemoryProbe::Invalid, MemoryProbe::Unavailable] {
            for hard_limit in [
                None,
                Some(0),
                Some(1),
                Some(512 * 1024 * 1024),
                Some(u64::MAX),
            ] {
                let hard = HardProbe {
                    identified: true,
                    known_min: hard_limit,
                    indeterminate: false,
                };
                let (actual, failure) = memory_from_probes(physical, hard);
                let expected = hard_limit.map_or(FALLBACK, |n| FALLBACK.min(n / 2));
                assert_eq!(actual, expected, "{physical:?}/{hard_limit:?}");
                assert!(failure.is_some());
            }
        }
    }

    #[test]
    fn indeterminate_hard_limit_warning_matches_each_resolved_cap() {
        for (physical, hard_limit, expected) in [
            (MemoryProbe::Known(64), None, 32),
            (MemoryProbe::Known(8 * FALLBACK), Some(10), 5),
            (
                MemoryProbe::Known(FALLBACK),
                Some(4 * FALLBACK),
                FALLBACK / 2,
            ),
            (MemoryProbe::Unavailable, Some(100), 50),
        ] {
            let hard = HardProbe {
                identified: true,
                known_min: hard_limit,
                indeterminate: true,
            };
            let (bytes, failure) = memory_from_probes(physical, hard);
            assert_eq!(bytes, expected);
            assert_eq!(failure, Some(Failure::Indeterminate));
            assert!(warning_for(failure.unwrap(), bytes).contains(&format!("budget {bytes} B")));
        }
    }
    #[test]
    fn temp_policy_checked_and_explicit_values_bypass_metadata_formula() {
        assert_eq!(
            temp_from_input(InputFacts {
                kind: InputKind::Regular,
                logical_len: Some(0)
            })
            .unwrap(),
            TEMP_BASE
        );
        assert_eq!(
            temp_from_input(InputFacts {
                kind: InputKind::Regular,
                logical_len: Some(4)
            })
            .unwrap(),
            TEMP_BASE
        );
        assert!(
            temp_from_input(InputFacts {
                kind: InputKind::Regular,
                logical_len: Some(u64::MAX)
            })
            .unwrap_err()
            .to_string()
            .contains("--temp-limit")
        );
        let resolved = resolve_compress_with(
            &PanicProbe,
            Some(1),
            Some(2),
            InputFacts {
                kind: InputKind::Regular,
                logical_len: Some(u64::MAX),
            },
        )
        .unwrap();
        assert_eq!(resolved.memory.origin, Origin::Explicit);
        assert_eq!(resolved.temp_limit.bytes, 2);
    }

    #[test]
    fn regular_input_temp_growth_uses_exact_u64_integer_boundaries() {
        const GIB: u64 = 1024 * 1024 * 1024;
        let regular = |logical_len| InputFacts {
            kind: InputKind::Regular,
            logical_len: Some(logical_len),
        };
        assert_eq!(temp_from_input(regular(4 * GIB)).unwrap(), 40 * GIB);
        assert_eq!(temp_from_input(regular(8 * GIB)).unwrap(), 80 * GIB);

        // 32 GiB is not divisible by 10. Check its exact floor and first
        // greater input length without floating-point arithmetic.
        let floor_len = TEMP_BASE / 10;
        assert_eq!(temp_from_input(regular(floor_len)).unwrap(), TEMP_BASE);
        let first_over_floor = floor_len + 1;
        let expected_first_over_floor = first_over_floor * 10;
        assert!(expected_first_over_floor > TEMP_BASE);
        assert_eq!(
            temp_from_input(regular(first_over_floor)).unwrap(),
            expected_first_over_floor
        );

        // The largest length whose factor of ten remains representable is
        // accepted; the immediately following length must fail explicitly.
        let largest_nonoverflowing = u64::MAX / 10;
        assert_eq!(
            temp_from_input(regular(largest_nonoverflowing)).unwrap(),
            (largest_nonoverflowing * 10).max(TEMP_BASE)
        );
        let first_overflowing = largest_nonoverflowing + 1;
        let error = temp_from_input(regular(first_overflowing)).unwrap_err();
        assert_eq!(error.kind(), crate::ErrorKind::InvalidConfiguration);
        assert!(error.to_string().contains("--temp-limit"));
    }
    #[test]
    fn fallback_warning_is_actual_budget_and_explicit_memory_skips_host_detection() {
        let probe = Probe(MemoryProbe::Unavailable, HardProbe::absent());
        let resolved = resolve_compress_with(&probe, None, Some(10), InputFacts::stdin()).unwrap();
        assert_eq!(resolved.memory.bytes, FALLBACK);
        assert_eq!(resolved.warnings.len(), 1);
        assert!(resolved.warnings[0].contains(&format!("memory budget {FALLBACK} B")));
    }
}
