use super::*;

/// A `CopRegistrant` with every field at a neutral default, ready for
/// per-test customization via struct-update syntax -- mirrors
/// `bind_frame_tests::registrant`'s own role in this file.
fn registrant(matches_got: u32, pending_rc: Option<u8>) -> CopRegistrant {
    CopRegistrant {
        cop_handle: 100,
        registration_seq: 0,
        tier: RegistrantTier::ActiveSendReceive,
        expected: Vec::new(),
        rc_cfg: None,
        request_sid: None,
        matches_needed: Some(2),
        matches_got,
        pending_rc,
        connect_generation: 1,
        cyclic_deadline: None,
        cyclic_timeout_ms: None,
        migrate_on_first_match: false,
        timing_cfg: None,
        timing_accumulator: None,
        pending_timing_change: None,
        concat_enabled: false,
        concat: Vec::new(),
        concat_segments_got: 0,
    }
}

fn baseline(matches_got: u32, pending_rc: Option<u8>) -> RegistrantBaseline {
    RegistrantBaseline {
        cop_handle: 100,
        matches_got,
        pending_rc,
        concat_segments_got: 0,
        timing_accumulator: None,
    }
}

/// Required test 1: two independent passes (e.g. primary + UUDT
/// companion channel, ADR-046), each baselined at 0 and each
/// independently accepting its own +1 match, merged back sequentially --
/// the live count sums to 2, not 1 (the absolute-overwrite bug ADR-101
/// fixes: a second merge from a snapshot cloned before the first merge
/// landed would otherwise stomp the first pass's contribution).
#[test]
fn matches_got_delta_merge_sums_two_independent_passes() {
    let mut live = registrant(0, None);

    // Pass 1 (e.g. primary channel): baseline 0, snapshot accepted +1.
    let snap1 = registrant(1, None);
    let base1 = baseline(0, None);
    merge_registrant_writeback(&mut live, &snap1, &base1);
    assert_eq!(live.matches_got, 1);

    // Pass 2 (e.g. companion channel), cloned from live BEFORE pass 1's
    // merge landed -- its own baseline is therefore also 0, and it also
    // independently accepted its own +1.
    let snap2 = registrant(1, None);
    let base2 = baseline(0, None);
    merge_registrant_writeback(&mut live, &snap2, &base2);

    assert_eq!(
        live.matches_got, 2,
        "each pass's own delta must be summed, not overwritten by the later pass's absolute count"
    );
}

/// Fail-without-the-fix control for test 1: an absolute overwrite
/// (`live.matches_got = snap.matches_got`, ADR-101's Context section --
/// the exact pre-fix behavior) applied in the same two-pass sequence
/// loses pass 1's contribution, sticking at 1 instead of 2. Pinned here,
/// alongside the real merge function, so a future regression back to an
/// overwrite is caught by inspection of this test's own contrast, not
/// just by trusting the fixed formula.
#[test]
fn absolute_overwrite_would_have_lost_the_first_passs_contribution() {
    let mut live = registrant(0, None);

    // Pass 1: same inputs as the test above.
    live.matches_got = registrant(1, None).matches_got;
    assert_eq!(live.matches_got, 1);

    // Pass 2: an absolute overwrite from a snapshot cloned before pass
    // 1's own writeback landed re-applies pass 2's own absolute count
    // (1), discarding pass 1's contribution entirely.
    live.matches_got = registrant(1, None).matches_got;

    assert_eq!(
        live.matches_got, 1,
        "documents the bug ADR-101 fixes: overwrite loses the concurrent pass's contribution"
    );
}

/// Required test 2, "real" case: baseline `None`, snapshot transitioned
/// to `Some(code)` during this pass, live is currently `None` -- the
/// transition merges in correctly.
#[test]
fn pending_rc_none_to_some_transition_merges_in() {
    let mut live = registrant(0, None);
    let snap = registrant(0, Some(0x78));
    let base = baseline(0, None);

    merge_registrant_writeback(&mut live, &snap, &base);

    assert_eq!(live.pending_rc, Some(0x78));
}

