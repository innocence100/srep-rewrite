//! Private bounded-memory compression contracts.
//!
//! This module is intentionally not part of the public API.  The 0.2
//! implementation is assembled behind these owned-row and fallible-cursor
//! contracts so the finder, storage, scheduler, and writer stages cannot
//! accidentally exchange borrowed data from an evictable cache.

pub(crate) mod candidate_sink;
pub(crate) mod contracts;
pub(crate) mod normalize;
pub(crate) mod schedule;
pub(crate) mod session;
pub(crate) mod sort;
pub(crate) mod storage;

#[cfg(test)]
mod tests;

/// Generate complete raw semantics into an independently bounded sink. Base
/// barrier is consumed before overlay construction, returning source loans.
pub(crate) fn enumerate_spooled(
    spool: &crate::codec::InputSpool,
    config: &crate::config::CompressionConfig,
    session: &session::CompressionSession,
    sink: &mut dyn contracts::CandidateSink,
) -> crate::Result<contracts::EnumerationSummary> {
    use crate::config::Method;
    use crate::match_finder::{cdc, fixed, m0, m5};
    use contracts::{EnumerationRequirement, Provenance, SourceBarrier, SourcePhase};
    config.validate()?;
    session.check_cancelled()?;
    let context = session.finder_context();
    let requirement = EnumerationRequirement::new(session, config.rep_overlay.is_some());
    let mut base = SourceBarrier::new(
        crate::candidate_index::new_codec_candidate_index(session)?,
        session,
        SourcePhase::Base,
    );
    let provenance = Provenance::new(config.method.wire_id(), 0)?;
    let result = match config.method {
        Method::M0Rep => m0::emit_matches_m0_spooled(
            spool,
            &m0::M0Parameters {
                min_match: config.min_match,
                region: (config.min_match / 8).max(1),
                max_distance: config.max_distance,
            },
            &context,
            0,
            base.index_mut(),
            session,
            sink,
            provenance,
        ),
        Method::M1RollingCdc | Method::M2Order1Cdc => cdc::emit_matches_spooled_with_digest(
            spool,
            config,
            &context,
            if config.method == Method::M1RollingCdc {
                cdc::CdcMethod::M1
            } else {
                cdc::CdcMethod::M2
            },
            base.index_mut(),
            session,
            sink,
            provenance,
            cdc::blake3_at,
        ),
        Method::M3FixedDigest => fixed::emit_matches_m3_spooled(
            spool,
            config,
            &context,
            session,
            base.index_mut(),
            sink,
            provenance,
            fixed::blake3_at,
        ),
        Method::M4Reread => fixed::emit_matches_m4_spooled(
            spool,
            config,
            &context,
            session,
            base.index_mut(),
            sink,
            provenance,
            fixed::polynomial_at,
        ),
        Method::M5Exhaustive => m5::emit_matches_spooled(
            spool,
            config,
            &context,
            session,
            base.index_mut(),
            sink,
            provenance,
        ),
    };
    let mut next = result?;
    let base_token = base.finish(session)?;
    let overlay_token = if let Some(overlay) = &config.rep_overlay {
        let distance = match config.max_distance {
            Some(n) if n != 0 => n.min(overlay.distance),
            _ => overlay.distance,
        };
        let mut index = SourceBarrier::new(
            crate::candidate_index::new_codec_candidate_index(session)?,
            session,
            SourcePhase::Overlay,
        );
        next = m0::emit_matches_m0_spooled(
            spool,
            &m0::M0Parameters {
                min_match: overlay.min_match,
                region: (overlay.min_match / 8).max(1),
                max_distance: Some(distance),
            },
            &context,
            next,
            index.index_mut(),
            session,
            sink,
            Provenance::new(0, 1)?,
        )?;
        Some(index.finish(session)?)
    } else {
        None
    };
    Ok(contracts::EnumerationSummary {
        next_ordinal: next,
        qualifying_count: next,
        source_integrity: requirement.complete(base_token, overlay_token)?,
    })
}

pub(crate) fn generate_batch(
    spool: &crate::codec::InputSpool,
    config: &crate::config::CompressionConfig,
    session: &session::CompressionSession,
) -> crate::Result<normalize::CandidateBatch> {
    use contracts::CandidateRunSink;
    config.validate()?;
    let mut sink =
        candidate_sink::ExternalCandidateSink::with_input_len(session, spool.len, usize::MAX)?;
    let enumeration = enumerate_spooled(spool, config, session, &mut sink)?;
    normalize::CandidateBatch::from_enumerated(
        sink.finish()?,
        enumeration.source_integrity,
        spool.len,
        session,
    )
}

