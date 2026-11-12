use super::*;

#[test]
fn every_shipped_profile_is_valid() {
    for profile in ALL {
        profile
            .validate()
            .unwrap_or_else(|e| panic!("{}: {e}", profile.layer));
        // One profile per layer: a second of the same name would be unreachable.
        let found = get(profile.layer).expect("found by its layer");
        assert!(std::ptr::eq(found, *profile), "{} twice", profile.layer);
    }
    assert!(get("no such layer").is_none());
}

#[test]
fn a_profile_of_the_three_calls_is_valid_and_looked_up_by_intent() {
    assert_eq!(UDS_PROFILE.validate(), Ok(()));
    let call = UDS_PROFILE.call(Intent::Routine).expect("defined");
    assert_eq!(call.steps[0].request.sid, 0x31);
    let only_dtcs = Profile {
        calls: &[READ_DTC],
        ..UDS_PROFILE
    };
    assert!(only_dtcs.call(Intent::SecurityAccess).is_none());
}

#[test]
fn the_negative_shape_and_pending_codes_are_those_the_workers_support() {
    let other_marker = Profile {
        negative: NegativeShape {
            marker: 0x7E,
            ..NEGATIVE_7F
        },
        ..UDS_PROFILE
    };
    assert_eq!(other_marker.validate(), Err(ProfileError::NegativeShape));
    let no_sid = Profile {
        negative: NegativeShape {
            echoes_sid: false,
            code_at: 1,
            ..NEGATIVE_7F
        },
        ..UDS_PROFILE
    };
    assert_eq!(no_sid.validate(), Err(ProfileError::NegativeShape));
    let pending_22 = Profile {
        pending: &[0x78, 0x22],
        ..UDS_PROFILE
    };
    assert_eq!(pending_22.validate(), Err(ProfileError::PendingCode(0x22)));
    let twice = Profile {
        pending: &[0x21, 0x21],
        ..UDS_PROFILE
    };
    assert_eq!(twice.validate(), Err(ProfileError::PendingTwice(0x21)));
    let all = Profile {
        pending: &[0x78, 0x21, 0x23],
        ..UDS_PROFILE
    };
    assert_eq!(all.validate(), Ok(()));
}

