//! The decision core of the guarded VCI handle (design 13.5 "Guarded VCI access", ADR-275 item 4,
//! ADR-333).
//!
//! [`decide`] is one pure function: it takes a worker request ([`Request`], one variant per RPC
//! of `service VciService`), the profile, the links the harness has bound and the ids the harness
//! has learned from the worker's answers ([`Context`]), and says whether the request may be sent
//! ([`Grant`]) or why not ([`GuardRefusal`]). It sends nothing and keeps no state: the state the
//! decision rests on (which links exist, which id means which name) is passed in, and the caller
//! changes it only from a [`Grant`] or from a worker response it has seen. Whatever the function
//! cannot classify is refused.
//!
//! - [`link`]: link setup, the resource of a link and its ComParams, compared with the profile
//! - [`uds`]: StartComPrimitive, the UDS request a payload or a raw CAN frame carries, and the
//!   per-class lists
//! - [`ioctl`]: the IOCTLs that pass and the hardware controls that never do here
//!
//! The seam that routes the agent's job runner and a direct check through [`decide`] is not part
//! of this module.

use std::collections::{BTreeMap, BTreeSet};

use vci_service_interface::{
    CancelComPrimitiveRequest, ConnectComLogicalLinkRequest, DisconnectComLogicalLinkRequest,
};
use vci_service_interface::{
    ComLogicalLinkHandle, ComPrimitiveHandle, CreateComLogicalLinkRequest,
    DestroyComLogicalLinkRequest, GetComParamRequest, GetConflictingResourcesRequest,
    GetEventItemRequest, GetModuleIdsRequest, GetObjectIdRequest, GetResourceIdsRequest,
    GetResourceStatusRequest, GetStatusRequest, GetTimestampRequest, GetUniqueRespIdTableRequest,
    GetVersionRequest, IoCtlRequest, LockResourceRequest, ModuleConnectRequest,
    ModuleDisconnectRequest, ModuleHandle, ObjectType, ResourceData, SetComParamRequest,
    SetUniqueRespIdTableRequest, StartComPrimitiveRequest, SubscribeEventRequest,
    UnlockResourceRequest, get_event_item_request, get_status_request, io_ctl_request,
    subscribe_event_request,
};

use crate::profile::{Class, Link, Profile, Setup, Vci};

mod ioctl;
mod link;
mod uds;

