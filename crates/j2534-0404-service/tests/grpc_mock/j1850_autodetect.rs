//! `SAE_J1850` VPW/PWM auto-detect probe at `ConnectComLogicalLink` (ADR-070).
//!
//! Resources 0x021C (`ISO_15031_5_on_SAE_J1850`, OBD-active probe) and 0x021A
//! (`SAE_J2190_on_SAE_J1850`, passive-only probe) both live on the combined
//! `SAE_J1850` bus (0x0307) and resolve their VPW-vs-PWM flavor at connect
//! time instead of a fixed connect protocol. `server.backdoor.connect_count()`
//! after a full `create_and_connect_cll` gives the *real* physical channel's
//! id, since it is always the most recent successful `PassThruConnect` --
//! whatever probe connects preceded it are earlier, lower channel ids.

use serial_test::serial;
use vci_service_interface::{ParamItem, SetComParamRequest, param_item};

use crate::harness::*;

#[tokio::test]
#[serial]
async fn vpw_bus_responds_connects_as_j1850vpw() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    server
        .backdoor
        .set_j1850_bus_flavor(Some(j2534_0404::J1850VPW));

    let cll_handle = create_and_connect_cll(&mut client, 0x021C, &[]).await;
    let channel_id = server.backdoor.connect_count() as u32;

    assert_eq!(server.backdoor.baud_rate(channel_id), 10_400);
    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE).await,
        10_400
    );

    send_data(&mut client, cll_handle, vec![0x01, 0x00], vec![]).await;
    assert_eq!(
        server.backdoor.written_protocol_id(channel_id, 0),
        j2534_0404::J1850VPW
    );

    server.shutdown().await;
}

/// A Codex review finding on the auto-detect commit: only `hw_protocol_id`
/// flipped to `J1850PWM` on a PWM win, but `tx_message_size_range` (and
/// header construction) still keyed off the service-level `protocol`
/// (`SAE_J2190_ON_SAE_J1850`/`ISO_15031_5_ON_SAE_J1850`), whose
/// `j2534_protocol_id()` is permanently `J1850VPW` (the "initial candidate",
/// ADR-070) -- so a PWM-detected link was validated with VPW's much wider
/// TX size range (1..=4128) instead of PWM's (3..=10), silently accepting
/// oversized messages the fixed `SAE_J1850_PWM` resource would reject.
/// `resolve_send_recv_tx` now derives the size-range/header-construction
/// view from `hw_protocol_id` instead.
#[tokio::test]
#[serial]
async fn pwm_detected_link_rejects_message_exceeding_pwm_tx_size_limit() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    server
        .backdoor
        .set_j1850_bus_flavor(Some(j2534_0404::J1850PWM));

    // 0x021C (ISO_15031_5_on_SAE_J1850): 3-byte J1850 header + payload. An
    // 8-byte payload produces an 11-byte message -- within VPW's 1..=4128
    // range, but outside PWM's 3..=10.
    let cll_handle = create_and_connect_cll(&mut client, 0x021C, &[]).await;
    let channel_id = server.backdoor.connect_count() as u32;
    assert_eq!(
        server.backdoor.baud_rate(channel_id),
        41_600,
        "sanity check: this link should have detected PWM"
    );

    let oversized_for_pwm = vec![0u8; 8];
    let status = send_data_expect_rejected(&mut client, cll_handle, oversized_for_pwm).await;
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(
        status.message().contains("3..=10"),
        "expected the PWM TX size range in the error, got: {}",
        status.message()
    );

    server.shutdown().await;
}

/// The same link, but VPW-detected (the default on a silent bus): the
/// identical 8-byte payload (11-byte message) is within VPW's 1..=4128
/// range and must be accepted -- proving the hw-effective size check
/// actually follows the detected flavor both ways, not just rejecting more.
#[tokio::test]
#[serial]
async fn vpw_detected_link_accepts_message_that_would_exceed_pwm_limit() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    // No `set_j1850_bus_flavor` call: silent bus, defaults to VPW.

    let cll_handle = create_and_connect_cll(&mut client, 0x021C, &[]).await;
    let channel_id = server.backdoor.connect_count() as u32;
    assert_eq!(
        server.backdoor.baud_rate(channel_id),
        10_400,
        "sanity check: this link should have defaulted to VPW"
    );

    let payload = vec![0u8; 8];
    send_data(&mut client, cll_handle, payload, vec![]).await;
    assert_eq!(
        server.backdoor.written_count(channel_id),
        1,
        "an 11-byte message is within VPW's 1..=4128 TX size range and should be accepted"
    );

    server.shutdown().await;
}

