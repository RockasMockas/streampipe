//! Loss repair between viewer and host.
//!
//! A viewer that finds a hole in the RTP sequence asks the host to resend it
//! (`TAG_NACK`), and the host answers from a short history of what it
//! forwarded. These check the framing of that request and the history.

use bytes::Bytes;
use streampipe::rtp::{
    decode_nack, encode_nack, History, NACK_AUDIO, NACK_MAX, NACK_VIDEO, TAG_NACK,
};

#[test]
fn nack_round_trips() {
    let seqs = vec![1, 2, 65535, 0, 300];
    let buf = encode_nack(NACK_VIDEO, &seqs);
    assert_eq!(buf[0], TAG_NACK);
    assert_eq!(buf.len(), 2 + 2 * seqs.len());
    assert_eq!(decode_nack(&buf), Some((NACK_VIDEO, seqs)));
    assert_eq!(
        decode_nack(&encode_nack(NACK_AUDIO, &[9])),
        Some((NACK_AUDIO, vec![9]))
    );
}

#[test]
fn nack_is_capped_to_fit_one_datagram() {
    let seqs: Vec<u16> = (0..(NACK_MAX as u16 + 50)).collect();
    let (_, decoded) = decode_nack(&encode_nack(NACK_VIDEO, &seqs)).expect("a nack");
    assert_eq!(decoded, seqs[..NACK_MAX].to_vec());
}

#[test]
fn malformed_nacks_are_rejected() {
    assert_eq!(decode_nack(&[]), None);
    assert_eq!(decode_nack(&[TAG_NACK]), None, "no media byte");
    assert_eq!(decode_nack(&[TAG_NACK, NACK_VIDEO, 1]), None, "half a seq");
    assert_eq!(
        decode_nack(&[TAG_NACK + 1, NACK_VIDEO, 0, 1]),
        None,
        "wrong tag"
    );
}

#[test]
fn history_answers_only_for_what_it_still_has() {
    let mut history = History::new(8);
    history.insert(3, Bytes::from_static(b"three"));
    assert_eq!(history.get(3).as_deref(), Some(&b"three"[..]));
    assert_eq!(history.get(4), None, "never forwarded");
    // 11 shares the slot of 3: the older packet is gone, and a NACK for 3 must
    // not be answered with 11.
    history.insert(11, Bytes::from_static(b"eleven"));
    assert_eq!(history.get(3), None);
    assert_eq!(history.get(11).as_deref(), Some(&b"eleven"[..]));
}

#[test]
fn history_survives_the_sequence_wrap() {
    let mut history = History::new(4096);
    history.insert(65535, Bytes::from_static(b"last"));
    history.insert(0, Bytes::from_static(b"first"));
    assert_eq!(history.get(65535).as_deref(), Some(&b"last"[..]));
    assert_eq!(history.get(0).as_deref(), Some(&b"first"[..]));
    history.clear();
    assert_eq!(history.get(0), None);
}