/// Required test 2, stale-snapshot case: a snapshot taken BEFORE a
/// consumer reset (baseline `Some(x)`, snapshot unchanged at `Some(x)`)
/// races a consumer reset that lands first, resetting live to `None`.
/// The merge must not resurrect the stale `x` -- live stays `None`.
#[test]
fn pending_rc_baseline_gate_does_not_resurrect_a_stale_snapshot() {
    let mut live = registrant(0, None); // reset by a racing consumer
    let snap = registrant(0, Some(0x78)); // unchanged since before the reset
    let base = baseline(0, Some(0x78)); // this pass's own baseline was already Some

    merge_registrant_writeback(&mut live, &snap, &base);

    assert_eq!(
        live.pending_rc, None,
        "a snapshot whose own baseline was already Some must not resurrect a stale code \
             over a live None a racing consumer reset legitimately produced"
    );
}

/// Fail-without-the-fix control for test 2: a simpler "only overwrite
/// None -> Some" rule WITHOUT the baseline gate (the naive fix ADR-101's
/// Decision section explicitly rejects as insufficient) would resurrect
/// the stale code in exactly the scenario the test above pins.
#[test]
fn ungated_none_to_some_rule_would_resurrect_a_stale_snapshot() {
    let mut live = registrant(0, None); // reset by a racing consumer
    let snap = registrant(0, Some(0x78)); // unchanged since before the reset

    // The naive rule: no baseline consulted at all.
    if live.pending_rc.is_none()
        && let Some(code) = snap.pending_rc
    {
        live.pending_rc = Some(code);
    }

    assert_eq!(
        live.pending_rc,
        Some(0x78),
        "documents the bug an ungated None->Some rule reintroduces: it cannot tell a stale \
             unchanged snapshot from a genuine this-pass transition"
    );
}

/// A live `Some` a different pass (or a consumer reset racing this
/// merge) already established must never be overwritten by this pass's
/// own snapshot, even if this pass's own baseline was `None` -- "first
/// code wins" (`bind_registrant`'s own `if r.pending_rc.is_none()`
/// guard) extends across the writeback merge too.
#[test]
fn pending_rc_never_overwrites_a_live_some_already_set_by_another_pass() {
    let mut live = registrant(0, Some(0x21)); // another pass already won
    let snap = registrant(0, Some(0x78));
    let base = baseline(0, None);

    merge_registrant_writeback(&mut live, &snap, &base);

    assert_eq!(live.pending_rc, Some(0x21));
}

/// Required test 3: a later, live deadline is never pulled backward by
/// an earlier/stale snapshot's own deadline.
#[test]
fn cyclic_deadline_monotone_merge_never_moves_backward() {
    let now = tokio::time::Instant::now();
    let earlier = now;
    let later = now + Duration::from_millis(500);

    let mut live = CopRegistrant {
        cyclic_deadline: Some(later),
        ..registrant(0, None)
    };
    let snap = CopRegistrant {
        cyclic_deadline: Some(earlier),
        ..registrant(0, None)
    };
    let base = baseline(0, None);

    merge_registrant_writeback(&mut live, &snap, &base);

    assert_eq!(
        live.cyclic_deadline,
        Some(later),
        "a stale snapshot's earlier deadline must never pull the live deadline backward"
    );
}

/// The forward direction: a snapshot's later deadline (this pass
/// restarted it on an accepted match) does advance the live deadline.
#[test]
fn cyclic_deadline_monotone_merge_advances_forward() {
    let now = tokio::time::Instant::now();
    let earlier = now;
    let later = now + Duration::from_millis(500);

    let mut live = CopRegistrant {
        cyclic_deadline: Some(earlier),
        ..registrant(0, None)
    };
    let snap = CopRegistrant {
        cyclic_deadline: Some(later),
        ..registrant(0, None)
    };
    let base = baseline(0, None);

    merge_registrant_writeback(&mut live, &snap, &base);

    assert_eq!(live.cyclic_deadline, Some(later));
}

