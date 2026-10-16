use super::*;

/// The UDS calls as ADR-314 sketches them.
const READ_DTC: Call = Call {
    intent: Intent::ReadDtc,
    steps: &[Step {
        kind: StepKind::SendReceive,
        request: Request {
            sid: 0x19,
            parts: &[
                Part::Const {
                    bits: 8,
                    value: 0x02,
                },
                Part::Arg {
                    bits: 8,
                    arg: Source::Mask,
                },
            ],
        },
        declared: true,
        result: ResultMap::ReadDtc {
            mask: Pos(0),
            dtcs: Pos(1),
            code_bits: 24,
            status_bits: 8,
        },
    }],
};

const ROUTINE_REQUEST: Request = Request {
    sid: 0x31,
    parts: &[
        Part::Select {
            bits: 8,
            arg: Source::Sub,
        },
        Part::Select {
            bits: 16,
            arg: Source::Rid,
        },
        Part::Params,
    ],
};

const ROUTINE: Call = Call {
    intent: Intent::Routine,
    steps: &[Step {
        kind: StepKind::RoutineControl,
        request: ROUTINE_REQUEST,
        declared: true,
        result: ResultMap::Declared,
    }],
};

const SECURITY_ACCESS: Call = Call {
    intent: Intent::SecurityAccess,
    steps: &[
        Step {
            kind: StepKind::SendReceive,
            request: Request {
                sid: 0x27,
                parts: &[Part::Arg {
                    bits: 8,
                    arg: Source::Level,
                }],
            },
            declared: false,
            result: ResultMap::Seed { after_bytes: 2 },
        },
        Step {
            kind: StepKind::SecurityKey,
            request: Request { sid: 0, parts: &[] },
            declared: false,
            result: ResultMap::None,
        },
        Step {
            kind: StepKind::SendReceive,
            request: Request {
                sid: 0x27,
                parts: &[
                    Part::Arg {
                        bits: 8,
                        arg: Source::LevelPlusOne,
                    },
                    Part::Arg {
                        bits: 0,
                        arg: Source::Key,
                    },
                ],
            },
            declared: false,
            result: ResultMap::None,
        },
    ],
};

const UDS_LIKE: Profile = Profile {
    layer: "UDS",
    calls: &[READ_DTC, ROUTINE, SECURITY_ACCESS],
    negative: NEGATIVE_7F,
    pending: &[0x78],
};

#[test]
fn every_shipped_profile_is_valid() {
    for profile in ALL {
        profile
            .validate()
            .unwrap_or_else(|e| panic!("{}: {e}", profile.layer));
        assert_eq!(get(profile.layer).map(|p| p.layer), Some(profile.layer));
    }
    assert!(get("no such layer").is_none());
}

#[test]
fn a_profile_of_the_three_calls_is_valid_and_looked_up_by_intent() {
    assert_eq!(UDS_LIKE.validate(), Ok(()));
    let call = UDS_LIKE.call(Intent::Routine).expect("defined");
    assert_eq!(call.steps[0].request.sid, 0x31);
    let only_dtcs = Profile {
        calls: &[READ_DTC],
        ..UDS_LIKE
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
        ..UDS_LIKE
    };
    assert_eq!(other_marker.validate(), Err(ProfileError::NegativeShape));
    let no_sid = Profile {
        negative: NegativeShape {
            echoes_sid: false,
            code_at: 1,
            ..NEGATIVE_7F
        },
        ..UDS_LIKE
    };
    assert_eq!(no_sid.validate(), Err(ProfileError::NegativeShape));
    let pending_22 = Profile {
        pending: &[0x78, 0x22],
        ..UDS_LIKE
    };
    assert_eq!(pending_22.validate(), Err(ProfileError::PendingCode(0x22)));
    let twice = Profile {
        pending: &[0x21, 0x21],
        ..UDS_LIKE
    };
    assert_eq!(twice.validate(), Err(ProfileError::PendingTwice(0x21)));
    let all = Profile {
        pending: &[0x78, 0x21, 0x23],
        ..UDS_LIKE
    };
    assert_eq!(all.validate(), Ok(()));
}

#[test]
fn malformed_calls_are_refused() {
    fn with(call: Call) -> Result<(), ProfileError> {
        Profile {
            calls: Box::leak(Box::new([call])),
            ..UDS_LIKE
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
        ..UDS_LIKE
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