#[cfg(test)]
mod generation_tests {
    use super::*;
    use crate::config::{CompressionConfig, Method, RepConfig};
    use crate::match_ir::MatchCandidate;
    use crate::resource::ResourceContext;
    use contracts::{CandidateRow, CandidateRunSink, CandidateSink, ReplayStore};
    const METHODS: [Method; 6] = [
        Method::M0Rep,
        Method::M1RollingCdc,
        Method::M2Order1Cdc,
        Method::M3FixedDigest,
        Method::M4Reread,
        Method::M5Exhaustive,
    ];
    struct Rows(Vec<CandidateRow>);
    impl CandidateSink for Rows {
        fn emit(&mut self, r: CandidateRow) -> crate::Result<()> {
            self.0.push(r);
            Ok(())
        }
    }
    fn canonical(mut rows: Vec<MatchCandidate>) -> Vec<MatchCandidate> {
        rows.sort_unstable_by_key(|r| (r.src, r.dst, r.len, r.insertion_ordinal));
        rows.dedup_by_key(|r| (r.src, r.dst, r.len));
        rows
    }
    fn config(method: Method) -> CompressionConfig {
        let mut c = CompressionConfig::for_method(method);
        c.min_match = 32;
        c.block_size = 1024;
        if matches!(method, Method::M3FixedDigest | Method::M4Reread) {
            c.seed_size = Some(16);
        }
        if matches!(method, Method::M1RollingCdc | Method::M2Order1Cdc) {
            c.target_chunk = Some(64);
        }
        c
    }
    fn raw(data: &[u8], c: &CompressionConfig) -> Vec<MatchCandidate> {
        let ctx = ResourceContext::with_resources(&c.resources).unwrap();
        let rows = match c.method {
            Method::M0Rep => crate::find_matches_m0(data, c, &ctx),
            Method::M1RollingCdc => crate::find_matches_m1(data, c, &ctx),
            Method::M2Order1Cdc => crate::find_matches_m2(data, c, &ctx),
            Method::M3FixedDigest => crate::find_matches_m3(data, c, &ctx),
            Method::M4Reread => crate::find_matches_m4(data, c, &ctx),
            Method::M5Exhaustive => crate::find_matches_m5(data, c, &ctx),
        }
        .unwrap();
        rows.iter().copied().collect()
    }
    fn streamed(data: &[u8], c: &CompressionConfig, spill: bool) -> (Vec<MatchCandidate>, u64) {
        let mut ctx = ResourceContext::with_resources(&c.resources).unwrap();
        if spill {
            ctx.candidate_index_memtable_bytes = Some(1);
        }
        let spool = crate::codec::spool_input(data, &c.resources, &ctx).unwrap();
        let s =
            session::CompressionSession::new(&ctx, &c.resources, session::OffloadPolicy::default())
                .unwrap();
        let mut sink = candidate_sink::ExternalCandidateSink::with_input_len(
            &s,
            spool.len,
            if spill { 3 } else { usize::MAX },
        )
        .unwrap();
        let summary = enumerate_spooled(&spool, c, &s, &mut sink).unwrap();
        summary.source_integrity.check_session(&s).unwrap();
        let store = sink.finish().unwrap();
        let mut out = Vec::new();
        {
            let mut cursor = store.cursor().unwrap();
            while let Some(r) = cursor.next().unwrap() {
                out.push(r.candidate);
            }
        }
        let next = summary.next_ordinal;
        assert_eq!(next, summary.qualifying_count);
        if spill
            && data.len() > 100
            && !matches!(c.method, Method::M1RollingCdc | Method::M2Order1Cdc)
        {
            assert!(ctx.candidate_index_spill_count() > 0);
        }
        drop(store);
        drop(spool);
        s.retry_cleanup().unwrap();
        assert_eq!(ctx.temp.current(), 0);
        assert_eq!(s.handle_usage().current, 7);
        drop(s);
        assert_eq!(ctx.memory.current(), 0);
        (out, next)
    }
    fn pseudo(n: usize) -> Vec<u8> {
        let mut x = 17u64;
        (0..n)
            .map(|_| {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
                (x >> 32) as u8
            })
            .collect()
    }
    #[test]
    fn all_six_streams_match_raw_canonical_candidates_distance_and_tail() {
        let block = pseudo(83);
        let repeated = [
            block.as_slice(),
            &[9, 8, 7],
            block.as_slice(),
            block.as_slice(),
        ]
        .concat();
        for method in METHODS {
            for data in [&[][..], &[0; 7][..], repeated.as_slice()] {
                for distance in [None, Some(84)] {
                    let mut c = config(method);
                    c.max_distance = distance;
                    let expected = canonical(raw(data, &c));
                    let (actual, _) = streamed(data, &c, false);
                    assert_eq!(
                        actual,
                        expected,
                        "{method:?} {distance:?} len{}",
                        data.len()
                    );
                }
            }
        }
    }
    #[test]
    fn all_six_forced_spill_and_exact_fallback_use_real_generation() {
        let block = pseudo(80);
        let data = [block.as_slice(), block.as_slice(), block.as_slice()].concat();
        for method in METHODS {
            let c = config(method);
            let expected = canonical(raw(&data, &c));
            let _snapshot = crate::match_finder::snapshot::test_snapshot_limit(0);
            let _fixed = crate::match_finder::fixed::disable_acceleration();
            assert_eq!(streamed(&data, &c, true).0, expected, "{method:?}");
        }
    }
    #[test]
    fn overlay_uses_full_ordinal_and_reuses_source_workspace() {
        let data = b"abcdefghijklmnopqrstuvwxyz012345".repeat(5);
        for method in [
            Method::M3FixedDigest,
            Method::M4Reread,
            Method::M5Exhaustive,
        ] {
            let mut c = config(method);
            c.rep_overlay = Some(RepConfig {
                distance: 128,
                min_match: 32,
            });
            let expected = canonical(raw(&data, &c));
            assert_eq!(streamed(&data, &c, true).0, expected);
            let ctx = ResourceContext::with_resources(&c.resources).unwrap();
            let spool = crate::codec::spool_input(data.as_slice(), &c.resources, &ctx).unwrap();
            let s = session::CompressionSession::new(
                &ctx,
                &c.resources,
                session::OffloadPolicy::default(),
            )
            .unwrap();
            let mut rows = Rows(Vec::new());
            let summary = enumerate_spooled(&spool, &c, &s, &mut rows).unwrap();
            assert_eq!(summary.next_ordinal, rows.0.len() as u64);
            assert!(rows.0.iter().any(|r| r.provenance.phase == 1));
            for (n, r) in rows.0.iter().enumerate() {
                assert_eq!(r.candidate.insertion_ordinal, n as u64);
            }
        }
    }
    #[test]
    fn m0_boundary_witness_and_m4_backward_extension_survive() {
        let mut data = vec![0; 512];
        data.push(1);
        data.extend([0; 513]);
        data.push(1);
        let mut c = config(Method::M0Rep);
        c.min_match = 512;
        let (actual, _) = streamed(&data, &c, false);
        assert!(
            actual
                .iter()
                .any(|r| (r.src, r.dst, r.len) == (0, 514, 513))
        );
        assert_eq!(actual, canonical(raw(&data, &c)));
        let data = [&b"UNALIGNED"[..], &pseudo(79), &b"X"[..], &pseudo(79)].concat();
        let c = config(Method::M4Reread);
        assert_eq!(streamed(&data, &c, false).0, canonical(raw(&data, &c)));
    }
    #[test]
    fn fixed_real_acceleration_branch_matches_disabled_session_path() {
        let mut data = pseudo(256 * 1024);
        let repeat = data[..128].to_vec();
        data[200000..200128].copy_from_slice(&repeat);
        for method in [Method::M3FixedDigest, Method::M4Reread] {
            let mut c = config(method);
            c.min_match = 64;
            c.seed_size = Some(32);
            let expected = canonical(raw(&data, &c));
            let accelerated = streamed(&data, &c, false).0;
            assert_eq!(
                crate::match_finder::fixed::last_acceleration_active(),
                Some(true)
            );
            assert_eq!(accelerated, expected);
            let _denial = crate::match_finder::fixed::deny_acceleration_stage(1);
            let fallback = streamed(&data, &c, false).0;
            assert_eq!(
                crate::match_finder::fixed::last_acceleration_active(),
                Some(false)
            );
            assert_eq!(accelerated, fallback);
        }
    }
    #[test]
    fn batch_completion_fails_on_callback_or_source_barrier_error() {
        use session::{FaultAction, Operation};
        let data = b"abcdefghijklmnopqrstuvwxyz012345".repeat(4);
        let mut c = config(Method::M5Exhaustive);
        c.rep_overlay = Some(RepConfig {
            distance: 128,
            min_match: 32,
        });
        for fail_barrier in [false, true] {
            let mut ctx = ResourceContext::with_resources(&c.resources).unwrap();
            ctx.candidate_index_memtable_bytes = Some(1);
            let spool = crate::codec::spool_input(data.as_slice(), &c.resources, &ctx).unwrap();
            let s = session::CompressionSession::new(
                &ctx,
                &c.resources,
                session::OffloadPolicy::default(),
            )
            .unwrap();
            struct Bad<'a> {
                s: &'a session::CompressionSession,
                barrier: bool,
            }
            impl CandidateSink for Bad<'_> {
                fn emit(&mut self, _: CandidateRow) -> crate::Result<()> {
                    if self.barrier {
                        self.s.inject(Operation::Read, 0, FaultAction::Fail);
                        Ok(())
                    } else {
                        Err(crate::Error::temp_limit("sink failure"))
                    }
                }
            }
            assert!(
                enumerate_spooled(
                    &spool,
                    &c,
                    &s,
                    &mut Bad {
                        s: &s,
                        barrier: fail_barrier
                    }
                )
                .is_err()
            );
            assert!(crate::candidate_index::new_codec_candidate_index(&s).is_err());
            drop(spool);
            s.retry_cleanup().unwrap();
            assert_eq!(ctx.temp.current(), 0);
        }
        let ctx = ResourceContext::with_resources(&c.resources).unwrap();
        let spool = crate::codec::spool_input(data.as_slice(), &c.resources, &ctx).unwrap();
        let s =
            session::CompressionSession::new(&ctx, &c.resources, session::OffloadPolicy::default())
                .unwrap();
        let batch = generate_batch(&spool, &c, &s).unwrap();
        normalize::validate_batch(batch, &spool, c.effective_min_match().unwrap(), &s).unwrap();
    }

    #[test]
    fn raw_sequences_match_private_before_dedup_and_cdc_crosses_blocks() {
        let block = pseudo(96);
        let small = [
            block.as_slice(),
            &[1, 2, 3],
            block.as_slice(),
            block.as_slice(),
        ]
        .concat();
        for method in METHODS {
            let data = if matches!(method, Method::M1RollingCdc | Method::M2Order1Cdc) {
                pseudo(1024).repeat(3)
            } else {
                small.clone()
            };
            let c = config(method);
            let expected = raw(&data, &c);
            let ctx = ResourceContext::with_resources(&c.resources).unwrap();
            let spool = crate::codec::spool_input(data.as_slice(), &c.resources, &ctx).unwrap();
            let s = session::CompressionSession::new(
                &ctx,
                &c.resources,
                session::OffloadPolicy::default(),
            )
            .unwrap();
            let mut rows = Rows(Vec::new());
            let summary = enumerate_spooled(&spool, &c, &s, &mut rows).unwrap();
            assert_eq!(summary.next_ordinal, rows.0.len() as u64);
            let mut actual = rows.0.into_iter().map(|r| r.candidate).collect::<Vec<_>>();
            if method == Method::M5Exhaustive {
                actual = canonical(actual);
                actual.sort_unstable_by_key(|r| r.insertion_ordinal);
            }
            assert_eq!(actual, expected, "raw sequence {method:?}");
        }
        let data = pseudo(1024).repeat(3);
        for method in [Method::M1RollingCdc, Method::M2Order1Cdc] {
            let c = config(method);
            assert_eq!(streamed(&data, &c, true).0, canonical(raw(&data, &c)));
        }
    }

    #[test]
    fn snapshot_fallback_states_and_overlay_denial_preserve_ordinals() {
        let data = b"0123456789abcdef0123456789abcdef".repeat(5);
        for method in [Method::M0Rep, Method::M5Exhaustive] {
            let c = config(method);
            let expected = canonical(raw(&data, &c));
            {
                let _deny = crate::match_finder::snapshot::test_snapshot_memory_budget(0);
                assert_eq!(streamed(&data, &c, false).0, expected);
            }
            {
                let _deny = crate::match_finder::snapshot::test_deny_lces(true, false);
                assert_eq!(streamed(&data, &c, false).0, expected);
            }
            {
                let _deny = crate::match_finder::snapshot::test_deny_lces(false, true);
                assert_eq!(streamed(&data, &c, false).0, expected);
            }
        }
        let mut c = config(Method::M5Exhaustive);
        c.rep_overlay = Some(RepConfig {
            distance: 128,
            min_match: 32,
        });
        let expected = canonical(raw(&data, &c));
        let _deny = crate::match_finder::snapshot::test_snapshot_limit(0);
        assert_eq!(streamed(&data, &c, true).0, expected);
    }

    #[test]
    fn bounded_small_seed_and_randomized_parameter_matrix() {
        for method in METHODS {
            for variant in 0..4 {
                let mut c = config(method);
                c.min_match = if variant == 0 { 2 } else { 26 + variant };
                if matches!(method, Method::M3FixedDigest | Method::M4Reread) {
                    c.seed_size = Some(if variant == 0 { 1 } else { 7 });
                }
                if matches!(method, Method::M1RollingCdc | Method::M2Order1Cdc) {
                    c.target_chunk = Some(32);
                }
                c.max_distance = Some(29 + variant);
                let mut block = pseudo(29);
                block[0] = variant as u8;
                let data = [block.as_slice(), &[99], block.as_slice()].concat();
                assert_eq!(
                    streamed(&data, &c, false).0,
                    canonical(raw(&data, &c)),
                    "{method:?} variant{variant}"
                );
            }
        }
    }

    #[test]
    fn m5_stream_overlay_starts_at_eighty_and_final_source_corruption_denies_batch() {
        let data = [0u8; 64];
        let mut c = config(Method::M5Exhaustive);
        c.rep_overlay = Some(RepConfig {
            distance: 64,
            min_match: 16,
        });
        let ctx = ResourceContext::with_resources(&c.resources).unwrap();
        let spool = crate::codec::spool_input(&data[..], &c.resources, &ctx).unwrap();
        let s =
            session::CompressionSession::new(&ctx, &c.resources, session::OffloadPolicy::default())
                .unwrap();
        let mut rows = Rows(Vec::new());
        let summary = enumerate_spooled(&spool, &c, &s, &mut rows).unwrap();
        assert_eq!(
            rows.0
                .iter()
                .find(|r| r.provenance.phase == 1)
                .unwrap()
                .candidate
                .insertion_ordinal,
            80
        );
        assert_eq!(summary.next_ordinal, rows.0.len() as u64);
        // Real base source corruption introduced by the final base callback:
        // no more target lookups; the completion barrier must reject it before
        // an overlay or CandidateBatch can be constructed.
        c.rep_overlay = None;
        let mut ctx = ResourceContext::with_resources(&c.resources).unwrap();
        ctx.candidate_index_memtable_bytes = Some(1);
        let spool = crate::codec::spool_input(&data[..], &c.resources, &ctx).unwrap();
        let s =
            session::CompressionSession::new(&ctx, &c.resources, session::OffloadPolicy::default())
                .unwrap();
        struct CorruptLast<'a>(&'a session::CompressionSession);
        impl CandidateSink for CorruptLast<'_> {
            fn emit(&mut self, row: CandidateRow) -> crate::Result<()> {
                if row.candidate.insertion_ordinal == 79 {
                    use std::io::{Read, Seek, SeekFrom, Write};
                    let dir = self.0.temp_directory()?;
                    for entry in std::fs::read_dir(&*dir).map_err(crate::Error::temp_storage)? {
                        let path = entry.map_err(crate::Error::temp_storage)?.path();
                        let mut f = std::fs::OpenOptions::new()
                            .read(true)
                            .write(true)
                            .open(path)
                            .map_err(crate::Error::temp_storage)?;
                        let mut header = [0; 128];
                        f.read_exact(&mut header)
                            .map_err(crate::Error::temp_storage)?;
                        if header[14] == 5 {
                            f.seek(SeekFrom::Start(128))
                                .map_err(crate::Error::temp_storage)?;
                            f.write_all(&[255]).map_err(crate::Error::temp_storage)?;
                            break;
                        }
                    }
                }
                Ok(())
            }
        }
        assert!(enumerate_spooled(&spool, &c, &s, &mut CorruptLast(&s)).is_err());
        assert!(crate::candidate_index::new_codec_candidate_index(&s).is_err());
        drop(spool);
        s.retry_cleanup().unwrap();
        assert_eq!(ctx.temp.current(), 0);
    }
}