/// The same Codex review finding also applies to `SetComParam`'s
/// hardware-flavor-dependent allowlist: `CP_NetworkLine` is PWM-only
/// (`is_j1850pwm_param`/`is_j1850vpw_param`, `comparam_support.rs`), gated on
/// whichever native protocol id `check_param_allowed` receives. It must key
/// off `hw_protocol_id`, not the service-level `protocol` (permanently
/// `J1850VPW` for these two resources' `j2534_protocol_id()`), or a
/// PWM-detected link could never set `CP_NetworkLine` and a VPW-detected one
/// would incorrectly be allowed to.
#[tokio::test]
#[serial]
async fn network_line_comparam_allowed_only_when_pwm_is_actually_detected() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    server
        .backdoor
        .set_j1850_bus_flavor(Some(j2534_0404::J1850PWM));

    let cll_handle = create_and_connect_cll(&mut client, 0x021C, &[]).await;
    client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(ParamItem {
                id: Some(vci_service_interface::param_item::Id::ParamId(
                    j2534_0404::NETWORK_LINE,
                )),
                com_param_class: vci_service_interface::PduParamClass::PduPcSpecified as i32,
                param_data: Some(param_item::ParamData::Unum32(1)),
            }),
        })
        .await
        .expect("CP_NetworkLine should be allowed once PWM is actually detected");

    server.shutdown().await;
}

/// The VPW-detected counterpart: `CP_NetworkLine` must still be rejected,
/// exactly as it always was for the fixed `SAE_J1850_VPW` resource.
#[tokio::test]
#[serial]
async fn network_line_comparam_rejected_when_vpw_is_detected() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    // No `set_j1850_bus_flavor` call: silent bus, defaults to VPW.

    let cll_handle = create_and_connect_cll(&mut client, 0x021C, &[]).await;
    let status = client
        .set_com_param(SetComParamRequest {
            cll_handle: Some(cll_handle),
            param_item: Some(ParamItem {
                id: Some(vci_service_interface::param_item::Id::ParamId(
                    j2534_0404::NETWORK_LINE,
                )),
                com_param_class: vci_service_interface::PduParamClass::PduPcSpecified as i32,
                param_data: Some(param_item::ParamData::Unum32(1)),
            }),
        })
        .await
        .expect_err("CP_NetworkLine should still be rejected on a VPW-detected link");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);

    server.shutdown().await;
}

#[tokio::test]
#[serial]
async fn pwm_bus_responds_lands_pwm_and_swaps_working_params() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    server
        .backdoor
        .set_j1850_bus_flavor(Some(j2534_0404::J1850PWM));

    let cll_handle = create_and_connect_cll(&mut client, 0x021C, &[]).await;
    let channel_id = server.backdoor.connect_count() as u32;

    assert_eq!(
        server.backdoor.baud_rate(channel_id),
        41_600,
        "the real connect should use the PWM baud rate once the probe resolves PWM"
    );
    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE).await,
        41_600,
        "the CLL's Working DATA_RATE should be swapped to the PWM preset in place"
    );

    send_data(&mut client, cll_handle, vec![0x01, 0x00], vec![]).await;
    assert_eq!(
        server.backdoor.written_protocol_id(channel_id, 0),
        j2534_0404::J1850PWM,
        "hw_protocol_id (used to build the outgoing message) should carry the PWM result"
    );
    // P1 verification-pass fix: `iso_15031_5_on_sae_j1850_pwm` previously
    // carried the VPW functional-request format byte (0x68) verbatim, so
    // this key was absent from the PWM override diff and a PWM-detected
    // link kept sending VPW-formatted requests a real PWM ECU ignores.
    // `CP_FuncReqFormatPriorityType` is not on the J1850 GetComParam/
    // SetComParam allowlist (`comparam_support::is_j1850pwm_param`/
    // `is_j1850vpw_param`), so the strongest available check is the actual
    // outgoing wire bytes: functional addressing is this resource's
    // default, so byte 0 is the format/priority byte `j1850_header_bytes`
    // builds from that ComParam.
    assert_eq!(
        server.backdoor.written_data(channel_id, 0)[0],
        0x61,
        "the outgoing message's functional-request format byte must be the PWM byte (0x61), \
         not the VPW byte (0x68) that a PWM-ignoring ECU would discard"
    );

    server.shutdown().await;
}

