# iso22900-mock Implementation Note

## Scope

Native-library test double used by wrapper and service tests, with deterministic behavior and controllable observability.

## Assumptions

- Global mock state is shared for process lifetime and requires deliberate reset discipline.
- Deterministic constants are preferred for reproducible tests.
- Exported ABI must mirror target-specific calling-convention constraints from real bindings.
- The link's state and tester-present (ADR-423) live in `MockState` and are scheduled by the delivery thread that already orders ComPrimitive events (ADR-299): a tester-present is a marker in the same schedule, so it is serialized with ComPrimitives by construction. State changes of STARTCOMM, UPDATEPARAM and STOPCOMM take effect when their last event is delivered, not when they start; `MockState::go_online` strips them from the ComPrimitives still waiting or scheduled, so none outlives the link it was started on.

## Implementation Policy

- Keep default behavior deterministic and explicit.
- Add new mock functionality only to support real test scenarios.
- Maintain function signature parity with sys bindings.
- Preserve counters and event queue visibility for diagnostics.

## Change Checklist

1. Re-run integration tests that depend on the mock after exported API edits.
2. Validate construct/destruct and queue cleanup behavior remains repeatable.
3. Confirm ABI and symbol names still match expected loader contracts.
4. Update state reset expectations if parallel test strategy changes.