#[test]
fn malformed_calls_are_refused() {
    fn with(call: Call) -> Result<(), ProfileError> {
        Profile {
            calls: Box::leak(Box::new([call])),
            ..UDS_PROFILE
        }
        .validate()
    }
    let step = |kind, sid, parts, declared, result| Step {
        kind,
        request: Request { sid, parts },
        declared,
        result,
    };
    let twice = Profile {
        calls: &[READ_DTC, READ_DTC],
        ..UDS_PROFILE
    };
    assert_eq!(
        twice.validate(),
        Err(ProfileError::IntentTwice(Intent::ReadDtc))
    );
    assert_eq!(
        with(Call {
            intent: Intent::ReadDtc,
            steps: &[]
        }),
        Err(ProfileError::NoStep(Intent::ReadDtc))
    );
    // A constant wider than its bits, and one not whole bytes.
    for parts in [
        &[
            Part::Const {
                bits: 8,
                value: 0x100,
            },
            Part::Arg {
                bits: 8,
                arg: Source::Mask,
            },
        ][..],
        &[
            Part::Const { bits: 4, value: 1 },
            Part::Arg {
                bits: 8,
                arg: Source::Mask,
            },
        ][..],
    ] {
        let parts: &'static [Part] = Box::leak(parts.to_vec().into_boxed_slice());
        let steps = Box::leak(Box::new([step(
            StepKind::SendReceive,
            0x19,
            parts,
            true,
            ResultMap::None,
        )]));
        assert_eq!(
            with(Call {
                intent: Intent::ReadDtc,
                steps
            }),
            Err(ProfileError::BadConstant(Intent::ReadDtc))
        );
    }
    // `params` before another part.
    assert_eq!(
        with(Call {
            intent: Intent::Routine,
            steps: &[Step {
                kind: StepKind::SendReceive,
                request: Request {
                    sid: 0x31,
                    parts: &[
                        Part::Select {
                            bits: 8,
                            arg: Source::Sub
                        },
                        Part::Params,
                        Part::Select {
                            bits: 16,
                            arg: Source::Rid
                        },
                    ],
                },
                declared: true,
                result: ResultMap::Declared,
            }],
        }),
        Err(ProfileError::ParamsNotLast(Intent::Routine))
    );
    // An argument of another call.
    assert_eq!(
        with(Call {
            intent: Intent::ReadDtc,
            steps: &[Step {
                kind: StepKind::SendReceive,
                request: Request {
                    sid: 0x19,
                    parts: &[Part::Arg {
                        bits: 8,
                        arg: Source::Level
                    }],
                },
                declared: true,
                result: ResultMap::None,
            }],
        }),
        Err(ProfileError::ForeignSource(Intent::ReadDtc, Source::Level))
    );
    // A required argument left out.
    assert_eq!(
        with(Call {
            intent: Intent::ReadDtc,
            steps: &[Step {
                kind: StepKind::SendReceive,
                request: Request {
                    sid: 0x19,
                    parts: &[Part::Const {
                        bits: 8,
                        value: 0x02
                    }],
                },
                declared: true,
                result: ResultMap::None,
            }],
        }),
        Err(ProfileError::SourceCount(Intent::ReadDtc, Source::Mask, 0))
    );
    // The key sent before the step that makes it.
    assert_eq!(
        with(Call {
            intent: Intent::SecurityAccess,
            steps: &[Step {
                kind: StepKind::SendReceive,
                request: Request {
                    sid: 0x27,
                    parts: &[Part::Arg {
                        bits: 0,
                        arg: Source::Key
                    }],
                },
                declared: false,
                result: ResultMap::None,
            }],
        }),
        Err(ProfileError::KeyBeforeKeyStep(Intent::SecurityAccess))
    );
    // `RoutineControl` without the sub-function and routine identifier it sends itself.
    assert_eq!(
        with(Call {
            intent: Intent::Routine,
            steps: &[Step {
                kind: StepKind::RoutineControl,
                request: Request {
                    sid: 0x31,
                    parts: &[
                        Part::Select {
                            bits: 16,
                            arg: Source::Rid
                        },
                        Part::Select {
                            bits: 8,
                            arg: Source::Sub
                        },
                        Part::Params,
                    ],
                },
                declared: true,
                result: ResultMap::Declared,
            }],
        }),
        Err(ProfileError::StepKind(Intent::Routine))
    );
    // A DTC mapping on a routine, and declared names on an undeclared response.
    assert_eq!(
        with(Call {
            intent: Intent::Routine,
            steps: &[Step {
                kind: StepKind::RoutineControl,
                request: ROUTINE_REQUEST,
                declared: true,
                result: ResultMap::ReadDtc {
                    mask: Pos(0),
                    dtcs: Pos(1),
                    code_bits: 24,
                    status_bits: 8
                },
            }],
        }),
        Err(ProfileError::ResultMap(Intent::Routine))
    );
    assert_eq!(
        with(Call {
            intent: Intent::Routine,
            steps: &[Step {
                kind: StepKind::RoutineControl,
                request: ROUTINE_REQUEST,
                declared: false,
                result: ResultMap::Declared,
            }],
        }),
        Err(ProfileError::ResultMap(Intent::Routine))
    );
}

fn leak<T>(items: Vec<T>) -> &'static [T] {
    Box::leak(items.into_boxed_slice())
}

/// `intent`'s call made of `steps`, validated in a profile of its own.
fn check(intent: Intent, steps: Vec<Step>) -> Result<(), ProfileError> {
    check_in(UDS, intent, steps)
}

/// As [`check`], in the profile of application layer `layer`.
fn check_in(layer: &'static str, intent: Intent, steps: Vec<Step>) -> Result<(), ProfileError> {
    Profile {
        layer,
        calls: leak(vec![Call {
            intent,
            steps: leak(steps),
        }]),
        ..UDS_PROFILE
    }
    .validate()
}

