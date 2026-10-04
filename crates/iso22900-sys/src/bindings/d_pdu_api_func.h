#ifndef C47D5884_7088_4F21_B8D4_8970B7015573
#define C47D5884_7088_4F21_B8D4_8970B7015573

#include "d_pdu_api_defs.h"

T_PDU_ERROR STDCALL PDUConstruct(CHAR8 *OptionStr, void *pAPITag);

T_PDU_ERROR STDCALL PDUDestruct();

T_PDU_ERROR STDCALL PDUIoCtl(UNUM32 hMod, UNUM32 hCLL, UNUM32 IoCtlCommandId,
                             PDU_DATA_ITEM *pInputData,
                             PDU_DATA_ITEM **pOutputData);

T_PDU_ERROR STDCALL PDUGetVersion(UNUM32 hMod, PDU_VERSION_DATA *pVersionData);

T_PDU_ERROR STDCALL PDUGetStatus(UNUM32 hMod, UNUM32 hCLL, UNUM32 hCoP,
                                 T_PDU_STATUS *pStatusCode, UNUM32 *pTimestamp,
                                 UNUM32 *pExtraInfo);

T_PDU_ERROR STDCALL PDUGetLastError(UNUM32 hMod, UNUM32 hCLL,
                                    T_PDU_ERR_EVT *pErrorCode, UNUM32 *phCoP,
                                    UNUM32 *pTimestamp,
                                    UNUM32 *pExtraErrorInfo);

T_PDU_ERROR STDCALL PDUGetResourceStatus(PDU_RSC_STATUS_ITEM *pResourceStatus);

T_PDU_ERROR STDCALL PDUCreateComLogicalLink(UNUM32 hMod, PDU_RSC_DATA *pRscData,
                                            UNUM32 resourceId, void *pCllTag,
                                            UNUM32 *phCLL,
                                            PDU_FLAG_DATA *pCllCreateFlag);

T_PDU_ERROR STDCALL PDUDestroyComLogicalLink(UNUM32 hMod, UNUM32 hCLL);

T_PDU_ERROR STDCALL PDUConnect(UNUM32 hMod, UNUM32 hCLL);

T_PDU_ERROR STDCALL PDUDisconnect(UNUM32 hMod, UNUM32 hCLL);

T_PDU_ERROR STDCALL PDULockResource(UNUM32 hMod, UNUM32 hCLL, UNUM32 LockMask);

T_PDU_ERROR STDCALL PDUUnlockResource(UNUM32 hMod, UNUM32 hCLL,
                                      UNUM32 LockMask);

T_PDU_ERROR STDCALL PDUGetComParam(UNUM32 hMod, UNUM32 hCLL, UNUM32 ParamId,
                                   PDU_PARAM_ITEM **pParamItem);

T_PDU_ERROR STDCALL PDUSetComParam(UNUM32 hMod, UNUM32 hCLL,
                                   PDU_PARAM_ITEM *pParamItem);

T_PDU_ERROR STDCALL PDUStartComPrimitive(UNUM32 hMod, UNUM32 hCLL,
                                         T_PDU_COPT CoPType, UNUM32 CoPDataSize,
                                         UNUM8 *pCoPData,
                                         PDU_COP_CTRL_DATA *pCopCtrlData,
                                         void *pCoPTag, UNUM32 *phCoP);

T_PDU_ERROR STDCALL PDUCancelComPrimitive(UNUM32 hMod, UNUM32 hCLL,
                                          UNUM32 hCoP);

T_PDU_ERROR STDCALL PDUGetEventItem(UNUM32 hMod, UNUM32 hCLL,
                                    PDU_EVENT_ITEM **pEventItem);

T_PDU_ERROR STDCALL PDUDestroyItem(PDU_ITEM *pItem);

T_PDU_ERROR STDCALL PDURegisterEventCallback(UNUM32 hMod, UNUM32 hCLL,
                                             CALLBACKFNC EventCallbackFunction);

T_PDU_ERROR STDCALL PDUGetObjectId(T_PDU_OBJT pduObjectType, CHAR8 *pShortname,
                                   UNUM32 *pPduObjectId);

T_PDU_ERROR STDCALL PDUGetModuleIds(PDU_MODULE_ITEM **pModuleIdList);

T_PDU_ERROR STDCALL PDUGetResourceIds(UNUM32 hMod,
                                      PDU_RSC_DATA *pResourceIdData,
                                      PDU_RSC_ID_ITEM **pResourceIdList);

T_PDU_ERROR STDCALL
PDUGetConflictingResources(UNUM32 resourceId, PDU_MODULE_ITEM *pInputModuleList,
                           PDU_RSC_CONFLICT_ITEM **pOutputConflictList);

T_PDU_ERROR STDCALL
PDUGetUniqueRespIdTable(UNUM32 hMod, UNUM32 hCLL,
                        PDU_UNIQUE_RESP_ID_TABLE_ITEM **pUniqueRespIdTable);

T_PDU_ERROR STDCALL
PDUSetUniqueRespIdTable(UNUM32 hMod, UNUM32 hCLL,
                        PDU_UNIQUE_RESP_ID_TABLE_ITEM *pUniqueRespIdTable);

T_PDU_ERROR STDCALL PDUModuleConnect(UNUM32 hMod);

T_PDU_ERROR STDCALL PDUModuleDisconnect(UNUM32 hMod);

T_PDU_ERROR STDCALL PDUGetTimestamp(UNUM32 hMod, UNUM32 *pTimestamp);

#endif /* C47D5884_7088_4F21_B8D4_8970B7015573 */
