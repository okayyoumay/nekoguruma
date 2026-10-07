# ADR-237: Regulatory Scope: Use the Access Routes Regulations Open, Without Tool Certification

**Date:** 2026-10-06
**Status:** Accepted
**Affects:** design 8.1, 9.2, 16.1, 16.2, 17 (P7 removed); `db/` retention (`retain_until`); generic OBD vehicle-knowledge package

## Context

Design 17 P7 left open which vehicle regulations the system supports: whether to follow the US
CARB / EPA generic scan tool requirements and the EU repairer access rules was said to depend on
the target markets. Design 16.2 already covers UNECE R155 / R156, VINs as personal data, rights to
ODX / PDX data and SAE J3138.

A survey of the main markets (October 2026) found that almost every regulation places its
obligations on vehicle makers or on repair and inspection businesses, not on diagnostic software:

- **United States.** CARB and EPA OBD rules require vehicles to report emissions-related
  diagnostic data to a generic scan tool (SAE J1979; for heavy-duty vehicles SAE J1979-2,
  "OBD on UDS", phases in from model year 2027, earlier for some makers). CARB's service
  information rules and state right-to-repair laws require makers to offer reprogramming through
  SAE J2534 pass-thru devices (TMC RP1210 is also allowed for heavy-duty). A tool only carries its
  own obligations when it is offered as a CARB-certified OBD test tool for inspection programs.
- **European Union.** Regulation (EU) 2018/858, Annex X (amended by Delegated Regulation (EU)
  2021/1244), requires makers to give independent operators non-discriminatory access to OBD
  data and repair and maintenance information (RMI), including diagnosis and reprogramming with
  standard pass-thru interfaces. Delegated Regulation (EU) 2026/699 (in force since June 2026;
  its secure-access rules apply from then, and only some of the makers' information-provision
  duties are staged until 2028) adds an appendix to Annex X that lets makers protect OBD
  and on-board access with security measures such as a secure gateway. Within limits it sets, a
  maker may authenticate the diagnostic tool and its manufacturer, the independent operator when
  the access changes the vehicle, and in some cases the employee; standardized access to OBD data
  and RMI must remain. Access to security-related RMI (immobiliser, key programming, theft
  protection) goes through SERMI: conformity assessment bodies are accredited, and they approve
  the repair business and authorise its employees. The authorised employee holds the credential
  (currently in a wallet app) and presents it interactively to the maker's portal, which then
  grants the session. The Data Act
  (applicable since September 2025) obliges makers, as data holders, to make vehicle data
  available to the user and to third parties the user names, and binds those third parties to
  purpose limitation, no onward disclosure beyond what the user agreed and security of the data.
  A business that uses this system to receive data that way is such a third party; the software
  itself is not. Euro 7 does not yet add requirements on tools.
- **Japan.** OBD inspection (from October 2024, imported vehicles from October 2025) reads
  specified DTCs with an application provided by the National Agency for Automobile and Land
  Transport Technology, run on a scan tool that meets the inspection requirements. Specified
  maintenance (tokutei seibi) certifies the repair business, not its tools.

So the software is directly regulated as a tool only if it seeks certification as an inspection
tool; the duties of a data recipient fall on the operator that runs it.
Everything else is a question of whether the system can use the access routes that the
regulations force makers to open: generic OBD, reprogramming through standard pass-thru
interfaces, and security-related functions behind the maker's authentication.

The options were: (A) support those access routes and seek no tool certification; (B) A plus
certification as one market's inspection tool; (C) leave generic OBD out and support only
maker-specific diagnosis from ODX or the proprietary format; (D) keep P7 open until the target
markets are chosen. The maintainer chose A.

## Decision

1. **No inspection-tool certification.** The system is not built or certified as a regulated
   inspection tool (a CARB-certified OBD test tool, a scan tool for Japan's OBD inspection, or
   similar). Pursuing one later is a separate decision that adds to the items below without
   changing them.
2. **Generic OBD is part of the standard function set.** Reading emissions-related generic OBD
   data (SAE J1979 / ISO 15031-5 on ISO 15765-4, SAE J1979-2, and WWH-OBD per ISO 27145) is
   provided as a standard vehicle-knowledge package: L3 data in the IR (8.1, 8.2), distributed and
   signed like any other extension package, running on the shared L2 primitives. The system does
   not depend on a maker's ODX for these reads. Classic generic OBD starts with ISO 15765-4 (CAN);
   the older transports ISO 15031-5 also runs on (ISO 9141-2, ISO 14230-4, SAE J1850), which the
   workers already support, follow as separate work in the same package.
3. **Reprogramming routes stay within the existing design.** The system's own reprogramming jobs
   (5.6, 8.2.5) use VCIs through the J2534 and D-PDU API workers. The system does not present
   itself as a J2534 or D-PDU API library to a maker's own reprogramming application; when such an
   application uses the VCI directly, the exclusive control in 8.8 only detects it. TMC RP1210 is
   out of scope.
4. **Security-related access goes through the OEM authentication provider.** SERMI and similar
   maker schemes are handled by the OEM authentication provider extension point (9.2) under the
   rules of 8.10. The credential stays with the authorised employee (for SERMI, in their
   wallet) and is presented by that person to the maker's portal; neither the server, the provider
   nor the agent holds or relays it. When the scheme needs that interactive step, the provider
   shows the operator the portal's prompt (for example a QR code to scan with the wallet) through
   a server-originated operator prompt in the web UI, and waits for the portal's grant. The HMI
   request path of 8.6 is agent-originated and is not used for this. The prompt carries only the
   portal's challenge, never the credential. The audit log records which operator ran the
   operation and the authorization reference the provider returns. Authentication of the
   diagnostic tool or of the business by a maker's secure gateway (2026/699) also goes through the
   provider. For that purpose the tool manufacturer is the framework user that ships a product
   built on the framework, not the framework; the framework user registers the product with the
   makers, and its tool credentials are server-side keys kept like the seed-key secrets of 8.10.
   Such registration is a maker access condition, not the inspection-tool certification of
   item 1.
5. **Retention periods are operator settings.** Statutory retention periods for maintenance
   records and for the audit records of write jobs differ by jurisdiction (UNECE R156 evidence
   retention is the vehicle maker's, under its own SUMS). The framework provides a configurable
   retention period per tenant and record kind and ships no jurisdiction-specific defaults; the
   operator sets them (16.1).
6. **Data-recipient duties are the operator's.** When an operator receives vehicle data under
   the EU Data Act, the recipient's duties (purpose limitation, onward disclosure, security) are
   the operator's (16.1); the framework supports them with the existing access control, audit
   log, retention and deletion mechanisms (4.3, 16.2) and adds no Data Act-specific function.
   Euro 7's on-board monitoring is revisited when it places requirements on diagnostic tools.

## Consequences

- Generic OBD needs functional (broadcast) requests answered by several ECUs at L1/L2, and the
  standards that define the generic services and identifiers (SAE J1979, J1979-2, ISO 15031-5,
  ISO 15765-4, ISO 27145), none of which is held yet.
- The server needs an operator prompt that a server-side provider can raise, with correlation to
  the waiting job, a timeout, and behaviour when the operator's browser reconnects. Section 8.6
  covers only agent-originated requests.
- The responsibility table in 16.1 gains a row: certification, SERMI approval and authorisation,
  registration with makers' secure gateways, statutory retention periods and data-recipient
  duties belong to the operator or the framework user.
- A business that wants to use the system for statutory inspection cannot do so without the
  separate decision in item 1.
- Japan's current inspection scan tool requirements were not reviewed for this decision; they
  are needed only if item 1 is revisited.