/// The VPW-detected counterpart: confirm the outgoing format byte stays at
/// the VPW value (0x68), as a regression guard alongside the PWM case above.
#[tokio::test]
#[serial]
async fn vpw_bus_responds_connects_with_vpw_header_byte() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    server
        .backdoor
        .set_j1850_bus_flavor(Some(j2534_0404::J1850VPW));

    let cll_handle = create_and_connect_cll(&mut client, 0x021C, &[]).await;
    let channel_id = server.backdoor.connect_count() as u32;

    send_data(&mut client, cll_handle, vec![0x01, 0x00], vec![]).await;
    assert_eq!(
        server.backdoor.written_data(channel_id, 0)[0],
        0x68,
        "a VPW-detected link's outgoing functional-request format byte must stay 0x68"
    );

    server.shutdown().await;
}

/// The two fixed-PWM `SAE_J1850` resources (`0x0215`
/// `ISO_15031_5_on_SAE_J1850_PWM`, `0x0217` `SAE_J2190_on_SAE_J1850_PWM`)
/// never go through the auto-detect probe at all -- their protocol-layer
/// ComParam defaults come straight from `comparam_defaults.rs`'s PWM preset
/// functions, so a connect+send on either must use the PWM format byte
/// (`0x61`) directly, with no probe involved. `0x0215` is the resource the
/// P1 fix's `iso_15031_5_on_sae_j1850_pwm` copy-paste bug affected directly
/// (it carried the VPW byte, `0x68`, verbatim); `0x0217` was already correct
/// and is pinned here as a regression guard.
#[tokio::test]
#[serial]
async fn fixed_pwm_resources_send_the_pwm_header_byte_directly() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let iso_obd_pwm_cll = create_and_connect_cll(&mut client, 0x0215, &[]).await;
    let iso_obd_channel_id = server.backdoor.connect_count() as u32;
    send_data(&mut client, iso_obd_pwm_cll, vec![0x01, 0x00], vec![]).await;
    assert_eq!(
        server.backdoor.written_data(iso_obd_channel_id, 0)[0],
        0x61,
        "resource 0x0215 (ISO_15031_5_on_SAE_J1850_PWM) must send the PWM format byte directly"
    );

    let j2190_pwm_cll = create_and_connect_cll(&mut client, 0x0217, &[]).await;
    let j2190_channel_id = server.backdoor.connect_count() as u32;
    send_data(&mut client, j2190_pwm_cll, vec![0x01, 0x00], vec![]).await;
    assert_eq!(
        server.backdoor.written_data(j2190_channel_id, 0)[0],
        0x61,
        "resource 0x0217 (SAE_J2190_on_SAE_J1850_PWM) must send the PWM format byte directly"
    );

    server.shutdown().await;
}

/// Verification-pass fix: a PWM win must only overwrite the flavor-dependent
/// Working keys (`DATA_RATE` and friends), not every key the full PWM preset
/// happens to carry -- a client's own `SetComParam` staged before `Connect`
/// (`CP_CyclicRespTimeout`, id `0x8010`, identical -- `0` -- in both the VPW
/// and PWM presets) must survive the override, not be silently clobbered
/// back to the preset value right before the Active snapshot. (`CP_P2Max`
/// itself is not usable here: it is not on the J1850 `SetComParam`
/// allowlist at all, native-config-only via the resource's initial
/// defaults -- see `comparam_support::is_j1850vpw_param`/`is_j1850pwm_param`.)
#[tokio::test]
#[serial]
async fn pwm_win_preserves_client_staged_working_param_but_still_swaps_data_rate() {
    const CP_CYCLIC_RESP_TIMEOUT: u32 = 0x8010;

    let server = TestServer::start().await;
    let mut client = server.client().await;
    server
        .backdoor
        .set_j1850_bus_flavor(Some(j2534_0404::J1850PWM));

    let cll_handle = create_cll(&mut client, 0x021C, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CYCLIC_RESP_TIMEOUT, 12_345).await;

    client
        .connect_com_logical_link(vci_service_interface::ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE).await,
        41_600,
        "DATA_RATE is flavor-dependent and auto-managed on this bus -- the detected PWM value \
         should win regardless of client staging"
    );
    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, CP_CYCLIC_RESP_TIMEOUT).await,
        12_345,
        "CP_CyclicRespTimeout is identical (0) in the VPW and PWM presets (flavor-independent) \
         -- the client-staged value must survive the PWM override, not be clobbered back to \
         the preset's 0"
    );

    server.shutdown().await;
}

