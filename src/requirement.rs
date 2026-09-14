//! CommonMark-based extraction and validation for the versioned requirement manifest.
//!
//! The implementation deliberately keeps canonicalization in byte-oriented helpers. The
//! resulting payload is hashed as arbitrary bytes; it is not reconstructed from JSON text.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Component, Path};

use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

pub const EXTRACTOR_VERSION: &str = "6";
pub const SPEC_RELATIVE_PATH: &str =
    "docs/superpowers/specs/2026-08-30-srep-capability-fidelity-design.md";
pub const MANIFEST_RELATIVE_PATH: &str = "tests/requirements.json";
// The mapping below is curated against this exact reviewed specification.  A changed
// specification must update the mapping deliberately; `--write` must not turn a changed
// payload into an apparently audited requirement merely because its heading survived.
const MAPPED_SPEC_SHA256: &str = "9c6abeafb1c7df5ae30c41e4e63bb9cada930c349c0401de89c608c7ce2c7db3";

/// The single approved retirement reason. Retirement is deliberately
/// requirement-level and reason-exact: a retired row whose reason differs from
/// this string is rejected, so a novel explanation cannot be used to smuggle an
/// unreviewed retirement past the audit.
pub const NG_V2_RETIRED_REASON: &str = "Genuinely obsolete NGv2-only wire layout, record, metadata, or exact-v2 matrix assertion; the archived v2 text and frozen pins remain provenance-only while the inherited algorithm, resource, legacy-reader, checksum, and NGv3 requirements stay active.";

/// One entry in the explicit, curated retirement policy. Every retired
/// requirement is named by its exact canonical ID and bound to the exact
/// historical subsection it was reviewed under. There is intentionally no
/// heading-prefix or section-range rule: a new unit dropped into section 10 is
/// active by default unless a human adds its exact ID here, and an entry whose
/// section no longer matches the extracted heading is reported as policy drift.
struct RetiredRequirement {
    id: &'static str,
    section: &'static str,
}