/// Fail-without-the-fix control for test 3: an absolute overwrite
/// (`live.cyclic_deadline = snap.cyclic_deadline`, the pre-fix
/// behavior) applied to the exact same inputs as the backward-merge test
/// above DOES pull the live deadline backward.
#[test]
fn absolute_overwrite_would_have_pulled_the_deadline_backward() {
    let now = tokio::time::Instant::now();
    let earlier = now;
    let later = now + Duration::from_millis(500);

    let mut live = CopRegistrant {
        cyclic_deadline: Some(later),
        ..registrant(0, None)
    };
    let snap = CopRegistrant {
        cyclic_deadline: Some(earlier),
        ..registrant(0, None)
    };

    live.cyclic_deadline = snap.cyclic_deadline; // pre-fix absolute overwrite

    assert_eq!(
        live.cyclic_deadline,
        Some(earlier),
        "documents the bug ADR-101 fixes: overwrite pulls a live deadline backward"
    );
}

// ── CP_EnableConcatenation (ADR-148) ──

/// `concat_segments_got` merges by the same delta pattern as
/// `matches_got` -- a snapshot's own per-pass contribution is summed
/// onto live, not overwritten.
#[test]
fn concat_segments_got_delta_merge_sums_the_passs_own_contribution() {
    let mut live = CopRegistrant {
        concat_segments_got: 1,
        ..registrant(0, None)
    };
    let snap = CopRegistrant {
        concat_segments_got: 3,
        ..registrant(0, None)
    };
    let base = RegistrantBaseline {
        concat_segments_got: 1,
        ..baseline(0, None)
    };

    merge_registrant_writeback(&mut live, &snap, &base);

    assert_eq!(
        live.concat_segments_got, 3,
        "live (1) + this pass's own delta (3 - 1 = 2) = 3"
    );
}

/// The open `concat` buffers themselves are written back from the
/// snapshot, not left stale on `live` -- e.g. a segment absorbed into a
/// buffer this pass must be visible on `live` afterward. Exercised with
/// TWO buffers (ADR-148 Amendment: `concat` is now a `Vec<ConcatBuf>`,
/// IS-MULTIPLE can have more than one open at once) to confirm the whole
/// `Vec`, not just its first element, lands on `live`.
#[test]
fn concat_buffers_are_written_back_from_the_snapshot() {
    let mut live = registrant(0, None);
    let buf_a = ConcatBuf {
        key: (1, None, 0x62),
        timestamp: 0,
        header_bytes: Vec::new(),
        unique_resp_identifier: 1,
        acceptance_id: 77,
        rx_status_flags: 0,
        data: vec![0x62, 0xAA],
        footer_bytes: Vec::new(),
        opened_vacuous: false,
        segments: 1,
    };
    let buf_b = ConcatBuf {
        key: (2, None, 0x62),
        timestamp: 0,
        header_bytes: Vec::new(),
        unique_resp_identifier: 2,
        acceptance_id: 77,
        rx_status_flags: 0,
        data: vec![0x62, 0xCC],
        footer_bytes: Vec::new(),
        opened_vacuous: false,
        segments: 1,
    };
    let snap = CopRegistrant {
        concat: vec![buf_a.clone(), buf_b.clone()],
        concat_segments_got: 2,
        ..registrant(0, None)
    };
    let base = RegistrantBaseline {
        concat_segments_got: 0,
        ..baseline(0, None)
    };

    merge_registrant_writeback(&mut live, &snap, &base);

    assert_eq!(
        live.concat
            .iter()
            .map(|b| b.data.clone())
            .collect::<Vec<_>>(),
        vec![buf_a.data, buf_b.data],
        "the snapshot's own buffers (both of them, in order) must land on live"
    );
}