/// A worker request, one variant per RPC of `service VciService` in `service.proto`. [`decide`]
/// matches it without a catch-all arm, so an RPC added to the service does not compile until it
/// is classified (the `every_rpc_has_a_variant` test also compares the names with the proto).
#[derive(Debug, Clone, Copy)]
pub enum Request<'a> {
    GetModuleIds(&'a GetModuleIdsRequest),
    ModuleConnect(&'a ModuleConnectRequest),
    ModuleDisconnect(&'a ModuleDisconnectRequest),
    GetVersion(&'a GetVersionRequest),
    GetTimestamp(&'a GetTimestampRequest),
    GetResourceStatus(&'a GetResourceStatusRequest),
    GetResourceIds(&'a GetResourceIdsRequest),
    GetConflictingResources(&'a GetConflictingResourcesRequest),
    CreateComLogicalLink(&'a CreateComLogicalLinkRequest),
    DestroyComLogicalLink(&'a DestroyComLogicalLinkRequest),
    ConnectComLogicalLink(&'a ConnectComLogicalLinkRequest),
    DisconnectComLogicalLink(&'a DisconnectComLogicalLinkRequest),
    LockResource(&'a LockResourceRequest),
    UnlockResource(&'a UnlockResourceRequest),
    GetComParam(&'a GetComParamRequest),
    SetComParam(&'a SetComParamRequest),
    StartComPrimitive(&'a StartComPrimitiveRequest),
    CancelComPrimitive(&'a CancelComPrimitiveRequest),
    GetStatus(&'a GetStatusRequest),
    GetEventItem(&'a GetEventItemRequest),
    SubscribeEvent(&'a SubscribeEventRequest),
    IoCtl(&'a IoCtlRequest),
    GetObjectId(&'a GetObjectIdRequest),
    GetUniqueRespIdTable(&'a GetUniqueRespIdTableRequest),
    SetUniqueRespIdTable(&'a SetUniqueRespIdTableRequest),
}

/// The RPC names of `service VciService`, in the order of [`Request`]'s variants.
pub const RPC_NAMES: [&str; 25] = [
    "GetModuleIds",
    "ModuleConnect",
    "ModuleDisconnect",
    "GetVersion",
    "GetTimestamp",
    "GetResourceStatus",
    "GetResourceIds",
    "GetConflictingResources",
    "CreateComLogicalLink",
    "DestroyComLogicalLink",
    "ConnectComLogicalLink",
    "DisconnectComLogicalLink",
    "LockResource",
    "UnlockResource",
    "GetComParam",
    "SetComParam",
    "StartComPrimitive",
    "CancelComPrimitive",
    "GetStatus",
    "GetEventItem",
    "SubscribeEvent",
    "IoCtl",
    "GetObjectId",
    "GetUniqueRespIdTable",
    "SetUniqueRespIdTable",
];

impl Request<'_> {
    /// The RPC's name.
    pub fn rpc(&self) -> &'static str {
        let index = match self {
            Request::GetModuleIds(_) => 0,
            Request::ModuleConnect(_) => 1,
            Request::ModuleDisconnect(_) => 2,
            Request::GetVersion(_) => 3,
            Request::GetTimestamp(_) => 4,
            Request::GetResourceStatus(_) => 5,
            Request::GetResourceIds(_) => 6,
            Request::GetConflictingResources(_) => 7,
            Request::CreateComLogicalLink(_) => 8,
            Request::DestroyComLogicalLink(_) => 9,
            Request::ConnectComLogicalLink(_) => 10,
            Request::DisconnectComLogicalLink(_) => 11,
            Request::LockResource(_) => 12,
            Request::UnlockResource(_) => 13,
            Request::GetComParam(_) => 14,
            Request::SetComParam(_) => 15,
            Request::StartComPrimitive(_) => 16,
            Request::CancelComPrimitive(_) => 17,
            Request::GetStatus(_) => 18,
            Request::GetEventItem(_) => 19,
            Request::SubscribeEvent(_) => 20,
            Request::IoCtl(_) => 21,
            Request::GetObjectId(_) => 22,
            Request::GetUniqueRespIdTable(_) => 23,
            Request::SetUniqueRespIdTable(_) => 24,
        };
        RPC_NAMES[index]
    }
}

/// A link the harness has created: which profile endpoint it serves and whether it was created
/// in RawMode (ISO 22900-2 (2022) D.2.3), which decides how a payload is read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundLink {
    /// Index into the profile's endpoints: 0 for a one-link profile; 0 or 1 for a T0 loopback.
    pub endpoint: usize,
    pub raw_mode: bool,
    /// The canonical names of the profile-pinned ComParams that have been set on this link to
    /// the profile's value (see [`Grant::SetsComParams`]). `ConnectComLogicalLink` is refused
    /// until every required one is here.
    pub set: BTreeSet<String>,
}

impl BoundLink {
    /// A link on `endpoint` with no profile ComParam set yet.
    pub fn new(endpoint: usize, raw_mode: bool) -> Self {
        BoundLink {
            endpoint,
            raw_mode,
            set: BTreeSet::new(),
        }
    }
}

/// The links of the check by their `cll_handle`.
pub type LinkTable = BTreeMap<u32, BoundLink>;

/// What the harness has learned from the worker's answers to `GetResourceIds` and
/// `GetObjectId`, so that a request that names something by id can be compared by what it means.
/// The guard asks the worker nothing: an id not in here is refused.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Learned {
    /// Resource id to the resource it was looked up for.
    pub resources: BTreeMap<u32, ResourceData>,
    /// Object type and id to the short name it was looked up for (ComParams, IO controls, pin
    /// types, protocols and bus types).
    pub objects: BTreeMap<(ObjectType, u32), String>,
}

/// What [`decide`] looks at besides the request.
#[derive(Debug, Clone, Copy)]
pub struct Context<'a> {
    pub profile: &'a Profile,
    /// The handle of the one module the check opened on this worker.
    pub module: u32,
    /// The profile's VCI this worker fronts. A link is created only for an endpoint on this VCI,
    /// so on a T0 loopback of two VCIs, each worker's context binds its own end only, whatever
    /// the order the links are created in. Two channels of one VCI are both this VCI's.
    pub vci: &'a Vci,
    pub links: &'a LinkTable,
    pub learned: &'a Learned,
}

/// A request the guard lets through, and what the caller must do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Grant {
    /// Send it; nothing changes.
    Pass,
    /// A link is created on this profile endpoint; bind the handle of the response to it.
    CreatesLink { endpoint: usize, raw_mode: bool },
    /// Send it; once the worker has accepted it, add these canonical ComParam names to the
    /// `set` of the link `cll`. Each was checked against the profile's value.
    SetsComParams { cll: u32, names: Vec<String> },
    /// The link with this `cll_handle` goes away; drop it from the table.
    DestroysLink(u32),
    /// The request moves the ECU to this diagnostic session (the sub-function with the
    /// suppress-response bit cleared); register the return to the default session as a teardown
    /// step.
    EntersSession(u8),
}

/// Why [`decide`] refuses a request. Distinct from [`crate::Refusal`], which is the profile
/// refusing a check before it starts.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GuardRefusal {
    #[error("link setup differs from the profile's: {0}")]
    LinkSetup(String),
    #[error("the profile describes no further link")]
    LinkCount,
    #[error("the request names a link or module the check does not hold")]
    UnknownHandle,
    #[error("the {kind} id {id:#x} was not learned from the worker, so its meaning is unknown")]
    UnknownId { kind: &'static str, id: u32 },
    #[error("{class:?} does not allow UDS service {sid:#04x} (sub-function {sub:02x?}) here")]
    Request {
        class: Class,
        sid: u8,
        sub: Option<u8>,
    },
    #[error("frame refused: {0}")]
    Frame(&'static str),
    #[error("hardware control refused: {0}")]
    HardwareControl(String),
    #[error("the request is for another module than the check's")]
    Module,
    #[error("the request cannot be classified: {0}")]
    Undecodable(&'static str),
}

/// Whether `request` may be sent. See the module documentation.
pub fn decide(ctx: &Context<'_>, request: &Request<'_>) -> Result<Grant, GuardRefusal> {
    match request {
        // Reads of what the worker lists and resolves, which change nothing.
        Request::GetModuleIds(_)
        | Request::GetObjectId(_)
        | Request::GetConflictingResources(_) => Ok(Grant::Pass),
        // The device is opened by the harness before the guard exists, and another module is
        // another device.
        Request::ModuleConnect(_) => Err(GuardRefusal::LinkSetup(
            "the harness opens the device; a check does not connect a module".to_owned(),
        )),
        // Without a handle it would disconnect whatever module the worker holds, the harness's
        // own included; the harness closes its device itself.
        Request::ModuleDisconnect(r) => match &r.module_handle {
            None => Err(GuardRefusal::Module),
            Some(handle) => ctx.module_of(Some(handle)).map(|()| Grant::Pass),
        },
        Request::GetVersion(r) => ctx
            .module_of(r.module_handle.as_ref())
            .map(|()| Grant::Pass),
        Request::GetTimestamp(r) => ctx
            .module_of(r.module_handle.as_ref())
            .map(|()| Grant::Pass),
        Request::GetResourceIds(r) => ctx
            .module_of(r.module_handle.as_ref())
            .map(|()| Grant::Pass),
        Request::GetResourceStatus(r) => {
            for resource in &r.resources {
                ctx.module_of(resource.module_handle.as_ref())?;
            }
            Ok(Grant::Pass)
        }
        Request::CreateComLogicalLink(r) => {
            ctx.module_of(r.module_handle.as_ref())?;
            link::create(ctx, r)
        }
        Request::DestroyComLogicalLink(r) => {
            ctx.bound(r.cll_handle.as_ref())?;
            Ok(Grant::DestroysLink(
                r.cll_handle.map_or(0, |h| h.cll_handle),
            ))
        }
        Request::ConnectComLogicalLink(r) => {
            // A link connects with the profile's ComParams, not the worker's defaults.
            let (bound, link) = ctx.bound(r.cll_handle.as_ref())?;
            let missing: Vec<String> = link::required(link, ctx.profile.class)
                .into_iter()
                .filter(|name| !bound.set.contains(name))
                .collect();
            if missing.is_empty() {
                Ok(Grant::Pass)
            } else {
                Err(GuardRefusal::LinkSetup(format!(
                    "the link is not connected before the profile's ComParams are set: {}",
                    missing.join(", ")
                )))
            }
        }
        Request::DisconnectComLogicalLink(r) => ctx.known_link(r.cll_handle.as_ref()),
        Request::LockResource(r) => ctx.known_link(r.cll_handle.as_ref()),
        Request::UnlockResource(r) => ctx.known_link(r.cll_handle.as_ref()),
        Request::GetComParam(r) => ctx.known_link(r.cll_handle.as_ref()),
        Request::GetUniqueRespIdTable(r) => ctx.known_link(r.cll_handle.as_ref()),
        Request::SetComParam(r) => {
            let (_, link) = ctx.bound(r.cll_handle.as_ref())?;
            let item = r
                .param_item
                .as_ref()
                .ok_or(GuardRefusal::Undecodable("SetComParam without a ComParam"))?;
            let names = link::check_param(ctx, link, item)?.into_iter().collect();
            Ok(sets(r.cll_handle, names))
        }
        Request::SetUniqueRespIdTable(r) => {
            let (_, link) = ctx.bound(r.cll_handle.as_ref())?;
            let table = r
                .unique_resp_id_table
                .as_ref()
                .ok_or(GuardRefusal::Undecodable(
                    "SetUniqueRespIdTable without a table",
                ))?;
            // A profile's link addresses one ECU, and the J2534 worker takes the physical
            // addressing from the first entry and pairs a response with its request only within
            // one entry, so the table is exactly one entry holding the whole pairing.
            let [entry] = table.unique_data.as_slice() else {
                return Err(GuardRefusal::LinkSetup(format!(
                    "the response table must have exactly one entry, the profile's ECU, not {}",
                    table.unique_data.len()
                )));
            };
            let mut names = Vec::new();
            for item in &entry.params {
                names.extend(link::check_param(ctx, link, item)?);
            }
            names.sort();
            names.dedup();
            // Both workers replace the table with this one, so it must hold every identifier
            // ComParam a table can hold (the UNIQUE_ID class), each at the profile's value:
            // an empty or partial table would leave the link without what a set recorded.
            let missing: Vec<String> = link::table_required(link)
                .into_iter()
                .filter(|name| !names.contains(name))
                .collect();
            if !missing.is_empty() {
                return Err(GuardRefusal::LinkSetup(format!(
                    "the response table replaces the old one and must carry the profile's {}",
                    missing.join(", ")
                )));
            }
            Ok(sets(r.cll_handle, names))
        }
        Request::StartComPrimitive(r) => {
            let (bound, link) = ctx.bound(r.cll_handle.as_ref())?;
            uds::start_com_primitive(ctx, bound, link, r)
        }
        Request::CancelComPrimitive(r) => {
            ctx.cop_known(r.cop_handle.as_ref())?;
            Ok(Grant::Pass)
        }
        Request::GetStatus(r) => match &r.handle {
            Some(get_status_request::Handle::ModuleHandle(m)) => {
                ctx.module_of(Some(m)).map(|()| Grant::Pass)
            }
            Some(get_status_request::Handle::CllHandle(c)) => ctx.known_link(Some(c)),
            Some(get_status_request::Handle::CopHandle(c)) => {
                ctx.cop_known(Some(c)).map(|()| Grant::Pass)
            }
            None => Err(GuardRefusal::Undecodable("GetStatus without a handle")),
        },
        Request::GetEventItem(r) => match &r.handle {
            Some(get_event_item_request::Handle::SystemHandle(_)) => Ok(Grant::Pass),
            Some(get_event_item_request::Handle::ModuleHandle(m)) => {
                ctx.module_of(Some(m)).map(|()| Grant::Pass)
            }
            Some(get_event_item_request::Handle::CllHandle(c)) => ctx.known_link(Some(c)),
            None => Err(GuardRefusal::Undecodable("GetEventItem without a handle")),
        },
        Request::SubscribeEvent(r) => match &r.handle {
            Some(subscribe_event_request::Handle::SystemHandle(_)) => Ok(Grant::Pass),
            Some(subscribe_event_request::Handle::ModuleHandle(m)) => {
                ctx.module_of(Some(m)).map(|()| Grant::Pass)
            }
            Some(subscribe_event_request::Handle::CllHandle(c)) => ctx.known_link(Some(c)),
            None => Err(GuardRefusal::Undecodable("SubscribeEvent without a handle")),
        },
        Request::IoCtl(r) => {
            match &r.handle {
                Some(io_ctl_request::Handle::SystemHandle(_)) => {}
                Some(io_ctl_request::Handle::ModuleHandle(m)) => ctx.module_of(Some(m))?,
                Some(io_ctl_request::Handle::CllHandle(c)) => {
                    ctx.bound(Some(c))?;
                }
                None => return Err(GuardRefusal::Undecodable("IoCtl without a handle")),
            }
            ioctl::check(ctx, r)
        }
    }
}

/// The grant of a ComParam request that passed: `Pass`, or the pinned ComParams it set.
fn sets(cll: Option<ComLogicalLinkHandle>, names: Vec<String>) -> Grant {
    match cll {
        Some(h) if !names.is_empty() => Grant::SetsComParams {
            cll: h.cll_handle,
            names,
        },
        _ => Grant::Pass,
    }
}

impl<'a> Context<'a> {
    /// The profile's links in endpoint order: one, or a T0 loopback's two.
    fn endpoint_links(&self) -> Vec<&'a Link> {
        match &self.profile.setup {
            Setup::Link { link, .. } => vec![link],
            Setup::Loopback(ends) => ends.iter().map(|e| &e.link).collect(),
        }
    }

    /// The endpoints a worker link can be created for, with their index: the VCI ends. A
    /// SocketCAN end is not on the worker. The channel is the selector of an additional channel
    /// of a VCI (SAE J2534-2 clause 7).
    fn worker_endpoints(&self) -> Vec<(usize, &'a Link, Option<&'a str>)> {
        match &self.profile.setup {
            Setup::Link { vci, link } if vci.identifies_same_module(self.vci) => {
                vec![(0, link, None)]
            }
            Setup::Link { .. } => Vec::new(),
            Setup::Loopback(ends) => ends
                .iter()
                .enumerate()
                .filter(|(_, e)| {
                    e.vci
                        .as_ref()
                        .is_some_and(|v| v.identifies_same_module(self.vci))
                })
                .map(|(i, e)| (i, &e.link, e.channel.as_deref()))
                .collect(),
        }
    }

    /// The module handle is the check's.
    fn module_of(&self, handle: Option<&ModuleHandle>) -> Result<(), GuardRefusal> {
        match handle {
            Some(h) if h.module_handle == self.module => Ok(()),
            _ => Err(GuardRefusal::Module),
        }
    }

    /// The link a handle names, with its profile link: the module must be the check's and the
    /// link one the harness bound.
    fn bound(
        &self,
        handle: Option<&ComLogicalLinkHandle>,
    ) -> Result<(&'a BoundLink, &'a Link), GuardRefusal> {
        let handle = handle.ok_or(GuardRefusal::UnknownHandle)?;
        if handle.module_handle != self.module {
            return Err(GuardRefusal::Module);
        }
        let bound = self
            .links
            .get(&handle.cll_handle)
            .ok_or(GuardRefusal::UnknownHandle)?;
        let link = *self
            .endpoint_links()
            .get(bound.endpoint)
            .ok_or(GuardRefusal::UnknownHandle)?;
        Ok((bound, link))
    }

    fn known_link(&self, handle: Option<&ComLogicalLinkHandle>) -> Result<Grant, GuardRefusal> {
        self.bound(handle).map(|_| Grant::Pass)
    }

    fn cop_known(&self, handle: Option<&ComPrimitiveHandle>) -> Result<(), GuardRefusal> {
        let handle = handle.ok_or(GuardRefusal::UnknownHandle)?;
        self.bound(Some(&ComLogicalLinkHandle {
            module_handle: handle.module_handle,
            cll_handle: handle.cll_handle,
        }))
        .map(|_| ())
    }
}

#[cfg(test)]
mod tests;