fn step(kind: StepKind, sid: u8, parts: Vec<Part>, declared: bool, result: ResultMap) -> Step {
    Step {
        kind,
        request: Request {
            sid,
            parts: leak(parts),
        },
        declared,
        result,
    }
}

const DTC_MAP: ResultMap = ResultMap::ReadDtc {
    mask: Pos(0),
    dtcs: Pos(1),
    code_bits: 24,
    status_bits: 8,
};

fn mask(bits: u8) -> Part {
    Part::Arg {
        bits,
        arg: Source::Mask,
    }
}

fn read_dtc(parts: Vec<Part>, result: ResultMap) -> Result<(), ProfileError> {
    check(
        Intent::ReadDtc,
        vec![step(StepKind::SendReceive, 0x19, parts, true, result)],
    )
}

fn security(steps: Vec<Step>) -> Result<(), ProfileError> {
    check(Intent::SecurityAccess, steps)
}

fn seed_step() -> Step {
    seed_step_with(ZeroSeed::Unlocked)
}

fn seed_step_with(zero: ZeroSeed) -> Step {
    step(
        StepKind::SendReceive,
        0x27,
        vec![Part::Arg {
            bits: 8,
            arg: Source::Level,
        }],
        false,
        ResultMap::Seed {
            after_bytes: 2,
            zero,
        },
    )
}

fn key_step(sid: u8) -> Step {
    step(StepKind::SecurityKey, sid, vec![], false, ResultMap::None)
}

fn send_key_step() -> Step {
    step(
        StepKind::SendReceive,
        0x27,
        vec![
            Part::Arg {
                bits: 8,
                arg: Source::LevelPlusOne,
            },
            Part::Arg {
                bits: 0,
                arg: Source::Key,
            },
        ],
        false,
        ResultMap::None,
    )
}

#[test]
fn parts_are_whole_bytes_of_at_most_64_bits_and_fit_their_source() {
    let i = Intent::ReadDtc;
    let sub = Part::Const {
        bits: 8,
        value: 0x02,
    };
    // A 64-bit constant is the widest.
    let wide = Part::Const {
        bits: 64,
        value: u64::MAX,
    };
    assert_eq!(read_dtc(vec![wide, mask(8)], DTC_MAP), Ok(()));
    let wider = Part::Const { bits: 72, value: 0 };
    assert_eq!(
        read_dtc(vec![wider, mask(8)], DTC_MAP),
        Err(ProfileError::BadConstant(i))
    );
    assert_eq!(
        read_dtc(vec![sub, mask(12)], DTC_MAP),
        Err(ProfileError::BadWidth(i))
    );
    // An argument that runs to the end of the request is last.
    assert_eq!(
        read_dtc(vec![mask(0), sub], DTC_MAP),
        Err(ProfileError::RestNotLast(i))
    );
    // Only the routine's identifier and sub-function select.
    let select_mask = Part::Select {
        bits: 8,
        arg: Source::Mask,
    };
    assert_eq!(
        read_dtc(vec![sub, select_mask], DTC_MAP),
        Err(ProfileError::PartForSource(i, Source::Mask))
    );
    assert_eq!(
        check(
            Intent::Routine,
            vec![step(
                StepKind::SendReceive,
                0x31,
                vec![
                    Part::Arg {
                        bits: 8,
                        arg: Source::Sub
                    },
                    Part::Select {
                        bits: 16,
                        arg: Source::Rid
                    },
                    Part::Params,
                ],
                true,
                ResultMap::Declared,
            )],
        ),
        Err(ProfileError::PartForSource(Intent::Routine, Source::Sub))
    );
    // Each argument once, and a routine's `params` once.
    assert_eq!(
        read_dtc(vec![mask(8), mask(8)], DTC_MAP),
        Err(ProfileError::SourceCount(i, Source::Mask, 2))
    );
    assert_eq!(
        check(
            Intent::Routine,
            vec![step(
                StepKind::RoutineControl,
                0x31,
                ROUTINE_REQUEST.parts[..2].to_vec(),
                true,
                ResultMap::Declared,
            )],
        ),
        Err(ProfileError::ParamsCount(Intent::Routine, 0))
    );
}

