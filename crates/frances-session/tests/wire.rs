//! Bincode round-trip tests for protocol types that cross the daemon
//! ↔ TUI wire. Bincode's serde adapter is not self-describing and
//! quietly chokes on internally-tagged enums (`#[serde(tag = "...")]`)
//! with `Serde(AnyNotSupported)` on decode. The encode side succeeds,
//! so the bug only surfaces at runtime on the receiving end — easy to
//! miss without an explicit round-trip.

use frances_session::protocol::{
    BlockId, BlockKind, PermissionId, PermissionRequest, PermissionResponseWire, StreamFrame,
};
use frances_models_llm::wire::ToolCall;

fn round_trip<T>(value: &T) -> T
where
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    let bytes = bincode::serde::encode_to_vec(value, bincode::config::standard())
        .expect("encode should succeed");
    let (decoded, _) = bincode::serde::decode_from_slice(&bytes, bincode::config::standard())
        .expect("decode should succeed");
    decoded
}

fn sample_tool_call() -> ToolCall {
    ToolCall {
        id: "call_1".to_owned(),
        name: "shell_run".to_owned(),
        arguments: serde_json::json!({ "cmd": "echo hi" }),
    }
}

#[test]
fn permission_request_round_trips_with_tool_call() {
    let req = PermissionRequest {
        id: PermissionId(42),
        prompt: "run echo?".to_owned(),
        tool_call: Some(sample_tool_call()),
    };
    let decoded: PermissionRequest = round_trip(&req);
    assert_eq!(decoded.id, req.id);
    assert_eq!(decoded.prompt, req.prompt);
    let call = decoded.tool_call.expect("tool_call survives wire");
    assert_eq!(call.id, "call_1");
    assert_eq!(call.name, "shell_run");
    assert_eq!(call.arguments, serde_json::json!({ "cmd": "echo hi" }));
}

#[test]
fn permission_request_round_trips_without_tool_call() {
    let req = PermissionRequest {
        id: PermissionId(7),
        prompt: "are you sure?".to_owned(),
        tool_call: None,
    };
    let decoded: PermissionRequest = round_trip(&req);
    assert_eq!(decoded.id, req.id);
    assert_eq!(decoded.prompt, req.prompt);
    assert!(decoded.tool_call.is_none());
}

#[test]
fn permission_response_wire_round_trips_all_variants() {
    let yes = PermissionResponseWire::Yes {
        details: Some("only this once".to_owned()),
    };
    let no = PermissionResponseWire::No { details: None };
    let redirect = PermissionResponseWire::RedirectToChat {
        content: "use a different directory please".to_owned(),
    };
    assert_eq!(round_trip(&yes), yes);
    assert_eq!(round_trip(&no), no);
    assert_eq!(round_trip(&redirect), redirect);
}

#[test]
fn block_kind_text_round_trips_with_sender() {
    let with = StreamFrame::BlockDelta {
        id: BlockId(1),
        kind: BlockKind::Text {
            sender: Some("you".into()),
        },
        text: Some("hello".to_owned()),
    };
    let bytes = bincode::serde::encode_to_vec(&with, bincode::config::standard()).unwrap();
    let (decoded, _): (StreamFrame, _) =
        bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
    match decoded {
        StreamFrame::BlockDelta {
            kind: BlockKind::Text { sender: Some(s) },
            text,
            ..
        } => {
            assert_eq!(&*s, "you");
            assert_eq!(text.as_deref(), Some("hello"));
        }
        other => panic!("expected Text {{ sender: Some(\"you\") }}, got {other:?}"),
    }

    let without = StreamFrame::BlockDelta {
        id: BlockId(2),
        kind: BlockKind::Text { sender: None },
        text: None,
    };
    let bytes = bincode::serde::encode_to_vec(&without, bincode::config::standard()).unwrap();
    let (decoded, _): (StreamFrame, _) =
        bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
    match decoded {
        StreamFrame::BlockDelta {
            kind: BlockKind::Text { sender: None },
            text: None,
            ..
        } => {}
        other => panic!("expected Text {{ sender: None }} with text=None, got {other:?}"),
    }
}

#[test]
fn stream_frame_permission_round_trips() {
    let frame = StreamFrame::Permission(PermissionRequest {
        id: PermissionId(7),
        prompt: "wire test".to_owned(),
        tool_call: Some(sample_tool_call()),
    });
    let bytes = bincode::serde::encode_to_vec(&frame, bincode::config::standard())
        .expect("encode StreamFrame::Permission");
    let (decoded, _): (StreamFrame, _) =
        bincode::serde::decode_from_slice(&bytes, bincode::config::standard())
            .expect("decode StreamFrame::Permission");
    match decoded {
        StreamFrame::Permission(req) => {
            assert_eq!(req.id, PermissionId(7));
            assert_eq!(req.prompt, "wire test");
            assert_eq!(req.tool_call.as_ref().expect("tool_call").name, "shell_run");
        }
        other => panic!("expected Permission, got {other:?}"),
    }
}
