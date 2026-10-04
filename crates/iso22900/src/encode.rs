use std::{ffi::c_void, ptr};

use iso22900_sys::bindings::{
    E_PDU_CPST, E_PDU_IT, PDU_COP_CTRL_DATA, PDU_ECU_UNIQUE_RESP_DATA, PDU_EXP_RESP_DATA,
    PDU_FLAG_DATA, PDU_PARAM_BYTEFIELD_DATA, PDU_PARAM_ITEM, PDU_PARAM_LONGFIELD_DATA,
    PDU_PARAM_STRUCT_ACCESS_TIMING, PDU_PARAM_STRUCT_SESS_TIMING,
    PDU_PARAM_STRUCT_TLS_VERSION_AND_CIPHER, PDU_PARAM_STRUCTFIELD_DATA, PDU_PIN_DATA,
    PDU_RSC_DATA, PDU_UNIQUE_RESP_ID_TABLE_ITEM, UNUM8, UNUM32,
};

use crate::{
    ComParam, ComParamValue, ComPrimitiveControl, DPduApiError, ParamDataType, ResourceDescriptor,
    StructFieldEncoding, UniqueResponseEntry,
};

enum ComParamStorage {
    U8(Box<u8>),
    I8(Box<i8>),
    U16(Box<u16>),
    I16(Box<i16>),
    U32(Box<u32>),
    I32(Box<i32>),
    ByteField {
        meta: Box<PDU_PARAM_BYTEFIELD_DATA>,
        _data: Vec<UNUM8>,
    },
    LongField {
        meta: Box<PDU_PARAM_LONGFIELD_DATA>,
        _data: Vec<UNUM32>,
    },
    StructField {
        meta: Box<PDU_PARAM_STRUCTFIELD_DATA>,
        _entries: StructFieldStorage,
    },
}

enum StructFieldStorage {
    SessionTiming(Vec<PDU_PARAM_STRUCT_SESS_TIMING>),
    AccessTiming(Vec<PDU_PARAM_STRUCT_ACCESS_TIMING>),
    TlsVersionAndCipher(Vec<PDU_PARAM_STRUCT_TLS_VERSION_AND_CIPHER>),
    VendorSpecific(AlignedVendorStructBuf),
}

/// A 16-byte, 16-aligned chunk -- [`AlignedVendorStructBuf`]'s backing
/// element. Deliberately the same shape as
/// `j2534-0404-service::service::rpc_misc::Align16Chunk`; duplicated here
/// (not shared via a common crate) since `iso22900` and `j2534-0404-service`
/// share no runtime dependency that could host one type. See that sibling
/// type's own doc comment for why 16 bytes -- not 8, not `u128` -- is the
/// right fixed ceiling: `pStructArray` is a `void *` into caller-allocated
/// memory, so by the C standard (C11/C17 SS7.22.3, paraphrased) a conforming
/// vendor DLL can only assume *fundamental* alignment through it -- the
/// alignment an ordinary allocator guarantees, never an extended
/// (`_Alignas`/`__m256`-class) alignment -- and 16 is at or above the
/// fundamental alignment on every target this workspace's `*-sys` crates
/// build for.
#[derive(Clone, Copy)]
#[repr(C, align(16))]
struct Align16Chunk([u8; 16]);

/// Byte-buffer storage for [`StructFieldStorage::VendorSpecific`], backed by
/// a `Vec<Align16Chunk>` rather than a plain `Vec<u8>` so the allocation's
/// actual alignment is 16 bytes regardless of its requested byte length.
///
/// A vendor STRUCTFIELD ComParam's `pStructArray` pointer is handed to the
/// native `PDUSetComParam`/`PDUSetUniqueRespIdTable` call, which -- for a
/// vendor struct type -- casts it to the vendor DLL's own vendor-defined
/// struct type (per its own alignment requirements; ADR-218's
/// `vendor_struct_types` config only declares the entry SIZE, not
/// alignment). A `Vec<u8>` is only guaranteed `align_of::<u8>()` (1-byte)
/// alignment by Rust's memory model, so handing the native side a
/// `Vec<u8>`-backed pointer it dereferences as a wider type is undefined
/// behavior -- the same bug class already fixed for raw vendor IOCTL
/// buffers in `j2534-0404-service::service::rpc_misc::AlignedByteBuf` (PR
/// #133), now on the WRITE side (`SetComParam`/`SetUniqueRespIdTable`) of
/// vendor STRUCTFIELD ComParams. The READ side (`GetComParam` /
/// `GetUniqueRespIdTable`) is unaffected: there, the native DLL itself
/// allocates the buffer this codebase only borrows (see
/// `BorrowedVendorSpecificStructArray` in `item/data/common.rs`), so
/// alignment is entirely the native side's own responsibility.
///
/// An earlier version of this type used a `Vec<u64>` (8-byte alignment)
/// instead, which Codex's PR #133 sixth round correctly pointed out is
/// still insufficient for a fundamentally-16-byte-aligned native type
/// (e.g. `long double` on the x86_64 System V ABI) -- see
/// [`Align16Chunk`]'s own doc comment for why 16 (not a wider, e.g.
/// `u128`-backed, chunk) is the right fixed ceiling rather than another
/// guess of the same kind.
///
/// The requested byte length is rounded up to a whole number of 16-byte
/// chunks (`len.div_ceil(16)`); any trailing bytes in the last partial
/// chunk are implicitly zero, since the backing `Vec<Align16Chunk>` is
/// always zero-initialized at construction and never resized afterward.
/// [`Self::as_bytes`] still reports exactly the requested byte length,
/// never the chunk-rounded-up one.
struct AlignedVendorStructBuf {
    chunks: Vec<Align16Chunk>,
    len: usize,
}

