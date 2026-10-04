use std::borrow::Borrow;

use tonic::{Request, Response, Status};

use iso22900::{E_PDU_OBJT, ObjectType as IsoObjectType};
use vci_service_interface::{
    EcuUniqueRespData, GetObjectIdRequest, GetUniqueRespIdTableRequest, IoCtlRequest,
    IoCtlResponse, ObjectIdResponse, ObjectType, Response as VciServiceResponse,
    SetUniqueRespIdTableRequest, UniqueRespIdTableItem, UniqueRespIdTableResponse, io_ctl_request,
};

use crate::error::{api_not_initialized_status, map_runtime_error};
use crate::service::convert::{borrowed_param_to_item, from_unique_response_item};
use crate::service::handles::{parse_ioctl_handle, require_cll_message};
use crate::service::ioctl::{ioctl_input_to_owned, ioctl_output_to_data_item};
use crate::service::rpc::API_NOT_INITIALIZED;
use crate::service::rpc::Iso22900Service;

impl Iso22900Service {
    pub(super) async fn rpc_io_ctl(
        &self,
        request: Request<IoCtlRequest>,
    ) -> Result<Response<IoCtlResponse>, Status> {
        let request = request.into_inner();
        let (h_mod, h_cll) = parse_ioctl_handle(request.handle)?;
        let io_ctrl_command_id = match request.io_ctrl_command {
            Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandId(id)) => id,
            Some(io_ctl_request::IoCtrlCommand::IoCtrlCommandName(name)) => {
                self.with_api(|api| {
                    api.get_object_id(IsoObjectType(E_PDU_OBJT::PDU_OBJT_IO_CTRL), name.as_str())
                })
                .await?
                .0
            }
            None => {
                return Err(Status::invalid_argument(
                    "io_ctrl_command is required (io_ctrl_command_id or io_ctrl_command_name)",
                ));
            }
        };
        let output_data = {
            let api = self.api.lock().await;
            let owned_input = request.input_data.map(ioctl_input_to_owned).transpose()?;
            if request.has_output {
                let output = api
                    .as_ref()
                    .ok_or(api_not_initialized_status(API_NOT_INITIALIZED))?
                    .io_ctl_with_output(
                        h_mod,
                        h_cll,
                        io_ctrl_command_id,
                        owned_input.as_ref().map(Borrow::borrow),
                    )
                    .map(|output| ioctl_output_to_data_item(output.borrowed()))
                    .map_err(map_runtime_error)?;
                Some(output?)
            } else {
                api.as_ref()
                    .ok_or(api_not_initialized_status(API_NOT_INITIALIZED))?
                    .io_ctl_without_output(
                        h_mod,
                        h_cll,
                        io_ctrl_command_id,
                        owned_input.as_ref().map(Borrow::borrow),
                    )
                    .map_err(map_runtime_error)?;
                None
            }
        };