#[test]
fn each_call_has_the_results_adr_314_gives_it() {
    let sub = Part::Const {
        bits: 8,
        value: 0x02,
    };
    // `diag.read_dtc` maps `mask` and `dtcs`, to different fields; without the map the mask has
    // no request of its own either.
    assert_eq!(
        read_dtc(vec![sub, mask(8)], ResultMap::Declared),
        Err(ProfileError::ResultMap(Intent::ReadDtc))
    );
    assert_eq!(
        read_dtc(vec![sub, mask(8)], ResultMap::None),
        Err(ProfileError::SourceOutOfPlace(
            Intent::ReadDtc,
            Source::Mask
        ))
    );
    let same = ResultMap::ReadDtc {
        mask: Pos(1),
        dtcs: Pos(1),
        code_bits: 24,
        status_bits: 8,
    };
    assert_eq!(
        read_dtc(vec![sub, mask(8)], same),
        Err(ProfileError::ResultMap(Intent::ReadDtc))
    );
    let undeclared = check(
        Intent::ReadDtc,
        vec![step(
            StepKind::SendReceive,
            0x19,
            vec![sub, mask(8)],
            false,
            DTC_MAP,
        )],
    );
    assert_eq!(undeclared, Err(ProfileError::ResultMap(Intent::ReadDtc)));
    // A routine's last step gives the declared names.
    assert_eq!(
        check(
            Intent::Routine,
            vec![step(
                StepKind::RoutineControl,
                0x31,
                ROUTINE_REQUEST.parts.to_vec(),
                true,
                ResultMap::None,
            )],
        ),
        Err(ProfileError::ResultMap(Intent::Routine))
    );
    // Security access gives no declared names, and its seed is undeclared.
    let mut declared_seed = seed_step();
    declared_seed.declared = true;
    assert_eq!(
        security(vec![declared_seed, key_step(0), send_key_step()]),
        Err(ProfileError::ResultMap(Intent::SecurityAccess))
    );
    let mut declared_key = send_key_step();
    declared_key.declared = true;
    declared_key.result = ResultMap::Declared;
    assert_eq!(
        security(vec![seed_step(), key_step(0), declared_key]),
        Err(ProfileError::ResultMap(Intent::SecurityAccess))
    );
}

#[test]
fn on_uds_a_zero_seed_means_unlocked_and_another_layer_may_say_otherwise() {
    let with = |zero| vec![seed_step_with(zero), key_step(0), send_key_step()];
    assert_eq!(security(with(ZeroSeed::Unlocked)), Ok(()));
    assert_eq!(
        security(with(ZeroSeed::Key)),
        Err(ProfileError::ZeroSeed(Intent::SecurityAccess))
    );
    assert_eq!(
        check_in("other", Intent::SecurityAccess, with(ZeroSeed::Key)),
        Ok(())
    );
}

#[test]
fn the_key_step_follows_the_seed_and_sends_nothing() {
    let i = Intent::SecurityAccess;
    assert_eq!(
        security(vec![seed_step(), key_step(0), send_key_step()]),
        Ok(())
    );
    assert_eq!(
        security(vec![key_step(0), seed_step(), send_key_step()]),
        Err(ProfileError::StepKind(i))
    );
    assert_eq!(
        security(vec![seed_step(), key_step(0x27), send_key_step()]),
        Err(ProfileError::StepKind(i))
    );
    assert_eq!(
        security(vec![seed_step(), key_step(0), key_step(0), send_key_step()]),
        Err(ProfileError::StepKind(i))
    );
    // A seed with no key step after it.
    assert_eq!(
        security(vec![seed_step(), send_key_step()]),
        Err(ProfileError::ResultMap(i))
    );
    // The key step and `RoutineControl` belong to their own calls.
    assert_eq!(
        check(Intent::ReadDtc, vec![key_step(0)]),
        Err(ProfileError::StepKind(Intent::ReadDtc))
    );
    assert_eq!(
        check(
            Intent::ReadDtc,
            vec![step(
                StepKind::RoutineControl,
                0x19,
                ROUTINE_REQUEST.parts.to_vec(),
                true,
                DTC_MAP,
            )],
        ),
        Err(ProfileError::StepKind(Intent::ReadDtc))
    );
}