impl AlignedVendorStructBuf {
    /// Copies `bytes` into a newly allocated, zero-padded, 16-byte-aligned
    /// buffer of exactly `bytes.len()` requested bytes.
    ///
    /// Deliberately has NO sanity ceiling on `bytes.len()`, unlike the
    /// sibling `j2534-0404-service::service::rpc_misc::AlignedByteBuf`'s
    /// `VENDOR_IOCTL_MAX_RAW_CONTRACT_BYTES` (PR #133 eleventh round,
    /// design-advisor decision) -- the two mechanisms have a structurally
    /// different allocation-size source. `AlignedByteBuf`'s size comes
    /// directly from an OPERATOR-CONFIGURED `input_bytes`/`output_bytes`
    /// value, entirely independent of what any client sends, so an absurd
    /// configured value (a typo, e.g. `0xffffffff`) can reach an unbounded
    /// allocation on the very first client request regardless of that
    /// request's own size. Here, `bytes` is already a real, resident
    /// payload the client sent (`ComParamValue::StructField`'s `bytes`,
    /// itself bounded by this workspace's gRPC inbound message size limit)
    /// -- `from_bytes` only ever copies memory that already exists, it
    /// never turns an operator-configured NUMBER into a fresh allocation
    /// of that size. A future "keep the two `Aligned*Buf` copies in sync"
    /// sweep should not add a ceiling here, or remove `AlignedByteBuf`'s.
    fn from_bytes(bytes: &[u8]) -> Self {
        let mut chunks = vec![Align16Chunk([0u8; 16]); bytes.len().div_ceil(16)];
        // Safety: `chunks`'s backing allocation is at least `bytes.len()`
        // bytes long (rounded up to whole chunks above); reinterpreting it
        // as `u8` never violates alignment (every address satisfies
        // `align_of::<u8>() == 1`).
        unsafe { std::slice::from_raw_parts_mut(chunks.as_mut_ptr().cast::<u8>(), bytes.len()) }
            .copy_from_slice(bytes);
        AlignedVendorStructBuf {
            chunks,
            len: bytes.len(),
        }
    }