        Ok(Response::new(IoCtlResponse { output_data }))
    }

    pub(super) async fn rpc_get_object_id(
        &self,
        request: Request<GetObjectIdRequest>,
    ) -> Result<Response<ObjectIdResponse>, Status> {
        let request = request.into_inner();
        let object_type = match ObjectType::try_from(request.object_type)
            .map_err(|_| Status::invalid_argument("object_type is invalid"))?
        {
            ObjectType::ObjtProtocol => E_PDU_OBJT::PDU_OBJT_PROTOCOL,
            ObjectType::ObjtBustype => E_PDU_OBJT::PDU_OBJT_BUSTYPE,
            ObjectType::ObjtIoCtrl => E_PDU_OBJT::PDU_OBJT_IO_CTRL,
            ObjectType::ObjtComparam => E_PDU_OBJT::PDU_OBJT_COMPARAM,
            ObjectType::ObjtPintype => E_PDU_OBJT::PDU_OBJT_PINTYPE,
            ObjectType::ObjtResource => E_PDU_OBJT::PDU_OBJT_RESOURCE,
            ObjectType::ObjtUnspecified => {
                return Err(Status::invalid_argument(
                    "object_type must be a concrete object type",
                ));
            }
        };
        let object_id = self
            .with_api(|api| api.get_object_id(IsoObjectType(object_type), &request.shortname))
            .await?;

        Ok(Response::new(ObjectIdResponse {
            pdu_object_id: object_id.0,
        }))
    }

    pub(super) async fn rpc_get_unique_resp_id_table(
        &self,
        request: Request<GetUniqueRespIdTableRequest>,
    ) -> Result<Response<UniqueRespIdTableResponse>, Status> {
        let request = request.into_inner();
        let (h_mod, h_cll) = require_cll_message(request.cll_handle, "cll_handle")?;
        let api = self.api.lock().await;
        let entries = api
            .as_ref()
            .ok_or(api_not_initialized_status(API_NOT_INITIALIZED))?
            .get_unique_resp_id_table(h_mod, h_cll)
            .map_err(map_runtime_error)?;

        // ADR-218 Decision item 5: shares `resolve_vendor_struct_entry_size`
        // with `GetComParam` (`rpc_link.rs::rpc_get_com_param`) so the
        // read-side resolution algorithm cannot drift between the two.
        let mut resolve_vendor_entry_size =
            |struct_type| self.resolve_vendor_struct_entry_size(struct_type);
        let unique_data = entries
            .borrowed()
            .entries()
            .map_err(map_runtime_error)?
            .iter()
            .map(|entry| {
                let params = entry
                    .params()
                    .map_err(map_runtime_error)?
                    .iter()
                    .map(|item| borrowed_param_to_item(item, &mut resolve_vendor_entry_size))
                    .collect::<Result<Vec<_>, _>>()?;

                Ok(EcuUniqueRespData {
                    unique_resp_identifier: entry.unique_resp_identifier(),
                    params,
                })
            })
            .collect::<Result<Vec<_>, Status>>()?;

        Ok(Response::new(UniqueRespIdTableResponse {
            unique_resp_id_table: Some(UniqueRespIdTableItem { unique_data }),
        }))
    }

    /// Replaces the table wholesale with `request`'s entries -- a bare
    /// pass-through to `PDUSetUniqueRespIdTable`, matching
    /// `j2534-0404-service`'s own full-replace behavior for this RPC and
    /// preserving the caller's ability to shrink or clear the table (an
    /// earlier read-merge-write design was reverted, per Codex review on
    /// PR #106: with the previous CLL-creation-time defaults always merged
    /// back in, there was no way to ever delete a `unique_resp_identifier`
    /// entry through this, the only table-mutation RPC). See
    /// `docs/rpc-api-guide.md`'s "Unique Response ID Table" section for the
    /// resulting caller-side responsibility: some D-PDU implementations
    /// drop entries this request doesn't mention rather than preserving
    /// them, so a caller doing a partial update should `GetUniqueRespIdTable`
    /// first and send back the complete desired table, not just the delta.
    pub(super) async fn rpc_set_unique_resp_id_table(
        &self,
        request: Request<SetUniqueRespIdTableRequest>,
    ) -> Result<Response<VciServiceResponse>, Status> {
        let request = request.into_inner();
        let (h_mod, h_cll) = require_cll_message(request.cll_handle, "cll_handle")?;
        let table = request
            .unique_resp_id_table
            .ok_or_else(|| Status::invalid_argument("unique_resp_id_table is required"))?;

        let api = self.api.lock().await;
        let api = api
            .as_ref()
            .ok_or_else(|| api_not_initialized_status(API_NOT_INITIALIZED))?;

        // ADR-218 Decision item 4 (as amended): shares
        // `resolve_vendor_struct_entry_size` with `GetUniqueRespIdTable`
        // (this same file) and `SetComParam` (`rpc_link.rs`) -- a non-empty
        // vendor STRUCTFIELD entry's declared `size_of_entry` is validated
        // against per-library operator config; there is no process-lifetime
        // write cache to populate anymore.
        let mut resolve_vendor_entry_size =
            |struct_type| self.resolve_vendor_struct_entry_size(struct_type);
        let entries = from_unique_response_item(
            table,
            |name| {
                api.get_object_id(IsoObjectType(E_PDU_OBJT::PDU_OBJT_COMPARAM), name)
                    .map_err(map_runtime_error)
            },
            &mut resolve_vendor_entry_size,
        )?;

        api.set_unique_resp_id_table(h_mod, h_cll, entries)
            .map_err(map_runtime_error)?;

        Ok(Self::empty_response())
    }
}