/// Explicit curated list of genuinely obsolete NGv2 wire requirements.
///
/// Categories, each reviewed individually:
/// - `10.` / `10.1` / `10.2` / `10.3` / `10.4` / `10.5` / `10.6` / `10.7` /
///   `10.8` / `10.9` / `10.11`: exact NGv2 wire magic, header, framing,
///   payload, and trailer layouts, superseded by the NGv3 format.
/// - Index-LZ / Future-LZ / I/O-LZ DataBlock payload subsections: NGv2 wire
///   DataBlock schemas, superseded by the NGv3 operation stream.
/// - `13.2 Exact v2 matrix assertions`: the retired NGv2 wire test matrix.
///
/// Section `10.0 Wire limits versus operational limits` is deliberately absent:
/// it is a mixed requirement whose operational/resource limits are still live,
/// so the whole obligation stays active. Section `10.10 Checksum IDs and
/// implementation research decision` is absent because the checksum
/// implementations, CLI selector, and golden vectors remain NGv3-essential.
const RETIRED_NG_V2_WIRE_REQUIREMENTS: &[RetiredRequirement] = &[
    RetiredRequirement {
        id: "REQ-95ed3db344b00b89",
        section: "> 10. SREP-NG v2 wire format",
    },
    // 10.1 ArchiveHeader: exact 80 bytes
    RetiredRequirement {
        id: "REQ-30189c7adaccc542",
        section: "> 10.1 ArchiveHeader: exact 80 bytes",
    },
    RetiredRequirement {
        id: "REQ-af3e03fb6937dcea",
        section: "> 10.1 ArchiveHeader: exact 80 bytes",
    },
    RetiredRequirement {
        id: "REQ-fd8e42d8c9e680de",
        section: "> 10.1 ArchiveHeader: exact 80 bytes",
    },
    RetiredRequirement {
        id: "REQ-3960951c86d60bb4",
        section: "> 10.1 ArchiveHeader: exact 80 bytes",
    },
    RetiredRequirement {
        id: "REQ-a7fd4a53e29ff5c4",
        section: "> 10.1 ArchiveHeader: exact 80 bytes",
    },
    RetiredRequirement {
        id: "REQ-434abf04301a6b7e",
        section: "> 10.1 ArchiveHeader: exact 80 bytes",
    },
    RetiredRequirement {
        id: "REQ-949c0ab02d2f11cc",
        section: "> 10.1 ArchiveHeader: exact 80 bytes",
    },
    RetiredRequirement {
        id: "REQ-26e1aa45662c1ef8",
        section: "> 10.1 ArchiveHeader: exact 80 bytes",
    },
    RetiredRequirement {
        id: "REQ-adce3168872e8f38",
        section: "> 10.1 ArchiveHeader: exact 80 bytes",
    },
    RetiredRequirement {
        id: "REQ-ebbfe3bd779ced61",
        section: "> 10.1 ArchiveHeader: exact 80 bytes",
    },
    RetiredRequirement {
        id: "REQ-55722d6b53af0159",
        section: "> 10.1 ArchiveHeader: exact 80 bytes",
    },
    RetiredRequirement {
        id: "REQ-cd41302a3e044fba",
        section: "> 10.1 ArchiveHeader: exact 80 bytes",
    },
    RetiredRequirement {
        id: "REQ-8f713976d259a2e3",
        section: "> 10.1 ArchiveHeader: exact 80 bytes",
    },
    RetiredRequirement {
        id: "REQ-8d24af74add89557",
        section: "> 10.1 ArchiveHeader: exact 80 bytes",
    },
    RetiredRequirement {
        id: "REQ-e07e47c7ac502cce",
        section: "> 10.1 ArchiveHeader: exact 80 bytes",
    },
    RetiredRequirement {
        id: "REQ-30289e885fd4de87",
        section: "> 10.1 ArchiveHeader: exact 80 bytes",
    },
    RetiredRequirement {
        id: "REQ-4744c02078794bff",
        section: "> 10.1 ArchiveHeader: exact 80 bytes",
    },
    RetiredRequirement {
        id: "REQ-8442397aaaf91528",
        section: "> 10.1 ArchiveHeader: exact 80 bytes",
    },
    RetiredRequirement {
        id: "REQ-507062d05da8afff",
        section: "> 10.1 ArchiveHeader: exact 80 bytes",
    },
    RetiredRequirement {
        id: "REQ-339ac871e1b00798",
        section: "> 10.1 ArchiveHeader: exact 80 bytes",
    },
    RetiredRequirement {
        id: "REQ-f1afe0d2af6a61f6",
        section: "> 10.1 ArchiveHeader: exact 80 bytes",
    },
    RetiredRequirement {
        id: "REQ-777336509840145a",
        section: "> 10.1 ArchiveHeader: exact 80 bytes",
    },
    RetiredRequirement {
        id: "REQ-9b184a10043250b0",
        section: "> 10.1 ArchiveHeader: exact 80 bytes",
    },
    RetiredRequirement {
        id: "REQ-6d11735e2027bbff",
        section: "> 10.1 ArchiveHeader: exact 80 bytes",
    },
    RetiredRequirement {
        id: "REQ-0adb94e686eede7f",
        section: "> 10.1 ArchiveHeader: exact 80 bytes",
    },
    RetiredRequirement {
        id: "REQ-5fe43fd36432f007",
        section: "> 10.1 ArchiveHeader: exact 80 bytes",
    },
    // 10.2 Record framing and cardinality
    RetiredRequirement {
        id: "REQ-dfdac707854305fc",
        section: "> 10.2 Record framing and cardinality",
    },
    RetiredRequirement {
        id: "REQ-e5eadf5e82c993d2",
        section: "> 10.2 Record framing and cardinality",
    },
    RetiredRequirement {
        id: "REQ-bbd96eac6f03be5e",
        section: "> 10.2 Record framing and cardinality",
    },
    RetiredRequirement {
        id: "REQ-42700d729ed6f6e8",
        section: "> 10.2 Record framing and cardinality",
    },
    RetiredRequirement {
        id: "REQ-ea461e5d37e074f0",
        section: "> 10.2 Record framing and cardinality",
    },
    RetiredRequirement {
        id: "REQ-5c77228f039aa61b",
        section: "> 10.2 Record framing and cardinality",
    },
    RetiredRequirement {
        id: "REQ-637bc6d1b619f0a2",
        section: "> 10.2 Record framing and cardinality",
    },
    RetiredRequirement {
        id: "REQ-704da0008399e05a",
        section: "> 10.2 Record framing and cardinality",
    },
    RetiredRequirement {
        id: "REQ-1ff7ef53b5ee84ea",
        section: "> 10.2 Record framing and cardinality",
    },
    // 10.3 Record checksum domains
    RetiredRequirement {
        id: "REQ-00d70b12a048f06e",
        section: "> 10.3 Record checksum domains",
    },
    RetiredRequirement {
        id: "REQ-6b99b85bb0936c34",
        section: "> 10.3 Record checksum domains",
    },
    RetiredRequirement {
        id: "REQ-6af3f4d7d820cbe7",
        section: "> 10.3 Record checksum domains",
    },
    // 10.4 MethodParameters payload: exact 64 bytes
    RetiredRequirement {
        id: "REQ-20c4c2c1a1ff19d3",
        section: "> 10.4 MethodParameters payload: exact 64 bytes",
    },
    RetiredRequirement {
        id: "REQ-e798515d19062f9e",
        section: "> 10.4 MethodParameters payload: exact 64 bytes",
    },
    RetiredRequirement {
        id: "REQ-1afeed28e9740d77",
        section: "> 10.4 MethodParameters payload: exact 64 bytes",
    },
    RetiredRequirement {
        id: "REQ-60722680f8e274d9",
        section: "> 10.4 MethodParameters payload: exact 64 bytes",
    },
    RetiredRequirement {
        id: "REQ-f35a17a128d8e192",
        section: "> 10.4 MethodParameters payload: exact 64 bytes",
    },
    RetiredRequirement {
        id: "REQ-00a0e69dcb30585e",
        section: "> 10.4 MethodParameters payload: exact 64 bytes",
    },
    RetiredRequirement {
        id: "REQ-c6ec632b9c390f36",
        section: "> 10.4 MethodParameters payload: exact 64 bytes",
    },
    RetiredRequirement {
        id: "REQ-3244ca2b386b3acf",
        section: "> 10.4 MethodParameters payload: exact 64 bytes",
    },
    RetiredRequirement {
        id: "REQ-32804f6a2eb3a9c3",
        section: "> 10.4 MethodParameters payload: exact 64 bytes",
    },
    RetiredRequirement {
        id: "REQ-3a05ce3c9ee1b807",
        section: "> 10.4 MethodParameters payload: exact 64 bytes",
    },
    RetiredRequirement {
        id: "REQ-03fdff0224760ec0",
        section: "> 10.4 MethodParameters payload: exact 64 bytes",
    },
    RetiredRequirement {
        id: "REQ-453c6006ae3c5e26",
        section: "> 10.4 MethodParameters payload: exact 64 bytes",
    },
    RetiredRequirement {
        id: "REQ-7b5a9422853c5939",
        section: "> 10.4 MethodParameters payload: exact 64 bytes",
    },
    // 10.5 LayoutMetadata payload: exact 64 bytes
    RetiredRequirement {
        id: "REQ-83e077b344cee1f0",
        section: "> 10.5 LayoutMetadata payload: exact 64 bytes",
    },
    RetiredRequirement {
        id: "REQ-b9b2c0f927408f9e",
        section: "> 10.5 LayoutMetadata payload: exact 64 bytes",
    },
    RetiredRequirement {
        id: "REQ-942061ee62ffbaf2",
        section: "> 10.5 LayoutMetadata payload: exact 64 bytes",
    },
    RetiredRequirement {
        id: "REQ-44c101de2dcc36f0",
        section: "> 10.5 LayoutMetadata payload: exact 64 bytes",
    },
    RetiredRequirement {
        id: "REQ-c994cfdbd1d212c8",
        section: "> 10.5 LayoutMetadata payload: exact 64 bytes",
    },
    RetiredRequirement {
        id: "REQ-dc9fef2ca7e86062",
        section: "> 10.5 LayoutMetadata payload: exact 64 bytes",
    },
    RetiredRequirement {
        id: "REQ-d6073beaa0629b0f",
        section: "> 10.5 LayoutMetadata payload: exact 64 bytes",
    },
    RetiredRequirement {
        id: "REQ-ca35fb062cabef4c",
        section: "> 10.5 LayoutMetadata payload: exact 64 bytes",
    },
    RetiredRequirement {
        id: "REQ-ecc7350b2e453b5b",
        section: "> 10.5 LayoutMetadata payload: exact 64 bytes",
    },
    RetiredRequirement {
        id: "REQ-48f181a461536207",
        section: "> 10.5 LayoutMetadata payload: exact 64 bytes",
    },
    RetiredRequirement {
        id: "REQ-1bebb9a5bbea0685",
        section: "> 10.5 LayoutMetadata payload: exact 64 bytes",
    },
    RetiredRequirement {
        id: "REQ-46f0b3764b11ab64",
        section: "> 10.5 LayoutMetadata payload: exact 64 bytes",
    },
    RetiredRequirement {
        id: "REQ-81fc4e37bb868645",
        section: "> 10.5 LayoutMetadata payload: exact 64 bytes",
    },
    // 10.6 BlockDirectory payload: exact Index-LZ schema
    RetiredRequirement {
        id: "REQ-c451d68f9551fb5b",
        section: "> 10.6 BlockDirectory payload: exact Index-LZ schema",
    },
    RetiredRequirement {
        id: "REQ-c2aedd2d6076ab8d",
        section: "> 10.6 BlockDirectory payload: exact Index-LZ schema",
    },
    RetiredRequirement {
        id: "REQ-67d349a8fcf89456",
        section: "> 10.6 BlockDirectory payload: exact Index-LZ schema",
    },
    RetiredRequirement {
        id: "REQ-ccf61a7067319c35",
        section: "> 10.6 BlockDirectory payload: exact Index-LZ schema",
    },
    RetiredRequirement {
        id: "REQ-e636240890b260c6",
        section: "> 10.6 BlockDirectory payload: exact Index-LZ schema",
    },
    RetiredRequirement {
        id: "REQ-cd4874b532a1a130",
        section: "> 10.6 BlockDirectory payload: exact Index-LZ schema",
    },
    RetiredRequirement {
        id: "REQ-b6a4c66e4db34138",
        section: "> 10.6 BlockDirectory payload: exact Index-LZ schema",
    },
    RetiredRequirement {
        id: "REQ-7cf43d4ac76a3dbe",
        section: "> 10.6 BlockDirectory payload: exact Index-LZ schema",
    },
    RetiredRequirement {
        id: "REQ-dc28d4aa249a5f1b",
        section: "> 10.6 BlockDirectory payload: exact Index-LZ schema",
    },
    RetiredRequirement {
        id: "REQ-3df601af689e162d",
        section: "> 10.6 BlockDirectory payload: exact Index-LZ schema",
    },
    RetiredRequirement {
        id: "REQ-054fabbe22d1ef24",
        section: "> 10.6 BlockDirectory payload: exact Index-LZ schema",
    },
    RetiredRequirement {
        id: "REQ-628f3903273490be",
        section: "> 10.6 BlockDirectory payload: exact Index-LZ schema",
    },
    RetiredRequirement {
        id: "REQ-40166ac42e29a387",
        section: "> 10.6 BlockDirectory payload: exact Index-LZ schema",
    },
    RetiredRequirement {
        id: "REQ-e2d475e86e357c97",
        section: "> 10.6 BlockDirectory payload: exact Index-LZ schema",
    },
    RetiredRequirement {
        id: "REQ-74606b4a2c50e8a1",
        section: "> 10.6 BlockDirectory payload: exact Index-LZ schema",
    },
    // 10.7 Common DataBlock payload header: exact 48 bytes
    RetiredRequirement {
        id: "REQ-b93b66f9ef541cf4",
        section: "> 10.7 Common DataBlock payload header: exact 48 bytes",
    },
    RetiredRequirement {
        id: "REQ-f75490d701b20509",
        section: "> 10.7 Common DataBlock payload header: exact 48 bytes",
    },
    RetiredRequirement {
        id: "REQ-3a9e164d7a0ca2f4",
        section: "> 10.7 Common DataBlock payload header: exact 48 bytes",
    },
    RetiredRequirement {
        id: "REQ-08fbee08e6468452",
        section: "> 10.7 Common DataBlock payload header: exact 48 bytes",
    },
    RetiredRequirement {
        id: "REQ-3e74d10813c0afd6",
        section: "> 10.7 Common DataBlock payload header: exact 48 bytes",
    },
    RetiredRequirement {
        id: "REQ-9e8c7dda06634e8a",
        section: "> 10.7 Common DataBlock payload header: exact 48 bytes",
    },
    RetiredRequirement {
        id: "REQ-99d31ec2c757a6c6",
        section: "> 10.7 Common DataBlock payload header: exact 48 bytes",
    },
    RetiredRequirement {
        id: "REQ-9417f672e03be42d",
        section: "> 10.7 Common DataBlock payload header: exact 48 bytes",
    },
    RetiredRequirement {
        id: "REQ-fe3862d193bf8881",
        section: "> 10.7 Common DataBlock payload header: exact 48 bytes",
    },
    RetiredRequirement {
        id: "REQ-36f0574d95319747",
        section: "> 10.7 Common DataBlock payload header: exact 48 bytes",
    },
    RetiredRequirement {
        id: "REQ-f9ae09887b3896d7",
        section: "> 10.7 Common DataBlock payload header: exact 48 bytes",
    },
    RetiredRequirement {
        id: "REQ-643fb92c38dbfba1",
        section: "> 10.7 Common DataBlock payload header: exact 48 bytes",
    },
    RetiredRequirement {
        id: "REQ-0c0604b325783554",
        section: "> 10.7 Common DataBlock payload header: exact 48 bytes",
    },
    RetiredRequirement {
        id: "REQ-f49d801797b26291",
        section: "> 10.7 Common DataBlock payload header: exact 48 bytes",
    },
    // Index-LZ / Future-LZ / I/O-LZ DataBlock payload subsections
    RetiredRequirement {
        id: "REQ-f98865027e801c6a",
        section: "> Index-LZ DataBlock payload",
    },
    RetiredRequirement {
        id: "REQ-b92f55ced8c9893a",
        section: "> Future-LZ DataBlock payload",
    },
    RetiredRequirement {
        id: "REQ-84b0fbf479e2b8ab",
        section: "> Future-LZ DataBlock payload",
    },
    RetiredRequirement {
        id: "REQ-b38a78fac6de417a",
        section: "> Future-LZ DataBlock payload",
    },
    RetiredRequirement {
        id: "REQ-d08536bb04d55173",
        section: "> Future-LZ DataBlock payload",
    },
    RetiredRequirement {
        id: "REQ-2b58f37424e8c568",
        section: "> Future-LZ DataBlock payload",
    },
    RetiredRequirement {
        id: "REQ-fb6a2a1e707aac00",
        section: "> Future-LZ DataBlock payload",
    },
    RetiredRequirement {
        id: "REQ-3005f15f8f0439f9",
        section: "> Future-LZ DataBlock payload",
    },
    RetiredRequirement {
        id: "REQ-59298172de52cf6e",
        section: "> I/O-LZ DataBlock payload",
    },
    RetiredRequirement {
        id: "REQ-fe7792f00452d221",
        section: "> I/O-LZ DataBlock payload",
    },
    RetiredRequirement {
        id: "REQ-325582c5ae40bfc1",
        section: "> I/O-LZ DataBlock payload",
    },
    RetiredRequirement {
        id: "REQ-eb496bfc9f7de825",
        section: "> I/O-LZ DataBlock payload",
    },
    RetiredRequirement {
        id: "REQ-efcb1ff4a659e8d2",
        section: "> I/O-LZ DataBlock payload",
    },
    RetiredRequirement {
        id: "REQ-7f36f071285547a6",
        section: "> I/O-LZ DataBlock payload",
    },
    RetiredRequirement {
        id: "REQ-38c2a7ade08b973f",
        section: "> I/O-LZ DataBlock payload",
    },
    RetiredRequirement {
        id: "REQ-eb7a227e2393a89e",
        section: "> I/O-LZ DataBlock payload",
    },
    // 10.8 IndexSection payload: exact Index-LZ schema
    RetiredRequirement {
        id: "REQ-f5687efd3657235d",
        section: "> 10.8 IndexSection payload: exact Index-LZ schema",
    },
    RetiredRequirement {
        id: "REQ-2584d54e8942761a",
        section: "> 10.8 IndexSection payload: exact Index-LZ schema",
    },
    RetiredRequirement {
        id: "REQ-6857ec0348350597",
        section: "> 10.8 IndexSection payload: exact Index-LZ schema",
    },
    RetiredRequirement {
        id: "REQ-7a068c05be6a48f6",
        section: "> 10.8 IndexSection payload: exact Index-LZ schema",
    },
    RetiredRequirement {
        id: "REQ-2aafe52e6bc96fa7",
        section: "> 10.8 IndexSection payload: exact Index-LZ schema",
    },
    RetiredRequirement {
        id: "REQ-37b240eee559120c",
        section: "> 10.8 IndexSection payload: exact Index-LZ schema",
    },
    RetiredRequirement {
        id: "REQ-448ea38df130418d",
        section: "> 10.8 IndexSection payload: exact Index-LZ schema",
    },
    RetiredRequirement {
        id: "REQ-688e6e3bcb6bac58",
        section: "> 10.8 IndexSection payload: exact Index-LZ schema",
    },
    RetiredRequirement {
        id: "REQ-9aedd36111445db5",
        section: "> 10.8 IndexSection payload: exact Index-LZ schema",
    },
    RetiredRequirement {
        id: "REQ-76589d84be6cff6e",
        section: "> 10.8 IndexSection payload: exact Index-LZ schema",
    },
    // 10.9 ArchiveSummary payload: exact schema
    RetiredRequirement {
        id: "REQ-2aadfc3581a22ae5",
        section: "> 10.9 ArchiveSummary payload: exact schema",
    },
    RetiredRequirement {
        id: "REQ-06384dd5a3cefe5c",
        section: "> 10.9 ArchiveSummary payload: exact schema",
    },
    RetiredRequirement {
        id: "REQ-a573eb78f600fb6b",
        section: "> 10.9 ArchiveSummary payload: exact schema",
    },
    RetiredRequirement {
        id: "REQ-ac6ddb47d002fbce",
        section: "> 10.9 ArchiveSummary payload: exact schema",
    },
    RetiredRequirement {
        id: "REQ-8d9289fc9f477786",
        section: "> 10.9 ArchiveSummary payload: exact schema",
    },
    RetiredRequirement {
        id: "REQ-e7955c3e645f71f2",
        section: "> 10.9 ArchiveSummary payload: exact schema",
    },
    RetiredRequirement {
        id: "REQ-ddd38cd05f29135d",
        section: "> 10.9 ArchiveSummary payload: exact schema",
    },
    RetiredRequirement {
        id: "REQ-dd226c97ae349467",
        section: "> 10.9 ArchiveSummary payload: exact schema",
    },
    RetiredRequirement {
        id: "REQ-db108b4194904f13",
        section: "> 10.9 ArchiveSummary payload: exact schema",
    },
    RetiredRequirement {
        id: "REQ-3e619050552c5896",
        section: "> 10.9 ArchiveSummary payload: exact schema",
    },
    RetiredRequirement {
        id: "REQ-be7a362636807a08",
        section: "> 10.9 ArchiveSummary payload: exact schema",
    },
    RetiredRequirement {
        id: "REQ-c5a471fca9cd485d",
        section: "> 10.9 ArchiveSummary payload: exact schema",
    },
    RetiredRequirement {
        id: "REQ-e2b79f5fc7b40f20",
        section: "> 10.9 ArchiveSummary payload: exact schema",
    },
    RetiredRequirement {
        id: "REQ-ae9895e7bc37ab85",
        section: "> 10.9 ArchiveSummary payload: exact schema",
    },
    RetiredRequirement {
        id: "REQ-788cacc5ee77c28f",
        section: "> 10.9 ArchiveSummary payload: exact schema",
    },
    RetiredRequirement {
        id: "REQ-eeee1a26d95b1b4b",
        section: "> 10.9 ArchiveSummary payload: exact schema",
    },
    // 10.11 Fixed Trailer: exact 64 bytes
    RetiredRequirement {
        id: "REQ-78e02643c66788ac",
        section: "> 10.11 Fixed Trailer: exact 64 bytes",
    },
    RetiredRequirement {
        id: "REQ-122540ae25fdb059",
        section: "> 10.11 Fixed Trailer: exact 64 bytes",
    },
    RetiredRequirement {
        id: "REQ-d901321aa914c869",
        section: "> 10.11 Fixed Trailer: exact 64 bytes",
    },
    RetiredRequirement {
        id: "REQ-f6a1a75505801138",
        section: "> 10.11 Fixed Trailer: exact 64 bytes",
    },
    RetiredRequirement {
        id: "REQ-6d21fa1df16dd91f",
        section: "> 10.11 Fixed Trailer: exact 64 bytes",
    },
    RetiredRequirement {
        id: "REQ-5bfa31a1bc044e04",
        section: "> 10.11 Fixed Trailer: exact 64 bytes",
    },
    RetiredRequirement {
        id: "REQ-0793ed0523042a9c",
        section: "> 10.11 Fixed Trailer: exact 64 bytes",
    },
    RetiredRequirement {
        id: "REQ-d04f6aa064c1031a",
        section: "> 10.11 Fixed Trailer: exact 64 bytes",
    },
    RetiredRequirement {
        id: "REQ-b0c0dc4a62abb68a",
        section: "> 10.11 Fixed Trailer: exact 64 bytes",
    },
    RetiredRequirement {
        id: "REQ-8fbf1be63d6b0cef",
        section: "> 10.11 Fixed Trailer: exact 64 bytes",
    },
    RetiredRequirement {
        id: "REQ-27366dfcf8b0a60d",
        section: "> 10.11 Fixed Trailer: exact 64 bytes",
    },
    RetiredRequirement {
        id: "REQ-ffc99483930f7a7b",
        section: "> 10.11 Fixed Trailer: exact 64 bytes",
    },
    RetiredRequirement {
        id: "REQ-2bb20846ed1c2a08",
        section: "> 10.11 Fixed Trailer: exact 64 bytes",
    },
    // 13.2 Exact v2 matrix assertions
    RetiredRequirement {
        id: "REQ-3473ce1266119e1e",
        section: "> 13.2 Exact v2 matrix assertions",
    },
    RetiredRequirement {
        id: "REQ-571189ca8bb1175b",
        section: "> 13.2 Exact v2 matrix assertions",
    },
    RetiredRequirement {
        id: "REQ-d2803c9fa9f2e2bf",
        section: "> 13.2 Exact v2 matrix assertions",
    },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuditMode {
    Check,
    Write,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub spec_path: String,
    pub spec_sha256: String,
    pub extractor_version: String,
    pub requirements: Vec<RequirementRecord>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RequirementRecord {
    pub id: String,
    pub heading: String,
    pub kind: String,
    pub canonical_payload_hex: String,
    pub tests: Vec<String>,
    pub evidence: Vec<String>,
    pub status: RequirementStatus,
    pub retirement_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RequirementStatus {
    Active,
    Retired,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Node {
    kind: NodeKind,
    children: Vec<Node>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum NodeKind {
    Root,
    Paragraph,
    Heading(u32),
    List { ordered: bool, start: u32 },
    Item,
    CodeBlock { language: String },
    HtmlBlock,
    Table(Vec<u8>),
    TableHead,
    TableRow,
    TableCell,
    Other,
    Text(String),
    Code(String),
    Html(String),
    SoftBreak,
    HardBreak,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Unit {
    pub(crate) heading: String,
    pub(crate) kind: String,
    pub(crate) structural: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TableData {
    alignments: Vec<u8>,
    header: Vec<String>,
    body: Vec<Vec<String>>,
}

pub fn audit_repository(
    root: &Path,
    mode: AuditMode,
) -> Result<RequirementCounts, Box<dyn std::error::Error>> {
    let spec_path = root.join(SPEC_RELATIVE_PATH);
    let manifest_path = root.join(MANIFEST_RELATIVE_PATH);
    let spec = fs::read(&spec_path)?;
    let extracted = extract(std::str::from_utf8(&spec)?)?;
    let spec_sha256 = hex(Sha256::digest(&spec));
    if spec_sha256 != MAPPED_SPEC_SHA256 {
        return Err(format!(
            "spec changed ({spec_sha256}); review and update the curated requirement mappings before auditing"
        )
        .into());
    }
    let records = expected_records(&extracted, root)?;
    let retired = records
        .iter()
        .filter(|record| record.status == RequirementStatus::Retired)
        .count();
    let counts = RequirementCounts {
        extracted: extracted.len(),
        active: extracted.len() - retired,
        retired,
    };
    let expected = Manifest {
        spec_path: SPEC_RELATIVE_PATH.to_owned(),
        spec_sha256,
        extractor_version: EXTRACTOR_VERSION.to_owned(),
        requirements: records,
    };
    if mode == AuditMode::Write {
        if let Some(parent) = manifest_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&expected)? + "\n";
        fs::write(&manifest_path, json)?;
        return Ok(counts);
    }
    let actual: Manifest = serde_json::from_slice(&fs::read(&manifest_path)?)?;
    validate_manifest(root, &actual, &extracted, &expected.spec_sha256)?;
    Ok(counts)
}

fn expected_records(
    extracted: &[Unit],
    root: &Path,
) -> Result<Vec<RequirementRecord>, Box<dyn std::error::Error>> {
    extracted
        .iter()
        .map(|unit| record_for_unit(unit, root))
        .collect()
}

fn record_for_unit(
    unit: &Unit,
    root: &Path,
) -> Result<RequirementRecord, Box<dyn std::error::Error>> {
    let payload = canonical_payload(&unit.heading, &unit.kind, &unit.structural);
    let digest = Sha256::digest(&payload);
    let digest_hex = hex(digest);
    let id = format!("REQ-{}", &digest_hex[..16]);
    let mapping = mapping_for_unit(unit, &id)?;
    for evidence in &mapping.evidence {
        checked_evidence_path(root, evidence)?;
    }
    Ok(RequirementRecord {
        id: format!("REQ-{}", &digest_hex[..16]),
        heading: unit.heading.clone(),
        kind: unit.kind.clone(),
        canonical_payload_hex: digest_payload_hex(&payload),
        tests: mapping.tests,
        evidence: mapping.evidence,
        status: mapping.status,
        retirement_reason: mapping.retirement_reason,
    })
}

#[derive(Debug)]
struct TraceabilityMapping {
    tests: Vec<String>,
    evidence: Vec<String>,
    status: RequirementStatus,
    retirement_reason: Option<String>,
}

fn mapping_for_unit(
    unit: &Unit,
    id: &str,
) -> Result<TraceabilityMapping, Box<dyn std::error::Error>> {
    // These are semantic mapping rules, not a heading-title fallback.  The section and
    // subsection are the stable scope; payload words select the narrowest real witness
    // where a section contains more than one independent behavior.
    let heading = unit.heading.as_str();
    let payload = String::from_utf8_lossy(&unit.structural).to_ascii_lowercase();
    let mut tests = Vec::new();

    // Retirement is requirement-level and data-driven: only the exact canonical
    // IDs in the curated table are retired, each bound to its reviewed section.
    // There is deliberately no heading-prefix rule, so a mixed requirement such
    // as 10.0 (operational/resource limits still live) or 10.10 (checksum
    // implementations still live) stays active unless a human adds its ID.
    if let Some(section) = retirement_section(id, heading)? {
        let mut evidence = vec![SPEC_RELATIVE_PATH.to_owned()];
        // The exact-v2 matrix assertions are documented in the traceability
        // checklist; every NGv2 wire layout row is documented in the archived
        // FORMAT.md provenance text.
        if section.starts_with("> 13.2") {
            evidence.push("docs/acceptance-traceability.md".to_owned());
        } else {
            evidence.push("docs/FORMAT.md".to_owned());
        }
        return Ok(TraceabilityMapping {
            tests,
            evidence,
            status: RequirementStatus::Retired,
            retirement_reason: Some(NG_V2_RETIRED_REASON.to_owned()),
        });
    }

    let mut evidence = vec![SPEC_RELATIVE_PATH.to_owned()];

    let add = |tests: &mut Vec<String>, evidence: &mut Vec<String>, test: &str, path: &str| {
        tests.push(test.to_owned());
        if !evidence.iter().any(|item| item == path) {
            evidence.push(path.to_owned());
        }
    };

    if heading == "SREP Capability-Fidelity Design"
        || heading.starts_with("SREP Capability-Fidelity Design > 1.")
    {
        add(
            &mut tests,
            &mut evidence,
            "all_methods_are_routed_through_their_real_finders",
            "tests/m0.rs",
        );
        add(
            &mut tests,
            &mut evidence,
            "ordinary_m1_m2_round_trip_with_identical_semantic_ir_across_layouts_and_checksums",
            "tests/stage5.rs",
        );
    } else if heading.contains("> 2.1 Archive dispatch") {
        add(
            &mut tests,
            &mut evidence,
            "legacy_signature_is_recognized_and_header_is_returned",
            "src/dispatch.rs",
        );
        add(
            &mut tests,
            &mut evidence,
            "prototype_v1_cli_rejection_uses_stable_exit_code",
            "tests/cli.rs",
        );
    } else if heading.contains("> 2.2 Embedded and split indexes") {
        add(
            &mut tests,
            &mut evidence,
            "sidecar_is_checked_after_configuration_and_never_opened",
            "tests/cli.rs",
        );
        add(
            &mut tests,
            &mut evidence,
            "committed_legacy_matrix_is_hermetic_and_complete",
            "tests/legacy.rs",
        );
    } else if heading.contains("> 2.3 CLI compatibility") {
        add(
            &mut tests,
            &mut evidence,
            "layout_and_checksum_cli_round_trips",
            "tests/cli.rs",
        );
        add(
            &mut tests,
            &mut evidence,
            "m1_and_m2_cli_select_real_finders_and_round_trip",
            "tests/cli.rs",
        );
    } else if heading.starts_with("SREP Capability-Fidelity Design > 3.") {
        add(
            &mut tests,
            &mut evidence,
            "candidate_encoder_round_trips_all_layouts_with_same_ir",
            "tests/stage3.rs",
        );
        add(
            &mut tests,
            &mut evidence,
            "default_config_is_m3_index_xxh3",
            "src/config.rs",
        );
    } else if heading.contains("> 4.3 REP overlay configuration") {
        add(
            &mut tests,
            &mut evidence,
            "overlay_matrix_round_trips_with_identical_ir",
            "tests/stage6.rs",
        );
        add(
            &mut tests,
            &mut evidence,
            "m5_overlay_relations_have_complete_matrix_and_threshold_specific_matches",
            "tests/stage7.rs",
        );
    } else if heading.starts_with("SREP Capability-Fidelity Design > 4.") {
        add(
            &mut tests,
            &mut evidence,
            "default_config_is_m3_index_xxh3",
            "src/config.rs",
        );
        add(
            &mut tests,
            &mut evidence,
            "independent_boundary_oracles_cover_edge_lengths_and_threshold_hits",
            "tests/stage5.rs",
        );
    } else if heading.contains("> 5.1 Polynomial rolling hash") {
        add(
            &mut tests,
            &mut evidence,
            "polynomial_hash_uses_wrapping_base_fold",
            "tests/polynomial.rs",
        );
        add(
            &mut tests,
            &mut evidence,
            "wrapping_is_explicit_for_long_inputs",
            "src/polynomial.rs",
        );
    } else if heading.contains("> 5.2 Internal strong digest") {
        add(
            &mut tests,
            &mut evidence,
            "m1_and_m2_find_equal_chunks_with_collision_safe_confirmation",
            "src/match_finder/cdc.rs",
        );
        add(
            &mut tests,
            &mut evidence,
            "m3_digest_collision_requires_exact_bytes_in_the_production_loop",
            "src/match_finder/fixed.rs",
        );
    } else if heading.starts_with("SREP Capability-Fidelity Design > 6.") {
        add(
            &mut tests,
            &mut evidence,
            "weighted_schedule_beats_greedy_longest_overlap",
            "src/match_ir.rs",
        );
        add(
            &mut tests,
            &mut evidence,
            "normalizer_uses_global_weighted_schedule_and_canonical_origin_ids",
            "tests/stage3.rs",
        );
    } else if heading.starts_with("SREP Capability-Fidelity Design > 7.") {
        add(
            &mut tests,
            &mut evidence,
            "ram_index_deduplicates_exact_identity_and_orders_candidates",
            "tests/candidate_index.rs",
        );
        add(
            &mut tests,
            &mut evidence,
            "every_finder_has_identical_candidates_and_archives_when_spilled",
            "tests/stage8.rs",
        );
    } else if heading.contains("> 8.1 m0 / REP") {
        add(
            &mut tests,
            &mut evidence,
            "m0_emits_independently_extended_overlapping_candidates",
            "tests/m0.rs",
        );
        add(
            &mut tests,
            &mut evidence,
            "oracle_matches_multiple_representatives_and_overlap",
            "tests/m0_oracle.rs",
        );
    } else if heading.contains("> 8.2 m1 / rolling CDC")
        || heading.contains("> 8.3 m2 / order-1 CDC")
    {
        add(
            &mut tests,
            &mut evidence,
            "independent_candidate_oracle_matches_m1_and_m2_order_and_distance",
            "tests/stage5.rs",
        );
        add(
            &mut tests,
            &mut evidence,
            "m2_reset_does_not_replay_trigger_byte",
            "src/match_finder/cdc.rs",
        );
    } else if heading.contains("> 8.4 m3 / fixed digest") || heading.contains("> 8.5 m4 / reread") {
        add(
            &mut tests,
            &mut evidence,
            "fixed_finders_match_independent_oracles_for_grid_overlap_and_rounding",
            "tests/stage6.rs",
        );
        add(
            &mut tests,
            &mut evidence,
            "acceleration_on_off_matches_independent_oracle_and_releases",
            "src/match_finder/fixed.rs",
        );
    } else if heading.contains("> 8.6 m5 / exhaustive") {
        add(
            &mut tests,
            &mut evidence,
            "exhaustive_candidates_match_independent_oracle_for_nonaligned_repeats",
            "tests/stage7.rs",
        );
        add(
            &mut tests,
            &mut evidence,
            "m5_real_generator_matches_oracle_for_every_snapshot_fallback_state",
            "src/match_finder/m5.rs",
        );
    } else if heading.contains("> 8.7 REP overlay execution") {
        add(
            &mut tests,
            &mut evidence,
            "m5_overlay_keeps_base_first_and_uses_shared_effective_normalization",
            "tests/stage7.rs",
        );
        add(
            &mut tests,
            &mut evidence,
            "effective_minimum_is_the_lower_validated_base_or_overlay_minimum",
            "tests/stage6.rs",
        );
    } else if heading.starts_with("SREP Capability-Fidelity Design > 8.") {
        add(
            &mut tests,
            &mut evidence,
            "all_methods_are_routed_through_their_real_finders",
            "tests/m0.rs",
        );
    } else if heading.starts_with("SREP Capability-Fidelity Design > 9.") {
        add(
            &mut tests,
            &mut evidence,
            "candidate_encoder_round_trips_all_layouts_with_same_ir",
            "tests/stage3.rs",
        );
        add(
            &mut tests,
            &mut evidence,
            "empty_archives_match_spec_both_algorithms",
            "src/codec.rs",
        );
    } else if heading.starts_with("SREP Capability-Fidelity Design > 10.") {
        // The only NGv2 section-10 units that remain active are the two mixed
        // requirements whose retained halves are NGv3-essential: 10.0 keeps the
        // operational/resource limits, and 10.10 keeps the checksum IDs,
        // implementations, CLI selector, and golden vectors. Genuinely obsolete
        // NGv2 wire rows are handled by the curated retirement table above.
        if heading.contains("> 10.0 Wire limits versus operational limits") {
            add(
                &mut tests,
                &mut evidence,
                "temp_budget_failure_does_not_leave_a_visible_run",
                "tests/candidate_index.rs",
            );
            add(
                &mut tests,
                &mut evidence,
                "legacy_temp_limit_is_enforced_and_released",
                "tests/legacy.rs",
            );
            add(
                &mut tests,
                &mut evidence,
                "result_budget_failure_leaves_existing_index_unchanged",
                "tests/candidate_index.rs",
            );
            evidence.push("src/resource.rs".to_owned());
            evidence.push("docs/FORMAT-V3.md".to_owned());
        } else if heading.contains("> 10.10 Checksum IDs") {
            add(
                &mut tests,
                &mut evidence,
                "xxh3_oneshot_matches_independent_c_goldens",
                "tests/checksum_vectors.rs",
            );
            add(
                &mut tests,
                &mut evidence,
                "xxh3_streaming_digest_matches_independent_c_goldens_across_chunkings",
                "tests/checksum_vectors.rs",
            );
            add(
                &mut tests,
                &mut evidence,
                "encode_xxh3_matches_fixture_word_order",
                "tests/checksum_vectors.rs",
            );
            add(
                &mut tests,
                &mut evidence,
                "blake3_official_vectors_and_v3_serialization",
                "tests/checksum_vectors.rs",
            );
            evidence.push("docs/checksum-audit.md".to_owned());
            evidence.push("scripts/checksum-goldens.py".to_owned());
            evidence.push("src/checksum.rs".to_owned());
        } else {
            return Err(format!(
                "active section-10 requirement has no curated mapping: {heading} ({id})"
            )
            .into());
        }
    } else if heading.starts_with("SREP Capability-Fidelity Design > 11.") {
        add(
            &mut tests,
            &mut evidence,
            "committed_legacy_matrix_is_hermetic_and_complete",
            "tests/legacy.rs",
        );
        add(
            &mut tests,
            &mut evidence,
            "all_v4_fixture_footer_mutations_are_corrupt_and_internal_edits_are_observable",
            "tests/legacy.rs",
        );
    } else if heading.starts_with("SREP Capability-Fidelity Design > 12.") {
        add(
            &mut tests,
            &mut evidence,
            "display_uses_specified_shape",
            "src/error.rs",
        );
        add(
            &mut tests,
            &mut evidence,
            "corruption_does_not_leave_partial_destination",
            "tests/cli.rs",
        );
    } else if heading.starts_with("SREP Capability-Fidelity Design > 13.") {
        add(
            &mut tests,
            &mut evidence,
            "ordinary_m1_m2_round_trip_with_identical_semantic_ir_across_layouts_and_checksums",
            "tests/stage5.rs",
        );
        add(
            &mut tests,
            &mut evidence,
            "ordinary_m0_archives_are_byte_identical_across_layouts_checksums_and_repeats",
            "tests/stage4.rs",
        );
    } else if heading.starts_with("SREP Capability-Fidelity Design > 14 ") {
        add(
            &mut tests,
            &mut evidence,
            "canonical_payload_preserves_binary_length_prefixes",
            "src/requirement.rs",
        );
        add(
            &mut tests,
            &mut evidence,
            "table_schema_binds_header_and_order_but_rows_remain_stable",
            "src/requirement.rs",
        );
        if payload.contains("collision") || payload.contains("16-hex") {
            add(
                &mut tests,
                &mut evidence,
                "synthetic_collision_id_uses_full_digest_suffix",
                "src/requirement.rs",
            );
        }
    } else if heading.starts_with("SREP Capability-Fidelity Design > 15 ") {
        add(
            &mut tests,
            &mut evidence,
            "marked_nested_list_is_structural",
            "src/requirement.rs",
        );
    } else if heading.starts_with("SREP Capability-Fidelity Design > 16 ")
        || heading.starts_with("SREP Capability-Fidelity Design > 16.")
        || heading.starts_with("SREP Capability-Fidelity Design > 17 ")
        || heading.starts_with("SREP Capability-Fidelity Design > 17.")
    {
        add(
            &mut tests,
            &mut evidence,
            "fixture_manifest_is_repository_relative_and_complete",
            "tests/legacy.rs",
        );
        evidence.push("docs/acceptance-traceability.md".to_owned());
    } else if heading.starts_with("SREP Capability-Fidelity Design > 18.") {
        add(
            &mut tests,
            &mut evidence,
            "prototype_v1_is_unsupported_version",
            "src/dispatch.rs",
        );
    } else if heading.starts_with("SREP Capability-Fidelity Design > Appendix A:") {
        add(
            &mut tests,
            &mut evidence,
            "committed_legacy_matrix_is_hermetic_and_complete",
            "tests/legacy.rs",
        );
    } else if heading.starts_with("SREP Capability-Fidelity Design > Appendix B:") {
        add(
            &mut tests,
            &mut evidence,
            "canonical_payload_preserves_binary_length_prefixes",
            "src/requirement.rs",
        );
    } else {
        return Err(format!("no curated traceability mapping for heading {heading}").into());
    }

    Ok(TraceabilityMapping {
        tests,
        evidence,
        status: RequirementStatus::Active,
        retirement_reason: None,
    })
}

/// Looks up the curated retirement policy by exact canonical ID and verifies
/// the reviewed section still matches the extracted heading. A known ID whose
/// heading moved is policy drift and is rejected rather than silently retired.
fn retirement_section(
    id: &str,
    heading: &str,
) -> Result<Option<&'static str>, Box<dyn std::error::Error>> {
    for entry in RETIRED_NG_V2_WIRE_REQUIREMENTS {
        if entry.id == id {
            if !heading.ends_with(entry.section) {
                return Err(format!(
                    "retirement policy drift for {}: expected section ending {:?}, extracted heading {heading:?}",
                    id, entry.section
                )
                .into());
            }
            return Ok(Some(entry.section));
        }
    }
    Ok(None)
}

/// Counts of the three audited populations. The audit never claims that every
/// extracted requirement is "validated" or fully satisfied; it reports how many
/// canonical units were extracted, how many are active obligations, and how
/// many are curated historical retirements.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequirementCounts {
    pub extracted: usize,
    pub active: usize,
    pub retired: usize,
}

pub fn requirement_counts(root: &Path) -> Result<RequirementCounts, Box<dyn std::error::Error>> {
    let spec = fs::read(root.join(SPEC_RELATIVE_PATH))?;
    let extracted = extract(std::str::from_utf8(&spec)?)?;
    let records = expected_records(&extracted, root)?;
    let retired = records
        .iter()
        .filter(|record| record.status == RequirementStatus::Retired)
        .count();
    Ok(RequirementCounts {
        extracted: extracted.len(),
        active: extracted.len() - retired,
        retired,
    })
}

fn valid_id(id: &str) -> bool {
    let Some(rest) = id.strip_prefix("REQ-") else {
        return false;
    };
    let mut pieces = rest.split('-');
    let Some(prefix) = pieces.next() else {
        return false;
    };
    let suffix = pieces.next();
    pieces.next().is_none()
        && prefix.len() == 16
        && prefix
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        && suffix.is_none_or(|value| {
            value.len() == 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        })
}

fn decode_hex(value: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    if value.is_empty() || !value.len().is_multiple_of(2) {
        return Err("canonical payload hex must be nonempty and have even length".into());
    }
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let high = hex_digit(pair[0]).ok_or("canonical payload contains non-hex data")?;
        let low = hex_digit(pair[1]).ok_or("canonical payload contains non-hex data")?;
        decoded.push((high << 4) | low);
    }
    Ok(decoded)
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn checked_evidence_path(root: &Path, relative: &str) -> Result<(), Box<dyn std::error::Error>> {
    let path = Path::new(relative);
    if relative.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!("evidence path is not repository-relative: {relative}").into());
    }
    let root = fs::canonicalize(root)?;
    let candidate = root.join(path);
    let evidence =
        fs::canonicalize(&candidate).map_err(|_| format!("missing evidence path {relative}"))?;
    if !evidence.starts_with(&root) || !evidence.is_file() {
        return Err(format!("missing evidence path {relative}").into());
    }
    Ok(())
}

/// One-shot index of every integration-test file stem and every Rust
/// `#[test]` function name in the crate. Building it once avoids rescanning the
/// whole `src`/`tests` tree for every mapped witness, which otherwise dominates
/// audit time. It changes no policy: a witness still has to be a real file stem
/// or a real `#[test]` name.
struct TestIndex {
    file_stems: BTreeSet<String>,
    test_names: BTreeSet<String>,
}

impl TestIndex {
    fn build(root: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let mut stems = BTreeSet::new();
        let tests_dir = root.join("tests");
        if tests_dir.is_dir() {
            for entry in fs::read_dir(&tests_dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.extension().is_some_and(|extension| extension == "rs")
                    && let Some(stem) = path.file_stem().and_then(|stem| stem.to_str())
                {
                    stems.insert(stem.to_owned());
                }
            }
        }
        let mut names = BTreeSet::new();
        for directory in [root.join("src"), root.join("tests")] {
            if !directory.is_dir() {
                continue;
            }
            let mut pending = vec![directory];
            while let Some(path) = pending.pop() {
                for entry in fs::read_dir(path)? {
                    let entry = entry?;
                    let path = entry.path();
                    if path.is_dir() {
                        pending.push(path);
                    } else if path.extension().is_some_and(|extension| extension == "rs") {
                        collect_rust_test_names(&fs::read_to_string(path)?, &mut names);
                    }
                }
            }
        }
        Ok(TestIndex {
            file_stems: stems,
            test_names: names,
        })
    }

    fn contains(&self, test: &str) -> bool {
        self.file_stems.contains(test) || self.test_names.contains(test)
    }
}

fn validate_mapped_tests(
    index: &TestIndex,
    tests: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    for test in tests {
        if test.is_empty() || test.contains('/') || test.contains('\\') {
            return Err(format!("invalid mapped test name {test}").into());
        }
        if index.contains(test) {
            continue;
        }
        return Err(format!("mapped Rust test does not exist: {test}").into());
    }
    Ok(())
}

fn collect_rust_test_names(source: &str, out: &mut BTreeSet<String>) {
    let source = strip_rust_comments_and_strings(source);
    let mut cursor = 0;
    while let Some(marker) = source[cursor..].find("#[test]") {
        let start = cursor + marker + "#[test]".len();
        let Some(fn_offset) = source[start..].find("fn") else {
            return;
        };
        let fn_start = start + fn_offset;
        let before = source.as_bytes()[..fn_start].last().copied();
        let after = source.as_bytes().get(fn_start + 2).copied();
        if before.is_none_or(|byte| !is_rust_ident_byte(byte))
            && after.is_none_or(|byte| !is_rust_ident_byte(byte))
        {
            let name_start = fn_start + 2;
            let name = source[name_start..]
                .trim_start()
                .split(|character: char| !is_rust_ident_byte(character as u8))
                .next()
                .unwrap_or("");
            out.insert(name.to_owned());
        }
        cursor = start;
    }
}

#[cfg(test)]
fn rust_test_name_exists(source: &str, wanted: &str) -> bool {
    let mut names = BTreeSet::new();
    collect_rust_test_names(source, &mut names);
    names.contains(wanted)
}

fn is_rust_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn strip_rust_comments_and_strings(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut output = bytes.to_vec();
    let mut index = 0;
    let mut block_depth = 0usize;
    while index < bytes.len() {
        if block_depth > 0 {
            if bytes.get(index..index + 2) == Some(b"/*") {
                block_depth += 1;
                output[index..index + 2].fill(b' ');
                index += 2;
            } else if bytes.get(index..index + 2) == Some(b"*/") {
                block_depth -= 1;
                output[index..index + 2].fill(b' ');
                index += 2;
            } else {
                if bytes[index] != b'\n' {
                    output[index] = b' ';
                }
                index += 1;
            }
        } else if bytes.get(index..index + 2) == Some(b"//") {
            output[index..index + 2].fill(b' ');
            index += 2;
            while index < bytes.len() && bytes[index] != b'\n' {
                output[index] = b' ';
                index += 1;
            }
        } else if bytes.get(index..index + 2) == Some(b"/*") {
            block_depth = 1;
            output[index..index + 2].fill(b' ');
            index += 2;
        } else if let Some((prefix_len, hashes)) = raw_string_start(bytes, index) {
            let content_start = index + prefix_len;
            let terminator_len = hashes + 1;
            output[index..content_start].fill(b' ');
            index = content_start;
            while index < bytes.len() {
                if bytes[index] == b'"'
                    && bytes
                        .get(index + 1..index + 1 + hashes)
                        .is_some_and(|suffix| suffix.iter().all(|byte| *byte == b'#'))
                {
                    output[index..index + terminator_len].fill(b' ');
                    index += terminator_len;
                    break;
                }
                if bytes[index] != b'\n' {
                    output[index] = b' ';
                }
                index += 1;
            }
        } else if bytes[index] == b'"' {
            output[index] = b' ';
            index += 1;
            while index < bytes.len() {
                let escaped = index > 0 && bytes[index - 1] == b'\\';
                if bytes[index] == b'"' && !escaped {
                    output[index] = b' ';
                    index += 1;
                    break;
                }
                if bytes[index] != b'\n' {
                    output[index] = b' ';
                }
                index += 1;
            }
        } else {
            index += 1;
        }
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn raw_string_start(bytes: &[u8], index: usize) -> Option<(usize, usize)> {
    let mut prefix_len = if bytes.get(index) == Some(&b'r') {
        1
    } else if bytes.get(index..index + 2) == Some(b"br") {
        2
    } else {
        return None;
    };
    let mut hashes = 0;
    while bytes.get(index + prefix_len + hashes) == Some(&b'#') {
        hashes += 1;
    }
    if bytes.get(index + prefix_len + hashes) == Some(&b'"') {
        prefix_len += hashes + 1;
        Some((prefix_len, hashes))
    } else {
        None
    }
}

fn validate_manifest(
    root: &Path,
    actual: &Manifest,
    extracted: &[Unit],
    spec_sha256: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if actual.spec_path != SPEC_RELATIVE_PATH
        || actual.extractor_version != EXTRACTOR_VERSION
        || actual.spec_sha256 != spec_sha256
    {
        return Err("requirement manifest metadata is stale".into());
    }
    if actual.requirements.len() != extracted.len() {
        return Err(format!(
            "manifest requirement count differs: {} != {}",
            actual.requirements.len(),
            extracted.len()
        )
        .into());
    }
    let expected = expected_records(extracted, root)?;
    let test_index = TestIndex::build(root)?;
    let mut ids = BTreeSet::new();
    for (index, (record, expected_record)) in
        actual.requirements.iter().zip(expected.iter()).enumerate()
    {
        if record != expected_record {
            return Err(format!(
                "stale or altered requirement at index {index}: {}",
                record.id
            )
            .into());
        }
        if !ids.insert(record.id.clone()) {
            return Err(format!("duplicate requirement id {}", record.id).into());
        }
        if !valid_id(&record.id)
            || record.canonical_payload_hex.is_empty()
            || !record
                .canonical_payload_hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || record.canonical_payload_hex.len() % 2 != 0
        {
            return Err(format!("invalid lowercase payload hex for {}", record.id).into());
        }
        let decoded = decode_hex(&record.canonical_payload_hex)?;
        let expected_payload = decode_hex(&expected_record.canonical_payload_hex)?;
        if decoded != expected_payload {
            return Err(format!("canonical payload bytes differ for {}", record.id).into());
        }
        for evidence in &record.evidence {
            checked_evidence_path(root, evidence)?;
        }
        if record.tests.is_empty() && record.evidence.is_empty() {
            return Err(format!("empty traceability mapping for {}", record.id).into());
        }
        match record.status {
            RequirementStatus::Active => {
                if record.retirement_reason.is_some() {
                    return Err(format!(
                        "active requirement {} cannot have a retirement reason",
                        record.id
                    )
                    .into());
                }
            }
            RequirementStatus::Retired => {
                // Only the exact curated IDs may be retired, and they must use
                // exactly the approved reason. Any other heading, ID, or reason
                // is rejected so retirement cannot be broadened or renamed.
                if retirement_section(&record.id, &record.heading)?.is_none() {
                    return Err(format!(
                        "requirement {} is not in the curated NGv2 retirement policy",
                        record.id
                    )
                    .into());
                }
                if record.tests.iter().any(|test| !test.is_empty()) {
                    return Err(format!(
                        "retired requirement {} must not claim active test witnesses",
                        record.id
                    )
                    .into());
                }
                if record.retirement_reason.as_deref() != Some(NG_V2_RETIRED_REASON) {
                    return Err(format!(
                        "retired requirement {} must use the exact approved retirement reason",
                        record.id
                    )
                    .into());
                }
            }
        }
        validate_mapped_tests(&test_index, &record.tests)?;
    }
    Ok(())
}

pub(crate) fn extract(markdown: &str) -> Result<Vec<Unit>, Box<dyn std::error::Error>> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    let events: Vec<_> = Parser::new_ext(markdown, options).collect();
    let root = event_tree(events)?;
    let mut units = Vec::new();
    let mut headings = Vec::new();
    extract_scope(&root.children, &mut headings, &mut units)?;
    ensure_unique_units(&units)?;
    Ok(units)
}

fn event_tree<'a>(events: Vec<Event<'a>>) -> Result<Node, Box<dyn std::error::Error>> {
    let mut stack = vec![Node {
        kind: NodeKind::Root,
        children: Vec::new(),
    }];
    for event in events {
        match event {
            Event::Start(tag) => stack.push(Node {
                kind: node_kind(tag)?,
                children: Vec::new(),
            }),
            Event::End(_) => {
                let node = stack.pop().ok_or("unbalanced Markdown end event")?;
                stack
                    .last_mut()
                    .ok_or("unbalanced Markdown root")?
                    .children
                    .push(node);
            }
            Event::Text(value) => push_leaf(&mut stack, NodeKind::Text(value.into_string()))?,
            Event::Code(value) => push_leaf(&mut stack, NodeKind::Code(value.into_string()))?,
            Event::Html(value) | Event::InlineHtml(value) => {
                push_leaf(&mut stack, NodeKind::Html(value.into_string()))?
            }
            Event::SoftBreak => push_leaf(&mut stack, NodeKind::SoftBreak)?,
            Event::HardBreak => push_leaf(&mut stack, NodeKind::HardBreak)?,
            Event::Rule => push_leaf(&mut stack, NodeKind::Other)?,
            Event::InlineMath(value) | Event::DisplayMath(value) => {
                push_leaf(&mut stack, NodeKind::Code(value.into_string()))?
            }
            Event::FootnoteReference(_) | Event::TaskListMarker(_) => {
                push_leaf(&mut stack, NodeKind::Other)?
            }
        }
    }
    if stack.len() != 1 {
        return Err("unbalanced Markdown start event".into());
    }
    stack.pop().ok_or_else(|| "missing Markdown root".into())
}

fn push_leaf(stack: &mut [Node], kind: NodeKind) -> Result<(), Box<dyn std::error::Error>> {
    stack
        .last_mut()
        .ok_or("missing Markdown parent")?
        .children
        .push(Node {
            kind,
            children: Vec::new(),
        });
    Ok(())
}

fn node_kind(tag: Tag<'_>) -> Result<NodeKind, Box<dyn std::error::Error>> {
    Ok(match tag {
        Tag::Paragraph => NodeKind::Paragraph,
        Tag::Heading { level, .. } => NodeKind::Heading(level as u32),
        Tag::List(start) => NodeKind::List {
            ordered: start.is_some(),
            start: start.unwrap_or(1) as u32,
        },
        Tag::Item => NodeKind::Item,
        Tag::CodeBlock(kind) => NodeKind::CodeBlock {
            language: match kind {
                CodeBlockKind::Indented => String::new(),
                CodeBlockKind::Fenced(value) => value.into_string(),
            },
        },
        Tag::HtmlBlock => NodeKind::HtmlBlock,
        Tag::Table(alignments) => {
            NodeKind::Table(alignments.into_iter().map(alignment_code).collect())
        }
        Tag::TableHead => NodeKind::TableHead,
        Tag::TableRow => NodeKind::TableRow,
        Tag::TableCell => NodeKind::TableCell,
        _ => NodeKind::Other,
    })
}

fn extract_scope(
    nodes: &[Node],
    headings: &mut Vec<String>,
    units: &mut Vec<Unit>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut index = 0;
    let mut table_ordinals = BTreeMap::<String, u32>::new();
    while index < nodes.len() {
        if let NodeKind::Heading(level) = nodes[index].kind {
            let text = normalize_leaf(&plain_text(&nodes[index]))?;
            headings.truncate(level.saturating_sub(1) as usize);
            headings.push(text);
            index += 1;
            continue;
        }
        if let Some(kind) = hard_start(&nodes[index]) {
            let end = find_hard_end(nodes, index + 1)?;
            let content = &nodes[index + 1..end];
            if content.iter().any(contains_marker) {
                return Err("nested hard-unit marker".into());
            }
            let heading = heading_path(headings);
            let table_ordinal = content
                .iter()
                .find(|node| matches!(node.kind, NodeKind::Table(_)))
                .map(|_| next_table_ordinal(&mut table_ordinals, &heading));
            extract_marked(kind, content, &heading, table_ordinal, units)?;
            index = end + 1;
            continue;
        }
        if hard_table_marker(nodes, index) {
            let heading = heading_path(headings);
            let ordinal = next_table_ordinal(&mut table_ordinals, &heading);
            let table = table_from_node(&nodes[index])?;
            emit_table(table, &heading, ordinal, units)?;
            index += 1;
            continue;
        }
        if node_html(&nodes[index]).trim_end_matches('\n') == "<!-- /hard-unit -->" {
            return Err("orphan hard-unit end marker".into());
        }
        if matches!(nodes[index].kind, NodeKind::Table(_)) {
            let heading = heading_path(headings);
            let _ = next_table_ordinal(&mut table_ordinals, &heading);
        }
        extract_unmarked_node(&nodes[index], &heading_path(headings), units)?;
        index += 1;
    }
    Ok(())
}

fn hard_start(node: &Node) -> Option<&str> {
    match &node.kind {
        NodeKind::Html(_) | NodeKind::HtmlBlock => node_html(node)
            .strip_suffix('\n')
            .unwrap_or_else(|| node_html(node))
            .strip_prefix("<!-- hard-unit kind=\"")?
            .strip_suffix("\" -->"),
        _ => None,
    }
}

fn find_hard_end(nodes: &[Node], start: usize) -> Result<usize, Box<dyn std::error::Error>> {
    for (index, node) in nodes.iter().enumerate().skip(start) {
        if node_html(node).trim_end_matches('\n') == "<!-- /hard-unit -->" {
            return Ok(index);
        }
        if hard_start(node).is_some() {
            return Err("nested hard-unit marker".into());
        }
    }
    Err("unclosed hard-unit marker".into())
}

fn hard_table_marker(nodes: &[Node], index: usize) -> bool {
    index > 0
        && node_html(&nodes[index - 1]).trim_end_matches('\n') == "<!-- hard-requirements -->"
        && matches!(
            nodes.get(index).map(|node| &node.kind),
            Some(NodeKind::Table(_))
        )
}

fn extract_marked(
    kind: &str,
    content: &[Node],
    heading: &str,
    table_ordinal: Option<u32>,
    units: &mut Vec<Unit>,
) -> Result<(), Box<dyn std::error::Error>> {
    if !matches!(kind, "paragraph" | "list" | "formula" | "table") {
        return Err(format!("unknown hard-unit kind {kind}").into());
    }
    match kind {
        "paragraph" => {
            let text = content.iter().map(plain_text).collect::<String>();
            units.push(Unit {
                heading: heading.to_owned(),
                kind: "paragraph".to_owned(),
                structural: normalize_leaf(&text)?.into_bytes(),
            });
        }
        "list" => {
            let list = content
                .iter()
                .find(|node| matches!(node.kind, NodeKind::List { .. }))
                .ok_or("list hard-unit has no list")?;
            units.push(Unit {
                heading: heading.to_owned(),
                kind: "list".to_owned(),
                structural: encode_list(list, 0)?,
            });
        }
        "formula" => {
            let code = content
                .iter()
                .find_map(|node| match &node.kind {
                    NodeKind::CodeBlock { .. } => Some(code_text(node)),
                    _ => None,
                })
                .ok_or("formula hard-unit has no code block")?;
            units.push(Unit {
                heading: heading.to_owned(),
                kind: "formula".to_owned(),
                structural: code.nfc().collect::<String>().into_bytes(),
            });
        }
        "table" => {
            let table = content
                .iter()
                .find(|node| matches!(node.kind, NodeKind::Table(_)))
                .ok_or("table hard-unit has no table")?;
            emit_table(
                table_from_node(table)?,
                heading,
                table_ordinal.ok_or("missing table ordinal")?,
                units,
            )?;
        }
        _ => unreachable!(),
    }
    Ok(())
}

fn extract_unmarked_node(
    node: &Node,
    heading: &str,
    units: &mut Vec<Unit>,
) -> Result<(), Box<dyn std::error::Error>> {
    match node.kind {
        NodeKind::Paragraph => {
            let text = plain_text(node);
            if contains_must(&text) {
                units.push(Unit {
                    heading: heading.to_owned(),
                    kind: "paragraph".to_owned(),
                    structural: normalize_leaf(&text)?.into_bytes(),
                });
            }
        }
        NodeKind::List { .. } => {
            for item in node
                .children
                .iter()
                .filter(|child| matches!(child.kind, NodeKind::Item))
            {
                let text = item_inline_text(item);
                if contains_must(&text) {
                    units.push(Unit {
                        heading: heading.to_owned(),
                        kind: "list-item".to_owned(),
                        structural: normalize_leaf(&text)?.into_bytes(),
                    });
                }
                for child in &item.children {
                    if matches!(child.kind, NodeKind::List { .. }) {
                        extract_unmarked_node(child, heading, units)?;
                    }
                }
            }
        }
        NodeKind::Heading(_)
        | NodeKind::CodeBlock { .. }
        | NodeKind::Table(_)
        | NodeKind::Html(_) => {}
        _ => {}
    }
    Ok(())
}

fn emit_table(
    table: TableData,
    heading: &str,
    ordinal: u32,
    units: &mut Vec<Unit>,
) -> Result<(), Box<dyn std::error::Error>> {
    if table.header.is_empty() || table.body.iter().any(|row| row.len() != table.header.len()) {
        return Err("hard table has invalid header/body cell structure".into());
    }
    let rows: Vec<Vec<u8>> = table
        .body
        .iter()
        .map(|row| encode_row(row))
        .collect::<Result<_, _>>()?;
    let mut structural = Vec::new();
    structural.extend_from_slice(b"SREP-REQ-TSCH");
    push_u32(&mut structural, 1);
    push_u32(&mut structural, ordinal);
    push_u32(&mut structural, table.header.len() as u32);
    for (alignment, cell) in table.alignments.iter().zip(&table.header) {
        structural.push(*alignment);
        push_string(&mut structural, cell)?;
    }
    push_u32(&mut structural, rows.len() as u32);
    for (ordinal, row) in rows.iter().enumerate() {
        push_u32(&mut structural, ordinal as u32);
        push_u32(&mut structural, row.len() as u32);
        structural.extend_from_slice(&Sha256::digest(row));
    }
    units.push(Unit {
        heading: heading.to_owned(),
        kind: "table-schema".to_owned(),
        structural,
    });
    for row in rows {
        units.push(Unit {
            heading: heading.to_owned(),
            kind: "table-row".to_owned(),
            structural: row,
        });
    }
    Ok(())
}

fn table_from_node(node: &Node) -> Result<TableData, Box<dyn std::error::Error>> {
    let NodeKind::Table(alignments) = &node.kind else {
        return Err("expected table".into());
    };
    let mut header = Vec::new();
    let mut body = Vec::new();
    for row in node
        .children
        .iter()
        .filter(|child| matches!(child.kind, NodeKind::TableHead | NodeKind::TableRow))
    {
        let cells: Vec<String> = row
            .children
            .iter()
            .filter(|child| matches!(child.kind, NodeKind::TableCell))
            .map(|cell| normalize_leaf(&plain_text(cell)))
            .collect::<Result<_, _>>()?;
        if matches!(row.kind, NodeKind::TableHead) {
            header = cells;
        } else {
            body.push(cells);
        }
    }
    Ok(TableData {
        alignments: alignments.clone(),
        header,
        body,
    })
}

fn encode_row(row: &[String]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut bytes = Vec::new();
    push_u32(&mut bytes, row.len() as u32);
    for cell in row {
        push_string(&mut bytes, cell)?;
    }
    Ok(bytes)
}

fn encode_list(node: &Node, depth: u32) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let NodeKind::List { ordered, start } = node.kind else {
        return Err("expected list".into());
    };
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"SREP-REQ-LIST");
    push_u32(&mut bytes, 1);
    bytes.push(u8::from(ordered));
    push_u32(&mut bytes, start);
    let items: Vec<_> = node
        .children
        .iter()
        .filter(|child| matches!(child.kind, NodeKind::Item))
        .collect();
    push_u32(&mut bytes, items.len() as u32);
    for (ordinal, item) in items.into_iter().enumerate() {
        push_u32(&mut bytes, ordinal as u32);
        push_u32(&mut bytes, depth);
        let blocks: Vec<_> = item
            .children
            .iter()
            .filter(|child| !matches!(child.kind, NodeKind::Other))
            .collect();
        let has_direct_text = blocks.iter().any(|child| {
            matches!(
                child.kind,
                NodeKind::Text(_) | NodeKind::Code(_) | NodeKind::SoftBreak | NodeKind::HardBreak
            )
        });
        let block_count = blocks
            .iter()
            .filter(|child| {
                matches!(
                    child.kind,
                    NodeKind::Paragraph | NodeKind::List { .. } | NodeKind::CodeBlock { .. }
                )
            })
            .count()
            + usize::from(has_direct_text);
        push_u32(&mut bytes, u32::try_from(block_count)?);
        if has_direct_text {
            bytes.push(1);
            push_string(&mut bytes, &normalize_leaf(&item_inline_text(item))?)?;
        }
        for block in blocks {
            match block.kind {
                NodeKind::Paragraph => {
                    bytes.push(1);
                    push_string(&mut bytes, &normalize_leaf(&plain_text(block))?)?;
                }
                NodeKind::List { .. } => {
                    bytes.push(2);
                    bytes.extend_from_slice(&encode_list(block, depth + 1)?);
                }
                NodeKind::CodeBlock { .. } => {
                    bytes.push(3);
                    push_string_bytes(&mut bytes, block_code_nfc(block).as_bytes())?;
                }
                NodeKind::Text(_)
                | NodeKind::Code(_)
                | NodeKind::SoftBreak
                | NodeKind::HardBreak => {}
                _ => return Err("unsupported block in marked list".into()),
            }
        }
    }
    Ok(bytes)
}