    fn len(&self) -> usize {
        self.len
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// A byte-slice view of exactly the requested (not chunk-rounded-up)
    /// byte length.
    fn as_bytes(&self) -> &[u8] {
        // Safety: `chunks` is a valid, initialized `Vec<Align16Chunk>`
        // allocation of at least `self.len` bytes (rounded up to whole
        // chunks at construction); reinterpreting it as `u8` never
        // violates alignment, and `self.len` never exceeds the
        // allocation's actual byte length.
        unsafe { std::slice::from_raw_parts(self.chunks.as_ptr().cast::<u8>(), self.len) }
    }
}

impl ComParamStorage {
    fn as_mut_ptr(&mut self) -> *mut c_void {
        match self {
            Self::U8(value) => (&mut **value as *mut u8).cast(),
            Self::I8(value) => (&mut **value as *mut i8).cast(),
            Self::U16(value) => (&mut **value as *mut u16).cast(),
            Self::I16(value) => (&mut **value as *mut i16).cast(),
            Self::U32(value) => (&mut **value as *mut u32).cast(),
            Self::I32(value) => (&mut **value as *mut i32).cast(),
            Self::ByteField { meta, .. } => (&mut **meta as *mut PDU_PARAM_BYTEFIELD_DATA).cast(),
            Self::LongField { meta, .. } => (&mut **meta as *mut PDU_PARAM_LONGFIELD_DATA).cast(),
            Self::StructField { meta, .. } => {
                (&mut **meta as *mut PDU_PARAM_STRUCTFIELD_DATA).cast()
            }
        }
    }
}

fn encode_struct_entries_from_bytes<T: Copy>(
    encoding: &StructFieldEncoding,
) -> Result<Vec<T>, DPduApiError> {
    let entry_size = std::mem::size_of::<T>();
    if encoding.entry_size != entry_size {
        return Err(DPduApiError::Unsupported(
            "structfield com parameter entries do not match expected size",
        ));
    }
    if !encoding.bytes.len().is_multiple_of(entry_size) {
        return Err(DPduApiError::Unsupported(
            "structfield com parameter payload size is not aligned to entry size",
        ));
    }

    let entry_count = encoding.bytes.len() / entry_size;
    let mut values: Vec<T> = Vec::with_capacity(entry_count);
    if entry_count > 0 {
        unsafe {
            ptr::copy_nonoverlapping(
                encoding.bytes.as_ptr(),
                values.as_mut_ptr().cast::<u8>(),
                encoding.bytes.len(),
            );
            values.set_len(entry_count);
        }
    }

    Ok(values)
}

pub(crate) struct EncodedResourceDescriptor {
    pub(crate) ffi: PDU_RSC_DATA,
    _pins: Vec<PDU_PIN_DATA>,
}

impl EncodedResourceDescriptor {
    pub(crate) fn new(descriptor: &ResourceDescriptor) -> Self {
        let mut pins: Vec<PDU_PIN_DATA> = descriptor
            .pins
            .iter()
            .map(|pin| PDU_PIN_DATA {
                DLCPinNumber: pin.dlc_pin_number,
                DLCPinTypeId: pin.dlc_pin_type_id.0,
            })
            .collect();
        let ffi = PDU_RSC_DATA {
            BusTypeId: descriptor.bus_type_id.0,
            ProtocolId: descriptor.protocol_id.0,
            NumPinData: pins.len() as UNUM32,
            pDLCPinData: if pins.is_empty() {
                ptr::null_mut()
            } else {
                pins.as_mut_ptr()
            },
        };
        Self { ffi, _pins: pins }
    }
}

pub(crate) struct EncodedFlagData {
    pub(crate) ffi: PDU_FLAG_DATA,
    _bytes: Vec<UNUM8>,
}

impl EncodedFlagData {
    pub(crate) fn from_bytes(bytes: Vec<u8>) -> Self {
        let mut bytes = bytes;
        let ffi = PDU_FLAG_DATA {
            NumFlagBytes: bytes.len() as UNUM32,
            pFlagData: if bytes.is_empty() {
                ptr::null_mut()
            } else {
                bytes.as_mut_ptr()
            },
        };
        Self { ffi, _bytes: bytes }
    }
}

pub(crate) struct EncodedComParam {
    pub(crate) ffi: PDU_PARAM_ITEM,
    _storage: ComParamStorage,
}

fn infer_data_type_from_value(value: &ComParamValue) -> Option<ParamDataType> {
    match value {
        ComParamValue::U8(_) => Some(ParamDataType::U8),
        ComParamValue::I8(_) => Some(ParamDataType::I8),
        ComParamValue::U16(_) => Some(ParamDataType::U16),
        ComParamValue::I16(_) => Some(ParamDataType::I16),
        ComParamValue::U32(_) => Some(ParamDataType::U32),
        ComParamValue::I32(_) => Some(ParamDataType::I32),
        ComParamValue::Bytes(_) => Some(ParamDataType::BYTE_FIELD),
        ComParamValue::Longs(_) => Some(ParamDataType::LONG_FIELD),
        ComParamValue::StructField { .. } => Some(ParamDataType::STRUCT_FIELD),
        ComParamValue::Unsupported { .. } => None,
    }
}

impl EncodedComParam {
    pub(crate) fn new_owned(param: ComParam) -> Result<Self, DPduApiError> {
        let ComParam {
            id,
            data_type,
            class,
            value,
        } = param;
        let com_param_data_type = infer_data_type_from_value(&value).unwrap_or(data_type);

        let mut storage = match value {
            ComParamValue::U8(value) => ComParamStorage::U8(Box::new(value)),
            ComParamValue::I8(value) => ComParamStorage::I8(Box::new(value)),
            ComParamValue::U16(value) => ComParamStorage::U16(Box::new(value)),
            ComParamValue::I16(value) => ComParamStorage::I16(Box::new(value)),
            ComParamValue::U32(value) => ComParamStorage::U32(Box::new(value)),
            ComParamValue::I32(value) => ComParamStorage::I32(Box::new(value)),
            ComParamValue::Bytes(bytes) => {
                let mut data = bytes;
                let mut meta = Box::new(PDU_PARAM_BYTEFIELD_DATA {
                    ParamMaxLen: data.len() as UNUM32,
                    ParamActLen: data.len() as UNUM32,
                    pDataArray: ptr::null_mut(),
                });
                meta.pDataArray = if data.is_empty() {
                    ptr::null_mut()
                } else {
                    data.as_mut_ptr()
                };
                ComParamStorage::ByteField { meta, _data: data }
            }
            ComParamValue::Longs(values) => {
                let mut data = values;
                let mut meta = Box::new(PDU_PARAM_LONGFIELD_DATA {
                    ParamMaxLen: data.len() as UNUM32,
                    ParamActLen: data.len() as UNUM32,
                    pDataArray: ptr::null_mut(),
                });
                meta.pDataArray = if data.is_empty() {
                    ptr::null_mut()
                } else {
                    data.as_mut_ptr()
                };
                ComParamStorage::LongField { meta, _data: data }
            }
            ComParamValue::StructField {
                struct_type,
                encoding,
            } => {
                let typed_entries = match struct_type {
                    E_PDU_CPST::PDU_CPST_SESSION_TIMING => StructFieldStorage::SessionTiming(
                        encode_struct_entries_from_bytes(&encoding)?,
                    ),
                    E_PDU_CPST::PDU_CPST_ACCESS_TIMING => StructFieldStorage::AccessTiming(
                        encode_struct_entries_from_bytes(&encoding)?,
                    ),
                    E_PDU_CPST::PDU_CPST_TLS_VERSION_AND_CIPHER => {
                        StructFieldStorage::TlsVersionAndCipher(encode_struct_entries_from_bytes(
                            &encoding,
                        )?)
                    }
                    _ => {
                        if encoding.entry_size == 0 {
                            // A zero-length vendor STRUCTFIELD write (entry
                            // size and entry count both zero) is legitimate
                            // per ADR-218 Decision item 3; only reject a
                            // zero entry size when there is a non-empty
                            // payload to divide by it.
                            if !encoding.bytes.is_empty() {
                                return Err(DPduApiError::Unsupported(
                                    "vendor specific structfield entry size must be greater than zero when payload is non-empty",
                                ));
                            }
                        } else if encoding.bytes.len() % encoding.entry_size != 0 {
                            return Err(DPduApiError::Unsupported(
                                "vendor specific structfield payload size is not aligned to entry size",
                            ));
                        }
                        StructFieldStorage::VendorSpecific(AlignedVendorStructBuf::from_bytes(
                            &encoding.bytes,
                        ))
                    }
                };

                let param_entries = match &typed_entries {
                    StructFieldStorage::SessionTiming(items) => items.len() as UNUM32,
                    StructFieldStorage::AccessTiming(items) => items.len() as UNUM32,
                    StructFieldStorage::TlsVersionAndCipher(items) => items.len() as UNUM32,
                    StructFieldStorage::VendorSpecific(bytes) => {
                        bytes.len().checked_div(encoding.entry_size).unwrap_or(0) as UNUM32
                    }
                };

                let mut meta = Box::new(PDU_PARAM_STRUCTFIELD_DATA {
                    ComParamStructType: struct_type,
                    ParamMaxEntries: param_entries,
                    ParamActEntries: param_entries,
                    pStructArray: ptr::null_mut(),
                });

                meta.pStructArray = match &typed_entries {
                    StructFieldStorage::SessionTiming(items) => {
                        if items.is_empty() {
                            ptr::null_mut()
                        } else {
                            items.as_ptr().cast_mut().cast()
                        }
                    }
                    StructFieldStorage::AccessTiming(items) => {
                        if items.is_empty() {
                            ptr::null_mut()
                        } else {
                            items.as_ptr().cast_mut().cast()
                        }
                    }
                    StructFieldStorage::TlsVersionAndCipher(items) => {
                        if items.is_empty() {
                            ptr::null_mut()
                        } else {
                            items.as_ptr().cast_mut().cast()
                        }
                    }
                    StructFieldStorage::VendorSpecific(bytes) => {
                        if bytes.is_empty() {
                            ptr::null_mut()
                        } else {
                            bytes.as_bytes().as_ptr().cast_mut().cast()
                        }
                    }
                };

                ComParamStorage::StructField {
                    meta,
                    _entries: typed_entries,
                }
            }
            ComParamValue::Unsupported { .. } => {
                return Err(DPduApiError::Unsupported("unsupported com parameter value"));
            }
        };

        let ffi = PDU_PARAM_ITEM {
            ItemType: E_PDU_IT::PDU_IT_PARAM,
            ComParamId: id.0,
            ComParamDataType: com_param_data_type.as_raw(),
            ComParamClass: class.as_raw(),
            pComParamData: storage.as_mut_ptr(),
        };

        Ok(Self {
            ffi,
            _storage: storage,
        })
    }
}

pub(crate) struct EncodedComPrimitiveControl {
    pub(crate) ffi: PDU_COP_CTRL_DATA,
    _flags: EncodedFlagData,
    // Each entry's pointers are stable because they point into the flat backing buffers below.
    _response_ffi: Vec<PDU_EXP_RESP_DATA>,
    _masks_data: Vec<UNUM8>,
    _patterns_data: Vec<UNUM8>,
    _unique_resp_ids_data: Vec<UNUM32>,
}

impl EncodedComPrimitiveControl {
    pub(crate) fn new_owned(control: ComPrimitiveControl) -> Self {
        let ComPrimitiveControl {
            time,
            send_cycles,
            receive_cycles,
            temp_param_update,
            tx_flags,
            expected_responses,
        } = control;
        let flags = EncodedFlagData::from_bytes(tx_flags.bytes);

        let n = expected_responses.len();
        let total_mask_bytes = expected_responses
            .iter()
            .map(|resp| resp.mask.len())
            .sum::<usize>();
        let total_pattern_bytes = expected_responses
            .iter()
            .map(|resp| resp.pattern.len())
            .sum::<usize>();
        let total_unique_resp_ids = expected_responses
            .iter()
            .map(|resp| resp.unique_response_ids.len())
            .sum::<usize>();

        let mut response_ffi: Vec<PDU_EXP_RESP_DATA> = Vec::with_capacity(n);
        let mut masks_data: Vec<UNUM8> = Vec::with_capacity(total_mask_bytes);
        let mut patterns_data: Vec<UNUM8> = Vec::with_capacity(total_pattern_bytes);
        let mut unique_resp_ids_data: Vec<UNUM32> = Vec::with_capacity(total_unique_resp_ids);

        for resp in expected_responses {
            let mask_start = masks_data.len();
            masks_data.extend_from_slice(&resp.mask);
            let pattern_start = patterns_data.len();
            patterns_data.extend_from_slice(&resp.pattern);
            let unique_ids_start = unique_resp_ids_data.len();
            unique_resp_ids_data.extend_from_slice(&resp.unique_response_ids);

            let mask_len = resp.mask.len();
            let pattern_len = resp.pattern.len();
            let unique_ids_len = resp.unique_response_ids.len();

            response_ffi.push(PDU_EXP_RESP_DATA {
                ResponseType: resp.response_type,
                AcceptanceId: resp.acceptance_id,
                NumMaskPatternBytes: mask_len as UNUM32,
                pMaskData: if mask_len == 0 {
                    ptr::null_mut()
                } else {
                    unsafe { masks_data.as_mut_ptr().add(mask_start) }
                },
                pPatternData: if pattern_len == 0 {
                    ptr::null_mut()
                } else {
                    unsafe { patterns_data.as_mut_ptr().add(pattern_start) }
                },
                NumUniqueRespIds: unique_ids_len as UNUM32,
                pUniqueRespIds: if unique_ids_len == 0 {
                    ptr::null_mut()
                } else {
                    unsafe { unique_resp_ids_data.as_mut_ptr().add(unique_ids_start) }
                },
            });
        }

        let ffi = PDU_COP_CTRL_DATA {
            Time: time,
            NumSendCycles: send_cycles,
            NumReceiveCycles: receive_cycles,
            TempParamUpdate: temp_param_update,
            TxFlag: flags.ffi,
            NumPossibleExpectedResponses: response_ffi.len() as UNUM32,
            pExpectedResponseArray: if response_ffi.is_empty() {
                ptr::null_mut()
            } else {
                response_ffi.as_mut_ptr()
            },
        };
        Self {
            ffi,
            _flags: flags,
            _response_ffi: response_ffi,
            _masks_data: masks_data,
            _patterns_data: patterns_data,
            _unique_resp_ids_data: unique_resp_ids_data,
        }
    }
}

pub(crate) struct EncodedUniqueResponseTable {
    pub(crate) ffi: PDU_UNIQUE_RESP_ID_TABLE_ITEM,
    _entries: Vec<PDU_ECU_UNIQUE_RESP_DATA>,
    // EncodedComParam keeps FFI pointers stable via its _storage field.
    _encoded_params: Vec<EncodedComParam>,
    // Flat PDU_PARAM_ITEM storage; each unique-data entry points into this buffer.
    _param_ffi: Vec<PDU_PARAM_ITEM>,
    _scalar_u8_data: Vec<u8>,
    _scalar_i8_data: Vec<i8>,
    _scalar_u16_data: Vec<u16>,
    _scalar_i16_data: Vec<i16>,
    _scalar_u32_data: Vec<u32>,
    _scalar_i32_data: Vec<i32>,
}

impl EncodedUniqueResponseTable {
    pub(crate) fn new_owned(entries: Vec<UniqueResponseEntry>) -> Result<Self, DPduApiError> {
        let n = entries.len();
        let total_params = entries
            .iter()
            .map(|entry| entry.params.len())
            .sum::<usize>();

        let mut scalar_u8_count = 0_usize;
        let mut scalar_i8_count = 0_usize;
        let mut scalar_u16_count = 0_usize;
        let mut scalar_i16_count = 0_usize;
        let mut scalar_u32_count = 0_usize;
        let mut scalar_i32_count = 0_usize;
        for entry in &entries {
            for param in &entry.params {
                match &param.value {
                    ComParamValue::U8(_) => scalar_u8_count += 1,
                    ComParamValue::I8(_) => scalar_i8_count += 1,
                    ComParamValue::U16(_) => scalar_u16_count += 1,
                    ComParamValue::I16(_) => scalar_i16_count += 1,
                    ComParamValue::U32(_) => scalar_u32_count += 1,
                    ComParamValue::I32(_) => scalar_i32_count += 1,
                    _ => {}
                }
            }
        }

        let mut encoded_params: Vec<EncodedComParam> = Vec::with_capacity(total_params);
        let mut param_ffi: Vec<PDU_PARAM_ITEM> = Vec::with_capacity(total_params);
        let mut ffi_entries: Vec<PDU_ECU_UNIQUE_RESP_DATA> = Vec::with_capacity(n);
        let mut scalar_u8_data: Vec<u8> = Vec::with_capacity(scalar_u8_count);
        let mut scalar_i8_data: Vec<i8> = Vec::with_capacity(scalar_i8_count);
        let mut scalar_u16_data: Vec<u16> = Vec::with_capacity(scalar_u16_count);
        let mut scalar_i16_data: Vec<i16> = Vec::with_capacity(scalar_i16_count);
        let mut scalar_u32_data: Vec<u32> = Vec::with_capacity(scalar_u32_count);
        let mut scalar_i32_data: Vec<i32> = Vec::with_capacity(scalar_i32_count);

        for entry in entries {
            let UniqueResponseEntry {
                unique_response_id,
                params: entry_params,
            } = entry;

            let start_index = param_ffi.len();
            for param in entry_params {
                let ComParam {
                    id,
                    data_type,
                    class,
                    value,
                } = param;

                match value {
                    ComParamValue::U8(value) => {
                        scalar_u8_data.push(value);
                        param_ffi.push(PDU_PARAM_ITEM {
                            ItemType: E_PDU_IT::PDU_IT_PARAM,
                            ComParamId: id.0,
                            ComParamDataType: ParamDataType::U8.as_raw(),
                            ComParamClass: class.as_raw(),
                            pComParamData: scalar_u8_data.last_mut().expect("scalar arena reserved")
                                as *mut u8
                                as *mut c_void,
                        });
                    }
                    ComParamValue::I8(value) => {
                        scalar_i8_data.push(value);
                        param_ffi.push(PDU_PARAM_ITEM {
                            ItemType: E_PDU_IT::PDU_IT_PARAM,
                            ComParamId: id.0,
                            ComParamDataType: ParamDataType::I8.as_raw(),
                            ComParamClass: class.as_raw(),
                            pComParamData: scalar_i8_data.last_mut().expect("scalar arena reserved")
                                as *mut i8
                                as *mut c_void,
                        });
                    }
                    ComParamValue::U16(value) => {
                        scalar_u16_data.push(value);
                        param_ffi.push(PDU_PARAM_ITEM {
                            ItemType: E_PDU_IT::PDU_IT_PARAM,
                            ComParamId: id.0,
                            ComParamDataType: ParamDataType::U16.as_raw(),
                            ComParamClass: class.as_raw(),
                            pComParamData: scalar_u16_data
                                .last_mut()
                                .expect("scalar arena reserved")
                                as *mut u16
                                as *mut c_void,
                        });
                    }
                    ComParamValue::I16(value) => {
                        scalar_i16_data.push(value);
                        param_ffi.push(PDU_PARAM_ITEM {
                            ItemType: E_PDU_IT::PDU_IT_PARAM,
                            ComParamId: id.0,
                            ComParamDataType: ParamDataType::I16.as_raw(),
                            ComParamClass: class.as_raw(),
                            pComParamData: scalar_i16_data
                                .last_mut()
                                .expect("scalar arena reserved")
                                as *mut i16
                                as *mut c_void,
                        });
                    }
                    ComParamValue::U32(value) => {
                        scalar_u32_data.push(value);
                        param_ffi.push(PDU_PARAM_ITEM {
                            ItemType: E_PDU_IT::PDU_IT_PARAM,
                            ComParamId: id.0,
                            ComParamDataType: ParamDataType::U32.as_raw(),
                            ComParamClass: class.as_raw(),
                            pComParamData: scalar_u32_data
                                .last_mut()
                                .expect("scalar arena reserved")
                                as *mut u32
                                as *mut c_void,
                        });
                    }
                    ComParamValue::I32(value) => {
                        scalar_i32_data.push(value);
                        param_ffi.push(PDU_PARAM_ITEM {
                            ItemType: E_PDU_IT::PDU_IT_PARAM,
                            ComParamId: id.0,
                            ComParamDataType: ParamDataType::I32.as_raw(),
                            ComParamClass: class.as_raw(),
                            pComParamData: scalar_i32_data
                                .last_mut()
                                .expect("scalar arena reserved")
                                as *mut i32
                                as *mut c_void,
                        });
                    }
                    other => {
                        let param = ComParam {
                            id,
                            data_type,
                            class,
                            value: other,
                        };
                        let encoded = EncodedComParam::new_owned(param)?;
                        param_ffi.push(encoded.ffi);
                        encoded_params.push(encoded);
                    }
                }
            }

            let entry_len = param_ffi.len() - start_index;
            let entry_params_ptr = if entry_len == 0 {
                ptr::null_mut()
            } else {
                unsafe { param_ffi.as_mut_ptr().add(start_index) }
            };

            ffi_entries.push(PDU_ECU_UNIQUE_RESP_DATA {
                UniqueRespIdentifier: unique_response_id,
                NumParamItems: entry_len as UNUM32,
                pParams: entry_params_ptr,
            });
        }

        let ffi = PDU_UNIQUE_RESP_ID_TABLE_ITEM {
            ItemType: E_PDU_IT::PDU_IT_UNIQUE_RESP_ID_TABLE,
            NumEntries: ffi_entries.len() as UNUM32,
            pUniqueData: if ffi_entries.is_empty() {
                ptr::null_mut()
            } else {
                ffi_entries.as_mut_ptr()
            },
        };

        Ok(Self {
            ffi,
            _entries: ffi_entries,
            _encoded_params: encoded_params,
            _param_ffi: param_ffi,
            _scalar_u8_data: scalar_u8_data,
            _scalar_i8_data: scalar_i8_data,
            _scalar_u16_data: scalar_u16_data,
            _scalar_i16_data: scalar_i16_data,
            _scalar_u32_data: scalar_u32_data,
            _scalar_i32_data: scalar_i32_data,
        })
    }
}

#[cfg(test)]
mod aligned_vendor_struct_buf_tests {
    use super::*;

