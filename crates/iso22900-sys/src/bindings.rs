#![allow(unsafe_code)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(unsafe_op_in_unsafe_fn)]
#![allow(unused)]
// The bindgen-generated bindings below are FFI declarations mirroring the
// vendor C API; they cannot carry `# Safety` docs or reduce their argument
// count without diverging from the generated output, so these lints are
// suppressed for the included file rather than the (off-limits) generated
// source.
#![allow(clippy::missing_safety_doc)]
#![allow(clippy::too_many_arguments)]
include!(concat!("bindings/", env!("BINDINGS_RS_FILENAME")));
