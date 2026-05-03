// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;

use serde::Deserialize;
use vmm::replay::ReplayMode;
use vmm::rpc_interface::VmmAction;

use super::super::parsed_request::{ParsedRequest, RequestError};
use super::super::request::{Body, Method, StatusCode};

#[derive(Debug, Deserialize)]
struct ReplayModeBody {
    mode: ReplayMode,
}

#[derive(Debug, Deserialize)]
struct ReplayLogPathBody {
    path: PathBuf,
}

pub(crate) fn parse_put_replay(
    body: &Body,
    action: Option<&str>,
) -> Result<ParsedRequest, RequestError> {
    match action {
        Some("mode") => {
            let ReplayModeBody { mode } = serde_json::from_slice(body.raw())?;
            Ok(ParsedRequest::new_sync(VmmAction::SetReplayMode(mode)))
        }
        Some("save") => {
            let ReplayLogPathBody { path } = serde_json::from_slice(body.raw())?;
            Ok(ParsedRequest::new_sync(VmmAction::SaveReplayLog(path)))
        }
        Some("load") => {
            let ReplayLogPathBody { path } = serde_json::from_slice(body.raw())?;
            Ok(ParsedRequest::new_sync(VmmAction::LoadReplayLog(path)))
        }
        Some("reset") => Ok(ParsedRequest::new_sync(VmmAction::ResetReplayLog)),
        Some(other) => Err(RequestError::InvalidPathMethod(
            format!("/replay/{other}"),
            Method::Put,
        )),
        None => Err(RequestError::Generic(
            StatusCode::BadRequest,
            "Missing replay operation type.".to_string(),
        )),
    }
}

pub(crate) fn parse_get_replay(action: Option<&str>) -> Result<ParsedRequest, RequestError> {
    match action {
        Some("mode") => Ok(ParsedRequest::new_sync(VmmAction::GetReplayMode)),
        Some(other) => Err(RequestError::InvalidPathMethod(
            format!("/replay/{other}"),
            Method::Get,
        )),
        None => Err(RequestError::Generic(
            StatusCode::BadRequest,
            "Missing replay operation type.".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_server::parsed_request::tests::vmm_action_from_request;

    #[test]
    fn test_parse_put_replay_mode() {
        let body = r#"{"mode":"Record"}"#;
        assert_eq!(
            vmm_action_from_request(parse_put_replay(&Body::new(body), Some("mode")).unwrap()),
            VmmAction::SetReplayMode(ReplayMode::Record),
        );

        let body = r#"{"mode":"Replay"}"#;
        assert_eq!(
            vmm_action_from_request(parse_put_replay(&Body::new(body), Some("mode")).unwrap()),
            VmmAction::SetReplayMode(ReplayMode::Replay),
        );

        let body = r#"{"mode":"Off"}"#;
        assert_eq!(
            vmm_action_from_request(parse_put_replay(&Body::new(body), Some("mode")).unwrap()),
            VmmAction::SetReplayMode(ReplayMode::Off),
        );
    }

    #[test]
    fn test_parse_put_replay_save_load() {
        let body = r#"{"path":"/tmp/replay.detlog"}"#;
        assert_eq!(
            vmm_action_from_request(parse_put_replay(&Body::new(body), Some("save")).unwrap()),
            VmmAction::SaveReplayLog(PathBuf::from("/tmp/replay.detlog")),
        );
        assert_eq!(
            vmm_action_from_request(parse_put_replay(&Body::new(body), Some("load")).unwrap()),
            VmmAction::LoadReplayLog(PathBuf::from("/tmp/replay.detlog")),
        );
    }

    #[test]
    fn test_parse_put_replay_reset() {
        assert_eq!(
            vmm_action_from_request(parse_put_replay(&Body::new("{}"), Some("reset")).unwrap()),
            VmmAction::ResetReplayLog,
        );
    }

    #[test]
    fn test_parse_put_replay_rejects_bad_path() {
        parse_put_replay(&Body::new("{}"), None).unwrap_err();
        parse_put_replay(&Body::new("{}"), Some("bogus")).unwrap_err();
        parse_put_replay(&Body::new(r#"{"mode":"Invalid"}"#), Some("mode")).unwrap_err();
        parse_put_replay(&Body::new("{}"), Some("save")).unwrap_err();
    }

    #[test]
    fn test_parse_get_replay_mode() {
        assert_eq!(
            vmm_action_from_request(parse_get_replay(Some("mode")).unwrap()),
            VmmAction::GetReplayMode,
        );
        parse_get_replay(None).unwrap_err();
        parse_get_replay(Some("bogus")).unwrap_err();
    }
}