#[test]
fn the_levels_go_with_the_seed_and_the_key_requests() {
    let i = Intent::SecurityAccess;
    // `level + 1` on the seed request and `level` on the key request.
    let mut seed = seed_step();
    seed.request.parts = leak(vec![Part::Arg {
        bits: 8,
        arg: Source::LevelPlusOne,
    }]);
    let mut send_key = send_key_step();
    send_key.request.parts = leak(vec![
        Part::Arg {
            bits: 8,
            arg: Source::Level,
        },
        Part::Arg {
            bits: 0,
            arg: Source::Key,
        },
    ]);
    assert_eq!(
        security(vec![seed, key_step(0), send_key]),
        Err(ProfileError::SourceOutOfPlace(i, Source::LevelPlusOne))
    );
}

#[test]
fn the_mask_goes_in_the_request_whose_response_gives_the_dtcs() {
    let mask_first = step(
        StepKind::SendReceive,
        0x19,
        vec![mask(8)],
        true,
        ResultMap::None,
    );
    let dtcs = step(
        StepKind::SendReceive,
        0x19,
        vec![Part::Const {
            bits: 8,
            value: 0x02,
        }],
        true,
        DTC_MAP,
    );
    assert_eq!(
        check(Intent::ReadDtc, vec![mask_first, dtcs]),
        Err(ProfileError::SourceOutOfPlace(
            Intent::ReadDtc,
            Source::Mask
        ))
    );
}

#[test]
fn the_level_and_the_key_go_in_one_request_that_ends_the_call() {
    let i = Intent::SecurityAccess;
    let level_plus_one = step(
        StepKind::SendReceive,
        0x27,
        vec![Part::Arg {
            bits: 8,
            arg: Source::LevelPlusOne,
        }],
        false,
        ResultMap::None,
    );
    let key_alone = step(
        StepKind::SendReceive,
        0x27,
        vec![Part::Arg {
            bits: 0,
            arg: Source::Key,
        }],
        false,
        ResultMap::None,
    );
    assert_eq!(
        security(vec![seed_step(), key_step(0), level_plus_one, key_alone]),
        Err(ProfileError::SourceOutOfPlace(i, Source::Key))
    );
    // A request after the send-key request.
    let after = step(StepKind::SendReceive, 0x3E, vec![], false, ResultMap::None);
    assert_eq!(
        security(vec![seed_step(), key_step(0), send_key_step(), after]),
        Err(ProfileError::ResultMap(i))
    );
}

#[test]
fn routine_control_serves_only_uds_routine_control() {
    let routine = |sid| Call {
        intent: Intent::Routine,
        steps: leak(vec![step(
            StepKind::RoutineControl,
            sid,
            ROUTINE_REQUEST.parts.to_vec(),
            true,
            ResultMap::Declared,
        )]),
    };
    let profile = |layer, sid| Profile {
        layer,
        calls: leak(vec![routine(sid)]),
        ..UDS_PROFILE
    };
    assert_eq!(profile(UDS, 0x31).validate(), Ok(()));
    assert_eq!(
        profile(UDS, 0x32).validate(),
        Err(ProfileError::StepKind(Intent::Routine))
    );
    assert_eq!(
        profile("KWP2000", 0x31).validate(),
        Err(ProfileError::StepKind(Intent::Routine))
    );
    // On UDS a routine keeps `RoutineControl`.
    let send_receive = Call {
        intent: Intent::Routine,
        steps: leak(vec![step(
            StepKind::SendReceive,
            0x31,
            ROUTINE_REQUEST.parts.to_vec(),
            true,
            ResultMap::Declared,
        )]),
    };
    let uds = Profile {
        calls: leak(vec![send_receive]),
        ..UDS_PROFILE
    };
    assert_eq!(uds.validate(), Err(ProfileError::StepKind(Intent::Routine)));
    let other = Profile {
        layer: "KWP2000",
        ..uds
    };
    assert_eq!(other.validate(), Ok(()));
}
