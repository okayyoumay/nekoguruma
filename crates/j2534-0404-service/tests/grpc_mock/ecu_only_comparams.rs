//! `CP_P2Max_Ecu` / `CP_P2Star_Ecu` are ECU-specific ComParams: ISO 22900-2
//! (2022) B.3.4 says the D-PDU API does not interpret them and returns an
//! error code when one is passed, B.3.5 defines usage "E" as ECU software
//! generation or configuration only, and Table B.10 marks both "S,E" (2009
//! edition: B.3.4, B.3.5, Tables B.10/B.19). `SetComParam` and `GetComParam`
//! refuse them with `PDU_ERR_COMPARAM_NOT_SUPPORTED`, by id and by name, on
//! every protocol (ADR-056).

use serial_test::serial;
use tonic::Code;
use vci_service_interface::{
    GetComParamRequest, ParamItem, PduError, PduParamClass, SetComParamRequest,
    error_detail_from_status, get_com_param_request, param_item, param_item::Id,
};

use crate::harness::*;

const CP_P2_STAR_ECU: u32 = 0x8012;
const CP_P2_MAX_ECU: u32 = 0x8013;

fn assert_not_supported(status: tonic::Status, what: &str) {
    assert_eq!(status.code(), Code::InvalidArgument, "{what}");
    let detail = error_detail_from_status(&status)
        .unwrap_or_else(|| panic!("{what} rejection should carry an ErrorDetail"));
    assert_eq!(
        detail.pdu_error,
        PduError::PduErrComparamNotSupported as i32,
        "{what} should report PDU_ERR_COMPARAM_NOT_SUPPORTED"
    );
}

#[tokio::test]
#[serial]
async fn ecu_only_p2_comparams_are_refused_by_id_and_name() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    for protocol in [j2534_0404::ISO15765, j2534_0404::ISO14230] {
        let cll_handle = create_cll(&mut client, protocol, 1).await;
        for (id, name) in [
            (CP_P2_STAR_ECU, "CP_P2Star_Ecu"),
            (CP_P2_MAX_ECU, "CP_P2Max_Ecu"),
        ] {
            for item_id in [Id::ParamId(id), Id::ParamName(name.to_string())] {
                let what = format!("SetComParam {name} on protocol {protocol:#x} via {item_id:?}");
                let status = client
                    .set_com_param(SetComParamRequest {
                        cll_handle: Some(cll_handle),
                        param_item: Some(ParamItem {
                            id: Some(item_id),
                            com_param_class: PduParamClass::PduPcCom as i32,
                            param_data: Some(param_item::ParamData::Unum32(1_000)),
                        }),
                    })
                    .await
                    .expect_err(&what);
                assert_not_supported(status, &what);
            }

            let what = format!("GetComParam {name} on protocol {protocol:#x}");
            let status = client
                .get_com_param(GetComParamRequest {
                    cll_handle: Some(cll_handle),
                    param: Some(get_com_param_request::Param::ParamId(id)),
                })
                .await
                .expect_err(&what);
            assert_not_supported(status, &what);
        }
    }

    server.shutdown().await;
}