/// The VPW-detected counterpart: since a VPW win never merges any override
/// into Working at all, a client-staged param is trivially untouched --
/// pinned here as a regression guard alongside the PWM case above.
#[tokio::test]
#[serial]
async fn vpw_win_leaves_client_staged_working_param_untouched() {
    const CP_CYCLIC_RESP_TIMEOUT: u32 = 0x8010;

    let server = TestServer::start().await;
    let mut client = server.client().await;
    // No `set_j1850_bus_flavor` call: silent bus, defaults to VPW.

    let cll_handle = create_cll(&mut client, 0x021C, 1).await;
    set_com_param_unum32(&mut client, cll_handle, CP_CYCLIC_RESP_TIMEOUT, 12_345).await;

    client
        .connect_com_logical_link(vci_service_interface::ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE).await,
        10_400
    );
    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, CP_CYCLIC_RESP_TIMEOUT).await,
        12_345,
        "a VPW win must leave the client-staged CP_CyclicRespTimeout untouched"
    );

    server.shutdown().await;
}

/// Allow-list fix follow-up (`comparam_support::is_j1850pwm_param`/
/// `is_j1850vpw_param` now allow `CP_PhysReqFormatPriorityType`/
/// `CP_PhysReqTargetAddr` via `SetComParam` for J1850, previously rejected):
/// unlike `CP_CyclicRespTimeout` above, `CP_PhysReqFormatPriorityType` is
/// flavor-*dependent* -- `iso_15031_5_on_sae_j1850_pwm`/`_vpw`'s
/// `phys_format` genuinely differs (`0xC4` vs `0x6C`), so it survives in
/// `comparam_defaults::sae_j1850_pwm_override_params`'s diff and a PWM win
/// still overwrites it with the PWM preset's own value, discarding whatever
/// the client staged -- exactly the same "only non-differing keys survive"
/// mechanism `pwm_win_preserves_client_staged_working_param_but_still_swaps_data_rate`
/// pins for `DATA_RATE`, just newly reachable for this param now that
/// `SetComParam` no longer rejects it outright. This is intentional,
/// documented behavior (auto-managed on this bus), not a bug -- pinned here
/// as a regression guard. `CP_PhysReqTargetAddr` is the contrasting
/// flavor-*independent* case (`j1850_common` hardcodes it to `0x10`
/// regardless of flavor): its client-staged override survives the same
/// PWM win untouched, like `CP_CyclicRespTimeout`.
#[tokio::test]
#[serial]
async fn pwm_win_discards_client_staged_flavor_dependent_param_but_keeps_flavor_independent_one() {
    const CP_PHYS_REQ_FORMAT_PRIORITY_TYPE: u32 = 0x8075;
    const CP_PHYS_REQ_TARGET_ADDR: u32 = 0x8076;
    // Distinct from both the VPW (0x6C) and PWM (0xC4) preset values for
    // `iso_15031_5_on_sae_j1850_pwm`/`_vpw`'s `phys_format`.
    const STAGED_PHYS_REQ_FORMAT_PRIORITY_TYPE: u32 = 0x99;
    const STAGED_PHYS_REQ_TARGET_ADDR: u32 = 0x55;

    let server = TestServer::start().await;
    let mut client = server.client().await;
    server
        .backdoor
        .set_j1850_bus_flavor(Some(j2534_0404::J1850PWM));

    let cll_handle = create_cll(&mut client, 0x021C, 1).await;
    set_com_param_unum32(
        &mut client,
        cll_handle,
        CP_PHYS_REQ_FORMAT_PRIORITY_TYPE,
        STAGED_PHYS_REQ_FORMAT_PRIORITY_TYPE,
    )
    .await;
    set_com_param_unum32(
        &mut client,
        cll_handle,
        CP_PHYS_REQ_TARGET_ADDR,
        STAGED_PHYS_REQ_TARGET_ADDR,
    )
    .await;

    client
        .connect_com_logical_link(vci_service_interface::ConnectComLogicalLinkRequest {
            cll_handle: Some(cll_handle),
        })
        .await
        .expect("connect_com_logical_link should succeed");

    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, CP_PHYS_REQ_FORMAT_PRIORITY_TYPE).await,
        0xC4,
        "CP_PhysReqFormatPriorityType is flavor-dependent (differs between the VPW/PWM presets) \
         -- a PWM win must overwrite the client-staged value with the PWM preset's own 0xC4, not \
         preserve the client's staged 0x99. This is intentional auto-managed-on-this-bus \
         behavior, not a bug."
    );
    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, CP_PHYS_REQ_TARGET_ADDR).await,
        STAGED_PHYS_REQ_TARGET_ADDR,
        "CP_PhysReqTargetAddr is flavor-independent (hardcoded 0x10 in both presets) -- the \
         client-staged value must survive the PWM override, for contrast with the \
         flavor-dependent param above"
    );

    server.shutdown().await;
}

