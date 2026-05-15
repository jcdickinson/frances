//! Bincode round-trip tests for protocol types that cross the daemon
//! ↔ TUI wire. Bincode's serde adapter is not self-describing and
//! quietly chokes on internally-tagged enums (`#[serde(tag = "...")]`)
//! with `Serde(AnyNotSupported)` on decode. The encode side succeeds,
//! so the bug only surfaces at runtime on the receiving end — easy to
//! miss without an explicit round-trip.

use frances_daemon::protocol::{
    ApprovalChoice, ApprovalId, ApprovalKind, ApprovalRequest, BlockId, BlockKind, StreamFrame,
};

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

#[test]
fn approval_kind_yes_no_round_trips() {
    let decoded: ApprovalKind = round_trip(&ApprovalKind::YesNo);
    assert_eq!(decoded, ApprovalKind::YesNo);
}

#[test]
fn approval_request_round_trips() {
    let req = ApprovalRequest {
        id: ApprovalId(42),
        prompt: "run echo?".to_owned(),
        kind: ApprovalKind::YesNo,
    };
    let decoded: ApprovalRequest = round_trip(&req);
    assert_eq!(decoded.id, req.id);
    assert_eq!(decoded.prompt, req.prompt);
    assert_eq!(decoded.kind, req.kind);
}

#[test]
fn approval_choice_round_trips_all_variants() {
    let yes = ApprovalChoice::Yes {
        details: Some("only this once".to_owned()),
    };
    let no = ApprovalChoice::No { details: None };
    let chat = ApprovalChoice::Chat {
        content: "do this differently".to_owned(),
    };
    assert_eq!(round_trip(&yes), yes);
    assert_eq!(round_trip(&no), no);
    assert_eq!(round_trip(&chat), chat);
}

#[test]
fn block_kind_text_round_trips_with_sender() {
    let with = StreamFrame::BlockStart {
        id: BlockId(1),
        kind: BlockKind::Text {
            sender: Some("you".to_owned()),
        },
    };
    let bytes = bincode::serde::encode_to_vec(&with, bincode::config::standard()).unwrap();
    let (decoded, _): (StreamFrame, _) =
        bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
    match decoded {
        StreamFrame::BlockStart {
            kind: BlockKind::Text { sender: Some(s) },
            ..
        } => assert_eq!(s, "you"),
        other => panic!("expected Text {{ sender: Some(\"you\") }}, got {other:?}"),
    }

    let without = StreamFrame::BlockStart {
        id: BlockId(2),
        kind: BlockKind::Text { sender: None },
    };
    let bytes = bincode::serde::encode_to_vec(&without, bincode::config::standard()).unwrap();
    let (decoded, _): (StreamFrame, _) =
        bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
    match decoded {
        StreamFrame::BlockStart {
            kind: BlockKind::Text { sender: None },
            ..
        } => {}
        other => panic!("expected Text {{ sender: None }}, got {other:?}"),
    }
}

#[test]
fn stream_frame_approval_round_trips() {
    let frame = StreamFrame::Approval(ApprovalRequest {
        id: ApprovalId(7),
        prompt: "wire test".to_owned(),
        kind: ApprovalKind::YesNo,
    });
    let bytes = bincode::serde::encode_to_vec(&frame, bincode::config::standard())
        .expect("encode StreamFrame::Approval");
    let (decoded, _): (StreamFrame, _) =
        bincode::serde::decode_from_slice(&bytes, bincode::config::standard())
            .expect("decode StreamFrame::Approval");
    match decoded {
        StreamFrame::Approval(req) => {
            assert_eq!(req.id, ApprovalId(7));
            assert_eq!(req.prompt, "wire test");
            assert_eq!(req.kind, ApprovalKind::YesNo);
        }
        other => panic!("expected Approval, got {other:?}"),
    }
}
