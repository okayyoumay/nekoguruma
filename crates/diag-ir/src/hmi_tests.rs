use super::*;

fn template(id: &str, params: Vec<(&str, HmiParam)>) -> HmiTemplate {
    HmiTemplate {
        id: id.to_owned(),
        params: params
            .into_iter()
            .map(|(key, param)| (key.to_owned(), param))
            .collect(),
    }
}

#[test]
fn forms_and_templates_round_trip() {
    let t = template(
        "confirm_ignition",
        vec![
            ("count", HmiParam::Const(HmiValue::Int(-3))),
            ("ecu", HmiParam::Runtime),
            ("_ok2", HmiParam::Const(HmiValue::Bool(true))),
            ("label", HmiParam::Const(HmiValue::Text("ä".to_owned()))),
        ],
    );
    let bytes = encode_template(&t).unwrap();
    assert_eq!(decode_template(&bytes).unwrap(), t);
    let form = t.fill([HmiValue::Text("engine".to_owned())]).unwrap();
    assert_eq!(
        form.params[1],
        ("ecu".to_owned(), HmiValue::Text("engine".to_owned()))
    );
    assert_eq!(decode_form(&encode_form(&form).unwrap()).unwrap(), form);
    // A form is not a template, and neither takes trailing bytes.
    assert!(decode_form(&bytes).is_err());
    let mut longer = encode_form(&form).unwrap();
    longer.push(0);
    assert_eq!(decode_form(&longer), Err(HmiError::Malformed));
}

#[test]
fn fill_takes_exactly_one_value_per_slot_in_order() {
    let t = template(
        "f",
        vec![
            ("a", HmiParam::Runtime),
            ("b", HmiParam::Const(HmiValue::Int(2))),
            ("c", HmiParam::Runtime),
        ],
    );
    assert_eq!(t.runtime_slots(), 2);
    let form = t.fill([HmiValue::Int(1), HmiValue::Int(3)]).unwrap();
    let values: Vec<_> = form.params.iter().map(|(_, v)| v.clone()).collect();
    assert_eq!(
        values,
        [HmiValue::Int(1), HmiValue::Int(2), HmiValue::Int(3)]
    );
    assert_eq!(t.fill([HmiValue::Int(1)]), None);
    assert_eq!(t.fill(vec![HmiValue::Int(1); 3]), None);
}

#[test]
fn the_bounds_of_item_1_are_enforced_both_ways() {
    let int = || HmiParam::Const(HmiValue::Int(0));
    let refused = |t: HmiTemplate, error: HmiError| {
        assert_eq!(encode_template(&t), Err(error.clone()), "{t:?}");
        // The same template written without the checks is refused when read.
        let bytes = postcard::to_allocvec(&t).unwrap();
        assert_eq!(decode_template(&bytes), Err(error), "{t:?}");
    };
    refused(template("", vec![]), HmiError::BadId);
    refused(
        template(&"x".repeat(MAX_HMI_ID_LEN + 1), vec![]),
        HmiError::BadId,
    );
    encode_template(&template(&"x".repeat(MAX_HMI_ID_LEN), vec![])).unwrap();
    for key in ["", "1a", "a-b", "é", "a b"] {
        refused(
            template("f", vec![(key, int())]),
            HmiError::BadKey(key.to_owned()),
        );
    }
    let long_key = "k".repeat(MAX_HMI_KEY_LEN + 1);
    refused(
        template("f", vec![(&long_key, int())]),
        HmiError::BadKey(long_key.clone()),
    );
    encode_template(&template("f", vec![(&long_key[1..], int())])).unwrap();
    refused(
        template("f", vec![("a", int()), ("a", int())]),
        HmiError::DuplicateKey("a".to_owned()),
    );
    let keys: Vec<String> = (0..=MAX_HMI_PARAMS).map(|n| format!("p{n}")).collect();
    let many = |n: usize| template("f", keys[..n].iter().map(|k| (k.as_str(), int())).collect());
    encode_template(&many(MAX_HMI_PARAMS)).unwrap();
    refused(many(MAX_HMI_PARAMS + 1), HmiError::TooManyParams);
    // The whole encoding is bounded, read and written.
    let big = |len: usize| {
        template(
            "f",
            vec![("t", HmiParam::Const(HmiValue::Text("x".repeat(len))))],
        )
    };
    // The text's length prefix takes one byte more from 128 bytes on.
    let fits = MAX_HMI_FORM_LEN - postcard::to_allocvec(&big(0)).unwrap().len() - 1;
    assert_eq!(encode_template(&big(fits)).unwrap().len(), MAX_HMI_FORM_LEN);
    assert_eq!(
        encode_template(&big(fits + 1)),
        Err(HmiError::TooLong(MAX_HMI_FORM_LEN + 1))
    );
    let bytes = postcard::to_allocvec(&big(fits + 1)).unwrap();
    assert_eq!(
        decode_template(&bytes),
        Err(HmiError::TooLong(MAX_HMI_FORM_LEN + 1))
    );
    assert_eq!(decode_template(&[0xFF]), Err(HmiError::Malformed));
}

#[test]
fn answers_are_a_status_byte_and_text() {
    assert_eq!(answered("12.5"), b"\x0112.5");
    assert_eq!(answered(""), [ANSWERED]);
    assert_eq!(declined(), [DECLINED]);
    assert_eq!(timed_out(), [TIMED_OUT]);
}