#[tokio::test]
#[serial]
async fn silent_bus_defaults_to_vpw_and_still_connects() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    // No `set_j1850_bus_flavor` call: the bus stays silent (the default after
    // `reset()`), so neither probe candidate sees a response.

    let cll_handle = create_and_connect_cll(&mut client, 0x021C, &[]).await;
    let channel_id = server.backdoor.connect_count() as u32;

    assert_eq!(
        server.backdoor.baud_rate(channel_id),
        10_400,
        "an inconclusive probe must default to VPW rather than fail the connect"
    );
    assert_eq!(
        get_com_param_unum32(&mut client, cll_handle, j2534_0404::DATA_RATE).await,
        10_400
    );

    server.shutdown().await;
}

/// P1 fix: real J2534 adapters silently discard every RX frame until at
/// least one filter is installed on the channel -- the normal connect path
/// (`connect_new_physical_channel`) always installs a pass-all filter for
/// exactly this reason (see `install_pass_all_filter`'s doc comment), but
/// the probe's temporary candidate channels previously never did, so a
/// genuine OBD response on real hardware would never reach
/// `PassThruReadMsgs` and the probe would always (wrongly) time out and
/// fall back to VPW. The mock's RX queue does not itself enforce
/// filter-gating (unlike real hardware, and unlike gating it would require
/// invasive changes risking other mock-based tests that inject RX without
/// going through a full connect), so this is asserted via the
/// `PassThruStartMsgFilter` call count instead: on a silent bus neither
/// candidate is conclusive, so the probe alone must install one filter per
/// candidate channel (2 total, each torn down by the following
/// `PassThruDisconnect` before the next channel opens), on top of the 1 the
/// real connect installs afterward for its own channel -- 3 total. Without
/// the fix, only the real connect's single filter is ever installed (1),
/// so this assertion fails without it.
#[tokio::test]
#[serial]
async fn probe_installs_a_pass_all_filter_on_each_candidate_channel() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    // No `set_j1850_bus_flavor` call: silent bus, so both candidates are
    // attempted (neither is conclusive) instead of stopping after the first.

    let filters_before = server.backdoor.start_filter_count();
    let _cll_handle = create_and_connect_cll(&mut client, 0x021C, &[]).await;
    let filters_after = server.backdoor.start_filter_count();

    assert_eq!(
        filters_after - filters_before,
        3,
        "2 probe candidate channels (VPW then PWM, both silent) each need their own pass-all \
         filter installed before reading, plus 1 more for the real connect's own channel"
    );

    server.shutdown().await;
}

#[tokio::test]
#[serial]
async fn j2190_resource_uses_passive_detection_and_still_connects() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    server
        .backdoor
        .set_j1850_bus_flavor(Some(j2534_0404::J1850VPW));

    // 0x021A (SAE_J2190_on_SAE_J1850) has no universal probe request, so the
    // probe only listens -- it must never transmit anything, on the probe
    // channel or the real one.
    let _cll_handle = create_and_connect_cll(&mut client, 0x021A, &[]).await;
    let channel_id = server.backdoor.connect_count() as u32;

    assert_eq!(server.backdoor.baud_rate(channel_id), 10_400);
    for probed_channel_id in 1..=channel_id {
        assert_eq!(
            server.backdoor.written_count(probed_channel_id),
            0,
            "channel {probed_channel_id}: the J2190 probe must never transmit"
        );
    }

    server.shutdown().await;
}

