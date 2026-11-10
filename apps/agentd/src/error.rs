use std::io;

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use slop_process::ProcessError;

pub struct ApiError(pub StatusCode, pub String);

impl From<ProcessError> for ApiError {
    fn from(error: ProcessError) -> Self {
        match error {
            ProcessError::ShuttingDown => Self(
                StatusCode::SERVICE_UNAVAILABLE,
                "daemon is shutting down".into(),
            ),
            ProcessError::Capacity => Self(
                StatusCode::SERVICE_UNAVAILABLE,
                "process capacity reached".into(),
            ),
            ProcessError::Invalid(message) => Self(StatusCode::UNPROCESSABLE_ENTITY, message),
            ProcessError::Spawn(error) => Self(StatusCode::UNPROCESSABLE_ENTITY, error.to_string()),
            ProcessError::InvalidCursor => Self(
                StatusCode::BAD_REQUEST,
                "cursor is ahead of process output".into(),
            ),
            ProcessError::StdinClosed { offset } => Self(
                StatusCode::CONFLICT,
                format!("stdin is closed; {offset} bytes were written"),
            ),
            ProcessError::StdinOffsetAhead { offset } => Self(
                StatusCode::CONFLICT,
                format!("stdin offset is ahead; next offset is {offset}"),
            ),
            ProcessError::Gap { first_seq } => Self(
                StatusCode::GONE,
                format!("output expired; first available cursor is {first_seq}"),
            ),
            ProcessError::TerminalInputStaysOpen => Self(
                StatusCode::CONFLICT,
                "a terminal's input stays open until the process exits".into(),
            ),
            ProcessError::NotATerminal => Self(
                StatusCode::CONFLICT,
                "process was not started with a terminal".into(),
            ),
            ProcessError::Finished => Self(StatusCode::CONFLICT, "process has finished".into()),
            ProcessError::Resize(error) => {
                Self(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
            }
        }
    }
}

impl From<io::Error> for ApiError {
    fn from(error: io::Error) -> Self {
        let status = match error.kind() {
            io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
            io::ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
            io::ErrorKind::IsADirectory
            | io::ErrorKind::NotADirectory
            | io::ErrorKind::DirectoryNotEmpty
            | io::ErrorKind::AlreadyExists => StatusCode::CONFLICT,
            io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded => {
                StatusCode::INSUFFICIENT_STORAGE
            }
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self(status, error.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, self.1).into_response()
    }
}
