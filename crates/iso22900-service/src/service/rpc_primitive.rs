use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tonic::{Request, Response, Status};

use iso22900::{ComPrimitiveControl, ComPrimitiveType, E_PDU_COPT, FlagData};
use vci_service_interface::{
    CancelComPrimitiveRequest, ComOperationType, ComPrimitiveResponse, EventItemResponse,
    EventNotification, GetEventItemRequest, GetStatusRequest, Response as VciServiceResponse,
    StartComPrimitiveRequest, StatusResponse, SubscribeEventRequest,
    status_response::Status as VciServiceStatusResponse,
};

use crate::error::{api_not_initialized_status, map_runtime_error, map_runtime_error_for_link};
use crate::service::convert::{to_iso_control, to_status_response};
use crate::service::events::to_proto_event_item;
use crate::service::handles::{
    parse_event_item_handle, parse_status_handle, parse_subscribe_event_handle,
    require_cll_message, require_cop_message, to_proto_cop_handle,
};
use crate::service::rpc::{API_NOT_INITIALIZED, EventStream, Iso22900Service};

/// Maximum size, in bytes, of a `StartComPrimitiveRequest.cop_tag` (ADR-204).
/// Generous enough for a UUID (16 bytes) or a compact string key; enforced
/// before the tag ever reaches the native `StartComPrimitive` call, since the
/// tag is echoed on every event emitted for the resulting COP -- an
/// unbounded tag would be a per-event amplification hazard.
pub(super) const MAX_COP_TAG_LEN: usize = 64;

impl Iso22900Service {
    pub(super) async fn rpc_start_com_primitive(
        &self,
        request: Request<StartComPrimitiveRequest>,
    ) -> Result<Response<ComPrimitiveResponse>, Status> {
        let request = request.into_inner();
        let (h_mod, h_cll) = require_cll_message(request.cll_handle, "cll_handle")?;
        let control = request
            .cop_ctrl_data
            .map(to_iso_control)
            .unwrap_or_else(|| ComPrimitiveControl {
                time: 0,
                send_cycles: 0,
                receive_cycles: 0,
                temp_param_update: 0,
                tx_flags: FlagData::default(),
                expected_responses: Vec::new(),
            });

        let cop_type = match ComOperationType::try_from(request.cop_type)
            .map_err(|_| Status::invalid_argument("cop_type is invalid"))?
        {
            ComOperationType::CoptStartcomm => E_PDU_COPT::PDU_COPT_STARTCOMM,
            ComOperationType::CoptStopcomm => E_PDU_COPT::PDU_COPT_STOPCOMM,
            ComOperationType::CoptUpdateparam => E_PDU_COPT::PDU_COPT_UPDATEPARAM,
            ComOperationType::CoptSendrecv => E_PDU_COPT::PDU_COPT_SENDRECV,
            ComOperationType::CoptDelay => E_PDU_COPT::PDU_COPT_DELAY,
            ComOperationType::CoptRestoreParam => E_PDU_COPT::PDU_COPT_RESTORE_PARAM,
            ComOperationType::CoptUnspecified => {
                return Err(Status::invalid_argument(
                    "cop_type must be a concrete operation type",
                ));
            }
        };

        // Enforce the tag size cap before the native call so an oversized
        // tag never reaches it (ADR-204).
        if let Some(tag) = request.cop_tag.as_ref()
            && tag.len() > MAX_COP_TAG_LEN
        {
            return Err(Status::invalid_argument(format!(
                "cop_tag exceeds the maximum size of {MAX_COP_TAG_LEN} bytes"
            )));
        }

        let cop_data = request.cop_data;
        // `self.api` is acquired manually here (rather than via
        // `with_api_for_link`) and held across both the native call and the
        // `cop_tags` reconcile below -- not released in between -- so the
        // insert/remove happens-before any consumer of this freshly-minted
        // COP's events can observe it. Both other readers of a COP's tag
        // (`rpc_get_event_item` above and `run_event_subscription_task` in
        // `events.rs`) need `self.api` first and only lock `cop_tags` after,
        // so neither can reach `cop_tags` for this `h_cop` until this block
        // has released `self.api`, by which point the reconcile below has
        // already completed (ADR-204 Consequences).
        //
        // `cop_tags` is acquired here, *before* the native call, not after
        // it returns -- deliberately, per lock order `api` outer / `cop_tags`
        // inner (unchanged, still matching `rpc_get_event_item`/
        // `run_event_subscription_task`). `start_com_primitive` and the
        // reconcile below are both plain synchronous code, so once both
        // guards are held there is no further `.await` between the native
        // call's side effect and the reconcile: a cancelled/dropped RPC
        // future can only be interrupted at a suspension point, and none
        // exists in that window. Acquiring `cop_tags` only after the native
        // call returned (an earlier revision of this code did) left exactly
        // one such point -- a client disconnecting while that second
        // `.await` was still pending on a contended `cop_tags` (e.g. a
        // concurrent `terminate_cop_tags_for_link`/`_for_module` sweep) would
        // drop this future before the reconcile ever ran, leaving a
        // freshly-minted COP with no tag reconciled at all -- silently
        // dropping a supplied tag, or worse, leaving a reused handle's stale
        // prior occupant unevicted (Codex review, PR #116).
        let h_cop = {
            let api_guard = self.api.lock().await;
            let api = api_guard
                .as_ref()
                .ok_or_else(|| api_not_initialized_status(API_NOT_INITIALIZED))?;
            let mut cop_tags = self.cop_tags.lock().await;
            // The native pCoPTag stays null -- this is a deliberate
            // ADR-204 decision, not an oversight. The tag's only
            // authoritative home is `self.cop_tags`, echoed on
            // EventItem.cop_tag from there on every backend uniformly.
            let h_cop = api
                .start_com_primitive(
                    h_mod,
                    h_cll,
                    ComPrimitiveType(cop_type),
                    cop_data,
                    control,
                    0,
                )
                .map_err(|error| map_runtime_error_for_link(api, h_mod, h_cll, error))?;

            // Reconcile unconditionally, not just insert: this same
            // `(h_mod, h_cll, h_cop)` key may be a reused handle from a prior
            // COP whose stale tag was never evicted (e.g. its terminal event
            // was never fetched). Without the `None` -> `remove` branch, a
            // new tag-less COP reusing that key would incorrectly echo the
            // old occupant's tag on every event it emits.
            let key = (h_mod.0, h_cll.0, h_cop.0);
            match request.cop_tag {
                Some(tag) => {
                    cop_tags.insert(key, tag);
                }
                None => {
                    cop_tags.remove(&key);
                }
            }

            h_cop
        };

        Ok(Response::new(ComPrimitiveResponse {
            cop_handle: Some(to_proto_cop_handle(h_mod, h_cll, h_cop)),
        }))
    }