    /// The actual property this type exists to establish: the pointer
    /// handed to the native call must be aligned to at least 16 bytes,
    /// regardless of the requested byte length -- including lengths that
    /// are not themselves multiples of 16, which is exactly the case a
    /// plain `Vec<u8>` (1-byte-aligned by Rust's memory model) cannot
    /// guarantee. Mirrors
    /// `j2534-0404-service::service::rpc_misc::AlignedByteBuf`'s equivalent
    /// test (PR #133).
    #[test]
    fn backing_allocation_is_16_byte_aligned_for_a_range_of_byte_lengths() {
        for len in [0, 1, 3, 4, 7, 8, 9, 15, 16, 17] {
            let buf = AlignedVendorStructBuf::from_bytes(&vec![0xAB; len]);
            let ptr = buf.as_bytes().as_ptr();
            assert_eq!(
                ptr as usize % 16,
                0,
                "AlignedVendorStructBuf::from_bytes(len={len})'s pointer must be 16-byte-aligned"
            );
        }
    }

    #[test]
    fn align16chunk_is_actually_16_byte_aligned() {
        assert_eq!(std::mem::align_of::<Align16Chunk>(), 16);
        assert_eq!(std::mem::size_of::<Align16Chunk>(), 16);
    }

    /// `from_bytes` must copy the payload bytes through unchanged and
    /// report exactly the requested byte length, never the
    /// chunk-rounded-up backing allocation's length.
    #[test]
    fn from_bytes_copies_payload_and_reports_exact_length() {
        let payload = [1u8, 2, 3, 4, 5];
        let buf = AlignedVendorStructBuf::from_bytes(&payload);
        assert_eq!(buf.len(), payload.len());
        assert_eq!(buf.as_bytes(), &payload[..]);
    }