#[tokio::test]
#[serial]
async fn second_cll_on_same_module_skips_reprobe_and_shares_the_channel() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    server
        .backdoor
        .set_j1850_bus_flavor(Some(j2534_0404::J1850VPW));

    let _cll1 = create_and_connect_cll(&mut client, 0x021C, &[]).await;
    let connects_after_first = server.backdoor.connect_count();

    // A second, different bus-agnostic resource on the same module: the
    // cached flavor must be reused (no second probe), and since both CLLs
    // resolve to the same (hw_protocol_id, baud_rate) ChannelKey, they share
    // the already-open physical channel -- no additional PassThruConnect at
    // all.
    let _cll2 = create_and_connect_cll(&mut client, 0x021A, &[]).await;
    let connects_after_second = server.backdoor.connect_count();

    assert_eq!(
        connects_after_second, connects_after_first,
        "the second CLL should neither re-probe nor open a new physical channel"
    );

    server.shutdown().await;
}

/// Verification-pass fix: an inconclusive probe (the passive J2190 resource
/// on a silent bus, which never gets a genuine response either way) must
/// resolve *this* CLL to the VPW fallback but leave the module-wide cache
/// empty -- a later CLL that *can* actively probe (the OBD-capable
/// resource) must still get to run a real probe, not permanently inherit
/// the unconfirmed fallback. A third CLL after the cache is finally
/// populated skips the probe, exactly like the existing dedup test.
#[tokio::test]
#[serial]
async fn inconclusive_fallback_is_not_cached_and_a_later_active_probe_still_runs() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    // No `set_j1850_bus_flavor` call: silent bus, so 0x021A's passive-only
    // probe sees no response either way and falls back to VPW.

    let _cll1 = create_and_connect_cll(&mut client, 0x021A, &[]).await;
    let channel_id_1 = server.backdoor.connect_count() as u32;
    assert_eq!(
        server.backdoor.baud_rate(channel_id_1),
        10_400,
        "the passive probe on a silent bus is inconclusive and must fall back to VPW"
    );
    let connects_after_first = server.backdoor.connect_count();

    // Now make the bus answer PWM. If the fallback above had been cached,
    // this second CLL would skip the probe entirely and also connect at
    // VPW -- it must instead run its own (active) probe and detect PWM.
    server
        .backdoor
        .set_j1850_bus_flavor(Some(j2534_0404::J1850PWM));

    let _cll2 = create_and_connect_cll(&mut client, 0x021C, &[]).await;
    let connects_after_second = server.backdoor.connect_count();
    assert!(
        connects_after_second > connects_after_first,
        "the second CLL must run its own probe (more PassThruConnect calls), not skip it \
         via a wrongly-cached VPW fallback"
    );
    let channel_id_2 = connects_after_second as u32;
    assert_eq!(
        server.backdoor.baud_rate(channel_id_2),
        41_600,
        "the second CLL's own active probe should detect the bus is actually wired for PWM"
    );

    // A third bus-agnostic CLL, now that the cache holds a conclusive PWM
    // result: no further probe connects, and it shares the second CLL's
    // already-open PWM channel (existing dedup assertion style, mirroring
    // `second_cll_on_same_module_skips_reprobe_and_shares_the_channel`).
    let _cll3 = create_and_connect_cll(&mut client, 0x021D, &[]).await;
    let connects_after_third = server.backdoor.connect_count();
    assert_eq!(
        connects_after_third, connects_after_second,
        "once the cache is conclusive, a third CLL should neither re-probe nor open a new \
         physical channel"
    );

    server.shutdown().await;
}

/// Polls for `ResultData` the same way [`wait_for_result_data`] does, but
/// skips any delivery whose split header does not start with `marker` --
/// needed because `set_j1850_bus_flavor` (`PassThruConnect`'s ADR-070
/// simulation) queues a canned response frame on the real connect's own
/// channel too, not just on the probe candidates', the moment the flavor
/// matches (see `j2534-0404-mock/src/lib.rs`'s `PassThruConnect`, the
/// `j1850_bus_flavor` block). Whether that leftover frame is delivered here
/// (if the poll task's next read happens after this test's own
/// `arm_receive_only_monitor` call) or silently discarded as unbound (if
/// before, ADR-100 Decision §5) is itself a race the mock's design does not
/// resolve deterministically -- so this skips zero or one non-matching
/// delivery rather than assuming either outcome, distinguishing the two by
/// each frame's own leading header byte (this test's own injected frame
/// always uses a header distinct from `MOCK_J1850_PWM_RESPONSE`/
/// `MOCK_J1850_VPW_RESPONSE`'s fixed header).
async fn wait_for_result_data_with_header_marker(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    cll_handle: vci_service_interface::ComLogicalLinkHandle,
    marker: u8,
) -> vci_service_interface::ResultData {
    for _ in 0..2 {
        let result = wait_for_result_data(client, cll_handle).await;
        if result
            .extra_info
            .as_ref()
            .is_some_and(|info| info.header_bytes.first() == Some(&marker))
        {
            return result;
        }
    }
    panic!("no ResultData with header[0] == {marker:#x} arrived for the CLL");
}