    pub(super) async fn rpc_cancel_com_primitive(
        &self,
        request: Request<CancelComPrimitiveRequest>,
    ) -> Result<Response<VciServiceResponse>, Status> {
        let request = request.into_inner();
        let (h_mod, h_cll, h_cop) = require_cop_message(request.cop_handle, "cop_handle")?;
        self.with_api_for_link(h_mod, h_cll, |api| {
            api.cancel_com_primitive(h_mod, h_cll, h_cop)
        })
        .await?;
        Ok(Self::empty_response())
    }

    pub(super) async fn rpc_get_status(
        &self,
        request: Request<GetStatusRequest>,
    ) -> Result<Response<StatusResponse>, Status> {
        let request = request.into_inner();
        let (h_mod, h_cll, h_cop, status_kind) = parse_status_handle(request.handle)?;

        let status = self
            .with_api(|api| api.get_status(h_mod, h_cll, h_cop))
            .await?;

        let mapped_status = match status_kind {
            VciServiceStatusResponse::ModuleStatus(_) => {
                VciServiceStatusResponse::ModuleStatus(status.status.0 as i32)
            }
            VciServiceStatusResponse::CllStatus(_) => {
                VciServiceStatusResponse::CllStatus(status.status.0 as i32)
            }
            VciServiceStatusResponse::CopStatus(_) => {
                VciServiceStatusResponse::CopStatus(status.status.0 as i32)
            }
        };

        Ok(Response::new(to_status_response(status, mapped_status)))
    }

    pub(super) async fn rpc_get_event_item(
        &self,
        request: Request<GetEventItemRequest>,
    ) -> Result<Response<EventItemResponse>, Status> {
        let request = request.into_inner();
        let (h_mod, h_cll) = parse_event_item_handle(request.handle)?;
        // Hold the lock while the ApiItem borrows from the DPduApi.
        let api = self.api.lock().await;
        let api = api
            .as_ref()
            .ok_or_else(|| api_not_initialized_status(API_NOT_INITIALIZED))?;
        // Acquired before `get_event_item` so the non-`Send` `ApiItem` it
        // returns is never alive across an `.await` point (a
        // `cop_tags.lock().await` placed after the borrow would otherwise
        // make this handler's future itself non-`Send`).
        let mut cop_tags = self.cop_tags.lock().await;
        let item = api
            .get_event_item(h_mod, h_cll)
            .map_err(map_runtime_error)?;
        let event_item = to_proto_event_item(api, item.borrowed(), h_mod, h_cll, &mut cop_tags)?;

        Ok(Response::new(EventItemResponse {
            event_item: Some(event_item),
        }))
    }

    pub(super) async fn rpc_subscribe_event(
        &self,
        request: Request<SubscribeEventRequest>,
    ) -> Result<Response<EventStream>, Status> {
        let request = request.into_inner();
        let (h_mod, h_cll) = parse_subscribe_event_handle(request.handle)?;
        let key = (h_mod.0, h_cll.0);

        let (stream_tx, stream_rx) = mpsc::unbounded_channel::<Result<EventNotification, Status>>();
        let api_for_finalizer = Arc::clone(&self.api);

        let subscriptions_for_finalizer = self.subscriptions.clone();

        let stream_tx_in_finalizer = stream_tx.clone();
        let event_notifications = self.event_notifications.clone();
        if let Some(current_stream_tx) = self.subscriptions.lock().await.insert(key, stream_tx) {
            let _ = current_stream_tx.send(Err(Status::cancelled(
                "Another subscription exists for the same key",
            )));
        }

        let mut shutdown_for_finalizer = self.shutdown.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = shutdown_for_finalizer.changed() => {}
                _ = stream_tx_in_finalizer.closed() => {}
            };
            let should_unregister = {
                let subscriptions = subscriptions_for_finalizer.lock().await;
                match subscriptions.get(&key) {
                    Some(stream_tx) => stream_tx.same_channel(&stream_tx_in_finalizer),
                    None => true,
                }
            };

            if should_unregister {
                let api_guard = api_for_finalizer.lock().await;
                if let Some(api) = api_guard.as_ref() {
                    let _ = api.unregister_event_callback(h_mod, h_cll);
                }
            }
        });

        {
            let api_guard = self.api.lock().await;
            let api = api_guard
                .as_ref()
                .ok_or_else(|| api_not_initialized_status(API_NOT_INITIALIZED))?;
            api.register_event_callback(h_mod, h_cll, move |notification| {
                let _ = event_notifications.send(notification);
            })
            .map_err(map_runtime_error)?;
        }

        Ok(Response::new(Box::pin(UnboundedReceiverStream::new(
            stream_rx,
        ))))
    }
}
