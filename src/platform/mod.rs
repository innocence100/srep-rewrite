//! Private, best-effort host probes and compression resource policy.

#[cfg(target_os = "linux")]
mod linux;
mod resolve;

pub(crate) use resolve::{HardProbe, InputFacts, InputKind, MemoryProbe, resolve_compress_with};

pub(crate) trait HostProbe {
    fn physical(&self) -> MemoryProbe;
    fn hard(&self) -> HardProbe;
}

pub(crate) struct HostOsProbe;

impl HostProbe for HostOsProbe {
    fn physical(&self) -> MemoryProbe {
        #[cfg(target_os = "linux")]
        {
            linux::physical_memory()
        }
        #[cfg(target_os = "windows")]
        {
            system_memory::Snapshot::get()
                .map(|s| s.total)
                .map_or(MemoryProbe::Unavailable, |n| {
                    if n == 0 {
                        MemoryProbe::Invalid
                    } else {
                        MemoryProbe::Known(n)
                    }
                })
        }
        #[cfg(target_os = "macos")]
        {
            system_memory::macos::try_get_total_physical_memory().map_or(
                MemoryProbe::Unavailable,
                |n| {
                    if n == 0 {
                        MemoryProbe::Invalid
                    } else {
                        MemoryProbe::Known(n)
                    }
                },
            )
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
        {
            MemoryProbe::Unavailable
        }
    }

    fn hard(&self) -> HardProbe {
        #[cfg(target_os = "linux")]
        {
            linux::hard_limits()
        }
        #[cfg(not(target_os = "linux"))]
        {
            HardProbe::absent()
        }
    }
}