    #[test]
    fn from_bytes_empty_is_empty() {
        let buf = AlignedVendorStructBuf::from_bytes(&[]);
        assert!(buf.is_empty());
        assert_eq!(buf.as_bytes(), &[] as &[u8]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn move_value<T>(value: T) -> T {
        value
    }

    #[test]
    fn encoded_resource_descriptor_keeps_pin_pointer_after_move() {
        let descriptor = ResourceDescriptor {
            bus_type_id: crate::ObjectId(1),
            protocol_id: crate::ObjectId(2),
            pins: vec![crate::PinData {
                dlc_pin_number: 7,
                dlc_pin_type_id: crate::ObjectId(11),
            }],
        };

        let encoded = EncodedResourceDescriptor::new(&descriptor);
        let pins_ptr_before = encoded.ffi.pDLCPinData;
        assert!(!pins_ptr_before.is_null());
        assert_eq!(unsafe { (*pins_ptr_before).DLCPinNumber }, 7);

        let encoded = move_value(encoded);
        assert_eq!(encoded.ffi.pDLCPinData, pins_ptr_before);
        assert_eq!(unsafe { (*encoded.ffi.pDLCPinData).DLCPinTypeId }, 11);
    }

    #[test]
    fn encoded_com_param_keeps_data_pointer_after_move() {
        let param = ComParam {
            id: crate::ObjectId(42),
            data_type: ParamDataType::U32,
            class: crate::ParamClass::COM,
            value: ComParamValue::U32(0xAABBCCDD),
        };

        let encoded = EncodedComParam::new_owned(param).expect("encoding should succeed");
        let data_ptr_before = encoded.ffi.pComParamData.cast::<u32>();
        assert!(!data_ptr_before.is_null());
        assert_eq!(unsafe { *data_ptr_before }, 0xAABBCCDD);

        let encoded = move_value(encoded);
        assert_eq!(encoded.ffi.pComParamData.cast::<u32>(), data_ptr_before);
        assert_eq!(
            unsafe { *encoded.ffi.pComParamData.cast::<u32>() },
            0xAABBCCDD
        );
    }

    #[test]
    fn encoded_com_param_accepts_zero_length_vendor_structfield() {
        // Regression test: a zero-length vendor STRUCTFIELD write
        // (`entry_size == 0` with an empty payload, i.e. `count == 0`) is a
        // legitimate write per ADR-218 Decision item 3 -- `entry_size == 0`
        // is only invalid when there is a nonzero count of entries to
        // divide the payload by. This mirrors
        // `iso22900-service::service::convert::vendor_struct_from_proto`'s
        // write-side validation, which already accepted this shape; this
        // arm previously rejected it unconditionally and would otherwise
        // panic dividing by a zero `entry_size` if it hadn't.
        let param = ComParam {
            id: crate::ObjectId(7),
            data_type: ParamDataType::STRUCT_FIELD,
            class: crate::ParamClass::TIMING,
            value: ComParamValue::StructField {
                struct_type: E_PDU_CPST(0x8000_0001_u32 as _),
                encoding: StructFieldEncoding {
                    entry_size: 0,
                    bytes: vec![],
                },
            },
        };

        let encoded = EncodedComParam::new_owned(param)
            .expect("a zero-length vendor structfield should encode successfully");
        let meta = encoded
            .ffi
            .pComParamData
            .cast::<PDU_PARAM_STRUCTFIELD_DATA>();
        assert!(!meta.is_null());
        assert_eq!(unsafe { (*meta).ParamActEntries }, 0);
        assert!(unsafe { (*meta).pStructArray }.is_null());
    }

    #[test]
    fn encoded_com_primitive_control_keeps_nested_pointers_after_move() {
        let control = ComPrimitiveControl {
            time: 10,
            send_cycles: 1,
            receive_cycles: 2,
            temp_param_update: 3,
            tx_flags: crate::FlagData {
                bytes: vec![0x11, 0x22],
            },
            expected_responses: vec![crate::ExpectedResponse {
                response_type: 4,
                acceptance_id: 5,
                mask: vec![0xAA, 0xBB],
                pattern: vec![0xCC, 0xDD],
                unique_response_ids: vec![7, 8],
            }],
        };

        let encoded = EncodedComPrimitiveControl::new_owned(control);
        let flag_ptr_before = encoded.ffi.TxFlag.pFlagData;
        let exp_ptr_before = encoded.ffi.pExpectedResponseArray;
        assert!(!flag_ptr_before.is_null());
        assert!(!exp_ptr_before.is_null());

        let first_before = unsafe { &*exp_ptr_before };
        let mask_ptr_before = first_before.pMaskData;
        let pattern_ptr_before = first_before.pPatternData;
        let unique_ids_ptr_before = first_before.pUniqueRespIds;
        assert!(!mask_ptr_before.is_null());
        assert!(!pattern_ptr_before.is_null());
        assert!(!unique_ids_ptr_before.is_null());
        assert_eq!(unsafe { *mask_ptr_before }, 0xAA);
        assert_eq!(unsafe { *pattern_ptr_before }, 0xCC);
        assert_eq!(unsafe { *unique_ids_ptr_before }, 7);

        let encoded = move_value(encoded);
        assert_eq!(encoded.ffi.TxFlag.pFlagData, flag_ptr_before);
        assert_eq!(encoded.ffi.pExpectedResponseArray, exp_ptr_before);

        let first_after = unsafe { &*encoded.ffi.pExpectedResponseArray };
        assert_eq!(first_after.pMaskData, mask_ptr_before);
        assert_eq!(first_after.pPatternData, pattern_ptr_before);
        assert_eq!(first_after.pUniqueRespIds, unique_ids_ptr_before);
        assert_eq!(unsafe { *first_after.pMaskData }, 0xAA);
        assert_eq!(unsafe { *first_after.pPatternData }, 0xCC);
        assert_eq!(unsafe { *first_after.pUniqueRespIds }, 7);
    }

    #[test]
    fn encoded_unique_response_table_keeps_param_pointers_after_move() {
        let entries = vec![UniqueResponseEntry {
            unique_response_id: 99,
            params: vec![ComParam {
                id: crate::ObjectId(123),
                data_type: ParamDataType::U16,
                class: crate::ParamClass::COM,
                value: ComParamValue::U16(0x3344),
            }],
        }];

        let encoded = EncodedUniqueResponseTable::new_owned(entries)
            .expect("encoding unique response table should succeed");
        let unique_data_ptr_before = encoded.ffi.pUniqueData;
        assert!(!unique_data_ptr_before.is_null());

        let first_entry_before = unsafe { &*unique_data_ptr_before };
        let params_ptr_before = first_entry_before.pParams;
        assert!(!params_ptr_before.is_null());
        let first_param_data_ptr_before =
            unsafe { (*params_ptr_before).pComParamData.cast::<u16>() };
        assert!(!first_param_data_ptr_before.is_null());
        assert_eq!(unsafe { *first_param_data_ptr_before }, 0x3344);

        let encoded = move_value(encoded);
        assert_eq!(encoded.ffi.pUniqueData, unique_data_ptr_before);

        let first_entry_after = unsafe { &*encoded.ffi.pUniqueData };
        assert_eq!(first_entry_after.pParams, params_ptr_before);
        let first_param_data_ptr_after =
            unsafe { (*first_entry_after.pParams).pComParamData.cast::<u16>() };
        assert_eq!(first_param_data_ptr_after, first_param_data_ptr_before);
        assert_eq!(unsafe { *first_param_data_ptr_after }, 0x3344);
    }
}