fn block_code_nfc(node: &Node) -> String {
    code_text(node).nfc().collect()
}
fn code_text(node: &Node) -> String {
    node.children
        .iter()
        .map(|child| match &child.kind {
            NodeKind::Text(value) => value.clone(),
            _ => code_text(child),
        })
        .collect()
}

fn plain_text(node: &Node) -> String {
    match &node.kind {
        NodeKind::Text(value) | NodeKind::Code(value) => value.clone(),
        NodeKind::SoftBreak | NodeKind::HardBreak => " ".to_owned(),
        NodeKind::Html(_) | NodeKind::HtmlBlock => String::new(),
        _ => node.children.iter().map(plain_text).collect(),
    }
}

fn item_inline_text(node: &Node) -> String {
    node.children
        .iter()
        .filter(|child| !matches!(child.kind, NodeKind::List { .. }))
        .map(plain_text)
        .collect()
}

fn contains_marker(node: &Node) -> bool {
    node_html(node).contains("hard-unit")
}

fn node_html(node: &Node) -> &str {
    match &node.kind {
        NodeKind::Html(value) => value,
        NodeKind::HtmlBlock => node
            .children
            .iter()
            .find_map(|child| match &child.kind {
                NodeKind::Html(value) => Some(value.as_str()),
                _ => None,
            })
            .unwrap_or(""),
        _ => "",
    }
}
fn contains_must(text: &str) -> bool {
    let bytes = text.as_bytes();
    text.match_indices("MUST").any(|(start, _)| {
        let end = start + 4;
        (start == 0 || !bytes[start - 1].is_ascii_alphabetic())
            && (end == bytes.len() || !bytes[end].is_ascii_alphabetic())
    })
}

