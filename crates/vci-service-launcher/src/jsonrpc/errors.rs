use serde_json::{Value, json};

pub mod codes {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const SERVER_ERROR: i64 = -32000;
}

#[derive(Debug, Clone)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
}

impl JsonRpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn parse_error() -> Self {
        Self::new(codes::PARSE_ERROR, "Parse error")
    }

    pub fn invalid_request() -> Self {
        Self::new(codes::INVALID_REQUEST, "Invalid Request")
    }

    pub fn method_not_found() -> Self {
        Self::new(codes::METHOD_NOT_FOUND, "Method not found")
    }

    pub fn server_error(details: impl Into<String>) -> Self {
        Self::new(codes::SERVER_ERROR, details.into())
    }

    pub fn invalid_params(details: impl Into<String>) -> Self {
        Self::new(codes::INVALID_PARAMS, details.into())
    }

    pub fn to_response(&self, id: Option<&Value>) -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": self.code,
                "message": self.message,
            }
        })
    }
}

pub fn success_response(id: Option<&Value>, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}