/// Design-advisor fix (ADR-171 follow-up Decision item): `CllRxEntry::
/// header_protocol` (`events_rx_routing.rs::build_cll_rx_entries`) used to be
/// a fixed `l.protocol.j2534_protocol_id()` read, which for a bus-agnostic
/// `SAE_J1850` resource is permanently `J1850VPW` -- the VPW *initial probe
/// candidate* (ADR-070) -- even after the connect-time auto-detect probe
/// lands on PWM (recorded separately in `l.hw_protocol_id`). Since ADR-171
/// made the RX header/footer split flavor-sensitive (only the PWM arm
/// derives a footer from `ExtraDataIndex`; VPW never does), a PWM-detected
/// bus-agnostic link was silently routed through `header_footer_len`'s VPW
/// arm, which unconditionally reports an empty footer -- so genuine IFR
/// bytes leaked into `data_bytes` (the payload) instead of `footer_bytes`.
///
/// This is the discriminating regression test: it FAILS against the pre-fix
/// `header_protocol` derivation (the IFR bytes `0xAA, 0xBB` land in
/// `data_bytes` alongside the real payload, and `footer_bytes` is empty) and
/// PASSES once `header_protocol` follows the detected `hw_protocol_id`
/// instead (via `resources::base_protocol_id`).
#[tokio::test]
#[serial]
async fn pwm_detected_bus_agnostic_link_splits_genuine_ifr_footer_on_rx() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    server
        .backdoor
        .set_j1850_bus_flavor(Some(j2534_0404::J1850PWM));

    let cll_handle = create_and_connect_cll(&mut client, 0x021C, &[]).await;
    let channel_id = server.backdoor.connect_count() as u32;
    assert_eq!(
        server.backdoor.baud_rate(channel_id),
        41_600,
        "sanity check: this link should have detected PWM"
    );
    arm_receive_only_monitor(&mut client, cll_handle).await;

    let header = vec![0x61, 0x6B, 0x10];
    let payload = vec![0x41, 0x00];
    let ifr = vec![0xAA, 0xBB];
    let mut frame = header.clone();
    frame.extend_from_slice(&payload);
    frame.extend_from_slice(&ifr);
    let extra_data_index = (frame.len() - ifr.len()) as u32;
    server.backdoor.inject_rx_with_edi(
        channel_id,
        &frame,
        j2534_0404::J1850PWM,
        0,
        extra_data_index,
    );

    let result = wait_for_result_data_with_header_marker(&mut client, cll_handle, header[0]).await;
    assert_result_data(&result, &header, &ifr, &payload);

    server.shutdown().await;
}

/// The VPW-detected counterpart to the PWM regression test above: pins that
/// routing correctly follows the DETECTED flavor in both directions, not
/// just "PWM now works" -- a bus-agnostic link that auto-detects VPW must
/// still unconditionally ignore `ExtraDataIndex` (ADR-171's VPW rule), the
/// same as the fixed-`SAE_J1850_VPW` resource does
/// (`j1850vpw_protocol_ignores_in_range_extra_data_index_on_rx` in
/// `rx_header_split.rs`). An in-range-but-wrong `ExtraDataIndex` that would,
/// if honored, wrongly move a real payload byte into `footer_bytes`.
#[tokio::test]
#[serial]
async fn vpw_detected_bus_agnostic_link_ignores_in_range_extra_data_index_on_rx() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    server
        .backdoor
        .set_j1850_bus_flavor(Some(j2534_0404::J1850VPW));

    let cll_handle = create_and_connect_cll(&mut client, 0x021C, &[]).await;
    let channel_id = server.backdoor.connect_count() as u32;
    assert_eq!(
        server.backdoor.baud_rate(channel_id),
        10_400,
        "sanity check: this link should have detected VPW"
    );
    arm_receive_only_monitor(&mut client, cll_handle).await;

    let frame = vec![0x68, 0x10, 0xF1, 0x41, 0x00, 0xBE];
    let in_range_but_wrong_edi = (frame.len() - 1) as u32;
    server.backdoor.inject_rx_with_edi(
        channel_id,
        &frame,
        j2534_0404::J1850VPW,
        0,
        in_range_but_wrong_edi,
    );

    let result = wait_for_result_data_with_header_marker(&mut client, cll_handle, frame[0]).await;
    assert_result_data(&result, &frame[..3], &[], &frame[3..]);

    server.shutdown().await;
}