/// A finalize (currently only the empty-payload-match arm, or the
/// deadline-expiry path) clears the snapshot's own `concat` back to
/// empty -- this must also clear a stale non-empty `Vec` still sitting
/// on `live`.
#[test]
fn concat_buffer_finalize_clears_live_too() {
    let stale_buf = ConcatBuf {
        key: (0, None, 0x62),
        timestamp: 0,
        header_bytes: Vec::new(),
        unique_resp_identifier: 0,
        acceptance_id: 77,
        rx_status_flags: 0,
        data: vec![0x62, 0xAA],
        footer_bytes: Vec::new(),
        opened_vacuous: false,
        segments: 1,
    };
    let mut live = CopRegistrant {
        concat: vec![stale_buf],
        concat_segments_got: 1,
        ..registrant(0, None)
    };
    // This pass's own snapshot already finalized the buffer (e.g. via
    // `finalize_concat_buffers`): `concat` is back to empty, and
    // `matches_got` advanced by 1.
    let snap = CopRegistrant {
        concat: Vec::new(),
        concat_segments_got: 1,
        matches_got: 1,
        ..registrant(0, None)
    };
    let base = RegistrantBaseline {
        concat_segments_got: 1,
        ..baseline(0, None)
    };

    merge_registrant_writeback(&mut live, &snap, &base);

    assert!(
        live.concat.is_empty(),
        "a finalize observed in the snapshot must clear the live buffer too"
    );
    assert_eq!(live.matches_got, 1);
}

/// ADR-150: `timing_accumulator`'s baseline guard -- a stale
/// companion-task pass (one that observed no qualifying response this
/// pass, so its own snapshot is just an unchanged carry-forward of what
/// it started with) must not clobber a fresher primary-task pass's own
/// write for the SAME registrant. Before ADR-150 this field was only
/// ever `Some` on ISO14230 (no UUDT companion channel, ADR-046), so a
/// plain unconditional overwrite from `snap` was safe; ADR-150 extends
/// `timing_cfg` to ISO15765, which DOES have a companion channel, making
/// this race reachable.
///
/// Fail-without-the-fix control: reverting the guard back to
/// `if snap.timing_accumulator.is_some() { live.timing_accumulator = snap.timing_accumulator; }`
/// makes this test fail (`live.timing_accumulator` would read back the
/// companion pass's stale carried-forward value instead of the primary
/// pass's fresher one); the `!=`-against-`base` guard makes it pass.
#[test]
fn merge_registrant_writeback_stale_companion_snapshot_does_not_clobber_fresher_timing_accumulator()
{
    let stale_value = Some(TimingAccumulator::Session {
        p2_ms: 100,
        p2_star_10ms: 10,
    });
    let fresh_value = Some(TimingAccumulator::Session {
        p2_ms: 999,
        p2_star_10ms: 99,
    });

    // The primary task's own pass already landed its fresher write onto
    // `live` (simulating an earlier merge this same batch, or a
    // concurrently-running primary pass that finished first).
    let mut live = CopRegistrant {
        timing_accumulator: fresh_value,
        ..registrant(0, None)
    };
    // The companion task's own snapshot was cloned from `live` BEFORE
    // that fresher write landed -- its own pass observed nothing new
    // this pass, so its snapshot is still exactly its own baseline.
    let snap = CopRegistrant {
        timing_accumulator: stale_value,
        ..registrant(0, None)
    };
    let base = RegistrantBaseline {
        timing_accumulator: stale_value,
        ..baseline(0, None)
    };

    merge_registrant_writeback(&mut live, &snap, &base);

    assert_eq!(
        live.timing_accumulator, fresh_value,
        "an unchanged-since-baseline snapshot must not overwrite a fresher live value"
    );
}

/// Sibling of the test above: when THIS pass's own snapshot genuinely
/// changed the accumulator (differs from its own baseline), the merge
/// still applies it -- the guard only blocks an UNCHANGED snapshot, not
/// a genuine update.
#[test]
fn merge_registrant_writeback_applies_a_genuinely_changed_timing_accumulator() {
    let old_value = Some(TimingAccumulator::Kwp([1, 2, 3, 4, 5]));
    let new_value = Some(TimingAccumulator::Kwp([9, 8, 7, 6, 5]));

    let mut live = CopRegistrant {
        timing_accumulator: old_value,
        ..registrant(0, None)
    };
    let snap = CopRegistrant {
        timing_accumulator: new_value,
        ..registrant(0, None)
    };
    let base = RegistrantBaseline {
        timing_accumulator: old_value,
        ..baseline(0, None)
    };

    merge_registrant_writeback(&mut live, &snap, &base);

    assert_eq!(
        live.timing_accumulator, new_value,
        "a snapshot that genuinely changed since its own baseline must still be applied"
    );
}
