#![allow(unsafe_code)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(unsafe_op_in_unsafe_fn)]
#![allow(unused)]
// The bindgen-generated bindings below are FFI declarations mirroring the
// vendor C API; they cannot carry `# Safety` docs without diverging from the
// generated output, so this lint is suppressed for the included file rather
// than the (off-limits) generated source.
#![allow(clippy::missing_safety_doc)]

include!(concat!("bindings/", env!("BINDINGS_RS_FILENAME")));