#[tokio::test]
#[serial]
async fn get_resource_ids_by_renamed_sae_j1850_bus_name() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let sae_j1850_ids = get_resource_ids_by_bustype_name(&mut client, "SAE_J1850").await;
    assert_eq!(sae_j1850_ids, vec![0x021A, 0x021C, 0x021D]);

    // The VPW-only bus no longer includes 0x021A (moved to the renamed bus).
    let vpw_ids = get_resource_ids_by_bustype_name(&mut client, "SAE_J1850_VPW").await;
    assert_eq!(vpw_ids, vec![0x0218, 0x0219, 0x021B]);

    // The old combined name was never released and has no legacy alias: it
    // now hits the same unrecognized-bus_type_name error path as any other
    // unknown name.
    let err = get_resource_ids_expect_err(&mut client, "SAE_J1850_VPW_and_SAE_J1850_PWM").await;
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    server.shutdown().await;
}

/// P2 fix: a legacy bus-type alias/numeric id that resolves to a J2534
/// hardware protocol ID (here `"j1850_vpw"` -> `map_bustype_name` ->
/// `J1850VPW`) must match only the rows whose *fixed* connect protocol is
/// that id (`0x0218`/`0x0219`/`0x021B`, the real VPW-only bus) -- never the
/// `SAE_J1850` auto-detect bus's rows (`0x021A`/`0x021C`/`0x021D`), whose
/// `ChannelProtocol::j2534_protocol_id()` is only the VPW *initial probe
/// candidate* (ADR-070), not a fixed connect protocol at all. `"j1850_vpw"`
/// (no `SAE_` prefix) doesn't match any table `bus_type_name` directly, so
/// this exercises the legacy hw-id fallback specifically -- see
/// `names::J2534Service::legacy_bustype_hw_id`.
#[tokio::test]
#[serial]
async fn legacy_j1850_vpw_bustype_alias_excludes_autodetect_bus_rows() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let legacy_alias_ids = get_resource_ids_by_bustype_name(&mut client, "j1850_vpw").await;
    assert_eq!(
        legacy_alias_ids,
        vec![0x0218, 0x0219, 0x021B],
        "the legacy \"j1850_vpw\" alias must resolve to only the fixed-VPW bus's rows, not \
         the SAE_J1850 auto-detect bus's rows (0x021A/0x021C/0x021D)"
    );

    // The auto-detect bus itself is still reachable by its own name/id --
    // unaffected by the fix, re-asserted here for contrast.
    let sae_j1850_ids = get_resource_ids_by_bustype_name(&mut client, "SAE_J1850").await;
    assert_eq!(sae_j1850_ids, vec![0x021A, 0x021C, 0x021D]);

    server.shutdown().await;
}

async fn get_resource_ids_by_bustype_name(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    name: &str,
) -> Vec<u32> {
    client
        .get_resource_ids(vci_service_interface::GetResourceIdsRequest {
            module_handle: Some(vci_service_interface::ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource_data: Some(vci_service_interface::ResourceData {
                dlc_pin_data: vec![],
                bus_type: Some(vci_service_interface::resource_data::BusType::BusTypeName(
                    name.to_string(),
                )),
                protocol: None,
            }),
        })
        .await
        .expect("get_resource_ids should succeed")
        .into_inner()
        .resource_id_list
        .and_then(|list| list.resource_id_data_array.into_iter().next())
        .map(|data| data.resource_id_array)
        .unwrap_or_default()
}

async fn get_resource_ids_expect_err(
    client: &mut vci_service_interface::vci_service_client::VciServiceClient<
        tonic::transport::Channel,
    >,
    name: &str,
) -> tonic::Status {
    client
        .get_resource_ids(vci_service_interface::GetResourceIdsRequest {
            module_handle: Some(vci_service_interface::ModuleHandle {
                module_handle: MOCK_MODULE_HANDLE,
            }),
            resource_data: Some(vci_service_interface::ResourceData {
                dlc_pin_data: vec![],
                bus_type: Some(vci_service_interface::resource_data::BusType::BusTypeName(
                    name.to_string(),
                )),
                protocol: None,
            }),
        })
        .await
        .expect_err("an unrecognized bus_type_name should be rejected")
}