fn heading_path(headings: &[String]) -> String {
    headings.join(" > ")
}

fn next_table_ordinal(ordinals: &mut BTreeMap<String, u32>, heading: &str) -> u32 {
    let ordinal = ordinals.get(heading).copied().unwrap_or(0);
    ordinals.insert(heading.to_owned(), ordinal.saturating_add(1));
    ordinal
}

fn normalize_leaf(value: &str) -> Result<String, Box<dyn std::error::Error>> {
    let normalized: String = value.nfc().collect();
    let mut output = String::new();
    let mut in_space = false;
    for character in normalized.chars() {
        if character.is_whitespace() {
            in_space = true;
        } else {
            if in_space && !output.is_empty() {
                output.push(' ');
            }
            output.push(character);
            in_space = false;
        }
    }
    Ok(output.trim_matches(' ').to_owned())
}

fn canonical_payload(heading: &str, kind: &str, structural: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    push_u32(&mut bytes, heading.len() as u32);
    bytes.extend_from_slice(heading.as_bytes());
    push_u32(&mut bytes, kind.len() as u32);
    bytes.extend_from_slice(kind.as_bytes());
    bytes.extend_from_slice(structural);
    bytes
}

fn digest_payload_hex(payload: &[u8]) -> String {
    hex(payload)
}
fn push_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}
fn push_string(bytes: &mut Vec<u8>, value: &str) -> Result<(), Box<dyn std::error::Error>> {
    push_string_bytes(bytes, value.as_bytes())
}
fn push_string_bytes(bytes: &mut Vec<u8>, value: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    let length = u32::try_from(value.len())?;
    push_u32(bytes, length);
    bytes.extend_from_slice(value);
    Ok(())
}
fn alignment_code(alignment: Alignment) -> u8 {
    match alignment {
        Alignment::None => 0,
        Alignment::Left => 1,
        Alignment::Center => 2,
        Alignment::Right => 3,
    }
}
fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}
fn ensure_unique_units(units: &[Unit]) -> Result<(), Box<dyn std::error::Error>> {
    let mut seen = BTreeMap::<(String, String, String), Vec<u8>>::new();
    let mut prefixes = BTreeMap::<String, Vec<u8>>::new();
    for unit in units {
        let payload = canonical_payload(&unit.heading, &unit.kind, &unit.structural);
        let key = (
            unit.heading.clone(),
            unit.kind.clone(),
            digest_payload_hex(&payload),
        );
        if seen.insert(key, payload.clone()).is_some() {
            return Err("identical same-heading same-kind requirement".into());
        }
        let id_prefix = hex(Sha256::digest(&payload))[..16].to_owned();
        if let Some(previous) = prefixes.insert(id_prefix, payload.clone())
            && previous == payload
        {
            return Err("duplicate requirement payload".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_payload_preserves_binary_length_prefixes() {
        let structural = {
            let mut bytes = Vec::new();
            push_u32(&mut bytes, 1);
            push_u32(&mut bytes, 200);
            bytes.extend(std::iter::repeat_n(b'A', 200));
            bytes
        };
        let payload = canonical_payload("ExtractorGolden", "table-row", &structural);
        assert_eq!(payload.len(), 240);
        assert_eq!(&payload[36..40], &[0xc8, 0, 0, 0]);
        assert_eq!(
            hex(Sha256::digest(&payload)),
            "2ecd3c7354df617f7d6300d1b21e4716f43dbe8b84097f3d2fbb83bac5519db1"
        );
    }

    #[test]
    fn marked_nested_list_is_structural() {
        let source = "# H\n<!-- hard-unit kind=\"list\" -->\n- one\n  - two\n<!-- /hard-unit -->\n";
        let units = extract(source).unwrap();
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].kind, "list");
        assert!(units[0].structural.starts_with(b"SREP-REQ-LIST"));
    }

    #[test]
    fn table_rows_are_separate_and_schema_binds_membership() {
        let units = extract("# H\n<!-- hard-unit kind=\"table\" -->\n| A | B |\n|---|---|\n| x | y |\n| p | q |\n<!-- /hard-unit -->\n").unwrap();
        assert_eq!(
            units
                .iter()
                .map(|unit| unit.kind.as_str())
                .collect::<Vec<_>>(),
            vec!["table-schema", "table-row", "table-row"]
        );
        assert_eq!(
            units[1].structural,
            [2, 0, 0, 0, 1, 0, 0, 0, b'x', 1, 0, 0, 0, b'y']
        );
    }

    #[test]
    fn table_schema_binds_header_and_order_but_rows_remain_stable() {
        let first = extract(
            "# H\n<!-- hard-unit kind=\"table\" -->\n| A | B |\n|---|---|\n| x | y |\n| p | q |\n<!-- /hard-unit -->\n",
        )
        .unwrap();
        let reordered = extract(
            "# H\n<!-- hard-unit kind=\"table\" -->\n| A | B |\n|---|---|\n| p | q |\n| x | y |\n<!-- /hard-unit -->\n",
        )
        .unwrap();
        assert_ne!(first[0].structural, reordered[0].structural);
        assert_eq!(first[1].structural, reordered[2].structural);
        assert_eq!(first[2].structural, reordered[1].structural);
    }

    #[test]
    fn malformed_markers_are_rejected() {
        assert!(extract("<!-- hard-unit kind=\"unknown\" -->\ntext\n<!-- /hard-unit -->").is_err());
        assert!(extract("<!-- hard-unit kind=\"paragraph\" -->\ntext").is_err());
        assert!(extract("<!-- /hard-unit -->\ntext").is_err());
    }

    #[test]
    fn synthetic_collision_id_uses_full_digest_suffix() {
        let first = b"first payload";
        let second = b"second payload";
        let first_digest = Sha256::digest(first);
        let second_digest = Sha256::digest(second);
        let id = format!("REQ-{}-{}", &hex(first_digest)[..16], hex(second_digest));
        assert!(id.starts_with("REQ-"));
        assert_eq!(id.len(), 4 + 16 + 1 + 64);
    }

    #[test]
    fn rust_test_name_parser_ignores_comments_and_strings() {
        let source = r##"
            // #[test] fn fake_from_comment() {}
            const TEXT: &str = "#[test] fn fake_from_string() {}";
            #[test]
            fn real_test_name() {}
        "##;
        assert!(rust_test_name_exists(source, "real_test_name"));
        assert!(!rust_test_name_exists(source, "fake_from_comment"));
        assert!(!rust_test_name_exists(source, "fake_from_string"));
    }

    #[test]
    fn requirement_ids_accept_only_the_specified_lowercase_grammar() {
        assert!(valid_id("REQ-0123456789abcdef"));
        assert!(valid_id(
            "REQ-0123456789abcdef-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        ));
        assert!(!valid_id("REQ-0123456789ABCDEf"));
        assert!(!valid_id("REQ-0123456789abcdef-0123"));
        assert!(!valid_id("REQ-0123456789abcdef-extra-more"));
    }

    fn repository_root() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    fn load_repository() -> (std::path::PathBuf, Vec<Unit>, String, Manifest) {
        let root = repository_root();
        let spec = fs::read(root.join(SPEC_RELATIVE_PATH)).unwrap();
        let extracted = extract(std::str::from_utf8(&spec).unwrap()).unwrap();
        let sha = hex(Sha256::digest(&spec));
        let actual: Manifest =
            serde_json::from_slice(&fs::read(root.join(MANIFEST_RELATIVE_PATH)).unwrap()).unwrap();
        (root, extracted, sha, actual)
    }

    #[test]
    fn retirement_policy_is_exact_and_requirement_level() {
        // Positive: a genuinely obsolete NGv2 wire row is retired only by its
        // exact ID and exact historical section.
        assert_eq!(
            retirement_section(
                "REQ-30189c7adaccc542",
                "SREP Capability-Fidelity Design > 10. SREP-NG v2 wire format > 10.1 ArchiveHeader: exact 80 bytes",
            )
            .unwrap(),
            Some("> 10.1 ArchiveHeader: exact 80 bytes")
        );

        // Strong negatives: the two mixed requirements whose retained halves are
        // live must NOT be retired by the classifier, even though they sit in
        // section 10. This is the core bug the explicit policy fixes.
        assert_eq!(
            retirement_section(
                "REQ-be9f15856c6da519",
                "SREP Capability-Fidelity Design > 10. SREP-NG v2 wire format > 10.10 Checksum IDs and implementation research decision",
            )
            .unwrap(),
            None
        );
        assert_eq!(
            retirement_section(
                "REQ-9c1de6fb88dc4c57",
                "SREP Capability-Fidelity Design > 10. SREP-NG v2 wire format > 10.10 Checksum IDs and implementation research decision",
            )
            .unwrap(),
            None
        );
        assert_eq!(
            retirement_section(
                "REQ-8251cc2a92108692",
                "SREP Capability-Fidelity Design > 10. SREP-NG v2 wire format > 10.0 Wire limits versus operational limits",
            )
            .unwrap(),
            None
        );

        // Any ID outside the curated list is authoritative nothing, whatever its
        // heading. Section membership alone never retires a requirement.
        for fabricated in ["REQ-0000000000000000", "REQ-deadbeefdeadbeef"] {
            assert_eq!(
                retirement_section(
                    fabricated,
                    "SREP Capability-Fidelity Design > 10. SREP-NG v2 wire format > 10.1 ArchiveHeader: exact 80 bytes",
                )
                .unwrap(),
                None
            );
        }

        // A curated ID whose heading drifted is policy drift, not retirement.
        assert!(
            retirement_section(
                "REQ-30189c7adaccc542",
                "SREP Capability-Fidelity Design > 10. SREP-NG v2 wire format > 10.2 Record framing and cardinality",
            )
            .is_err()
        );
    }

    #[test]
    fn manifest_retires_only_curated_ids_with_the_approved_reason() {
        let (root, extracted, sha, actual) = load_repository();
        validate_manifest(&root, &actual, &extracted, &sha).unwrap();

        let retired: Vec<_> = actual
            .requirements
            .iter()
            .filter(|record| record.status == RequirementStatus::Retired)
            .collect();
        assert_eq!(retired.len(), 152);
        for record in &retired {
            assert!(record.tests.is_empty());
            assert_eq!(
                record.retirement_reason.as_deref(),
                Some(NG_V2_RETIRED_REASON)
            );
            assert!(
                retirement_section(&record.id, &record.heading)
                    .unwrap()
                    .is_some()
            );
        }
        // 10.0 and both 10.10 rows stay active, and no checksum/operational ID
        // is retired.
        for id in [
            "REQ-8251cc2a92108692",
            "REQ-be9f15856c6da519",
            "REQ-9c1de6fb88dc4c57",
        ] {
            let record = actual
                .requirements
                .iter()
                .find(|record| record.id == id)
                .unwrap();
            assert_eq!(record.status, RequirementStatus::Active, "{id}");
            assert!(record.retirement_reason.is_none(), "{id}");
        }
    }

    #[test]
    fn classifier_rejects_retiring_active_and_mixed_requirements() {
        let (root, extracted, sha, actual) = load_repository();
        let mutate = |id: &str, status: RequirementStatus, reason: Option<&str>| {
            let mut clone = actual.clone();
            let record = clone
                .requirements
                .iter_mut()
                .find(|record| record.id == id)
                .unwrap();
            record.status = status;
            record.retirement_reason = reason.map(str::to_owned);
            record.tests.clear();
            record.evidence = vec![SPEC_RELATIVE_PATH.to_owned()];
            validate_manifest(&root, &clone, &extracted, &sha)
        };

        // Attempting to retire either checksum ID, the mixed 10.0 operational
        // requirement, or any requirement outside the curated set must fail.
        for id in [
            "REQ-be9f15856c6da519",
            "REQ-9c1de6fb88dc4c57",
            "REQ-8251cc2a92108692",
        ] {
            assert!(
                mutate(id, RequirementStatus::Retired, Some(NG_V2_RETIRED_REASON)).is_err(),
                "retiring {id} must be rejected"
            );
        }

        // A curated ID is still rejected when paired with an arbitrary reason.
        assert!(
            mutate(
                "REQ-30189c7adaccc542",
                RequirementStatus::Retired,
                Some("NGv2 was retired for convenience"),
            )
            .is_err()
        );

        // A curated ID with the wrong section is rejected as policy drift even
        // when the ID and reason are otherwise correct.
        let mut drifted = actual.clone();
        if let Some(record) = drifted
            .requirements
            .iter_mut()
            .find(|record| record.id == "REQ-30189c7adaccc542")
        {
            record.heading = "SREP Capability-Fidelity Design > 10. SREP-NG v2 wire format > 10.2 Record framing and cardinality".to_owned();
            record.retirement_reason = Some(NG_V2_RETIRED_REASON.to_owned());
        }
        assert!(validate_manifest(&root, &drifted, &extracted, &sha).is_err());

        // A retired row that claims an active test witness is rejected.
        let mut witnessed = actual.clone();
        if let Some(record) = witnessed
            .requirements
            .iter_mut()
            .find(|record| record.id == "REQ-30189c7adaccc542")
        {
            record.tests = vec!["header_roundtrip_and_error_kinds".to_owned()];
        }
        assert!(validate_manifest(&root, &witnessed, &extracted, &sha).is_err());
    }

    #[test]
    fn classifier_accepts_only_the_actual_retired_rows() {
        let (root, extracted, sha, actual) = load_repository();
        // The unmutated manifest is valid: the actual retired v2 header/record
        // rows are accepted as retired.
        validate_manifest(&root, &actual, &extracted, &sha).unwrap();

        // Flipping a genuinely retired row to active (with the retired reason
        // removed) is rejected because the extracted expectation is retired.
        let mut flipped = actual.clone();
        if let Some(record) = flipped
            .requirements
            .iter_mut()
            .find(|record| record.id == "REQ-30189c7adaccc542")
        {
            record.status = RequirementStatus::Active;
            record.retirement_reason = None;
        }
        assert!(validate_manifest(&root, &flipped, &extracted, &sha).is_err());
    }

    #[test]
    fn altered_payload_or_id_is_rejected() {
        let (root, extracted, sha, actual) = load_repository();
        let mut altered_payload = actual.clone();
        altered_payload.requirements[0]
            .canonical_payload_hex
            .replace_range(0..2, "00");
        assert!(validate_manifest(&root, &altered_payload, &extracted, &sha).is_err());

        let mut altered_id = actual.clone();
        altered_id.requirements[0].id = "REQ-0123456789abcdef".to_owned();
        assert!(validate_manifest(&root, &altered_id, &extracted, &sha).is_err());

        let mut bad_id = actual.clone();
        bad_id.requirements[0].id = "REQ-0123456789ABCDEF".to_owned();
        assert!(validate_manifest(&root, &bad_id, &extracted, &sha).is_err());
    }

    #[test]
    fn repository_counts_match_the_manifest() {
        let (root, _, _, actual) = load_repository();
        let counts = requirement_counts(&root).unwrap();
        assert_eq!(counts.extracted, 515);
        assert_eq!(counts.active, 363);
        assert_eq!(counts.retired, 152);
        assert_eq!(counts.extracted, actual.requirements.len());
        let retired = actual
            .requirements
            .iter()
            .filter(|record| record.status == RequirementStatus::Retired)
            .count();
        assert_eq!(counts.retired, retired);
        assert_eq!(counts.active + counts.retired, counts.extracted);
    }
}
