use super::*;
fn call(d: &mut Decoder, time: u64, relay: u32, session: u32, id: u32) -> Request {
    d.receive(
        time,
        format!("SBX_RPC1 {relay} {session} {{\"version\":1,\"id\":{id}}}").as_bytes(),
    )
    .unwrap()
}
fn close(d: &mut Decoder, time: u64, session: u32) {
    assert_eq!(
        d.close(time, format!("SBX_CLOSE1 {session}").as_bytes()),
        Ok(NonZeroU32::new(session).unwrap())
    );
}
fn complete(d: &mut Decoder, r: &Request) {
    assert_eq!(
        d.reply(r.correlation, b"{}"),
        Ok(Some(
            format!(
                "SBX_REPLY1 {} {} {{}}\n",
                r.correlation.relay, r.correlation.session
            )
            .into_bytes()
        ))
    );
    d.delivered(r.correlation).unwrap();
}
#[test]
fn operation_errors_remain_correlated_and_pending_survives_snapshot() {
    let mut d = Decoder::default();
    let r = d
        .receive(9, b"SBX_RPC1 1 1 {\"version\":1,\"id\":1,\"op\":false}")
        .unwrap();
    assert_eq!(r.payload, b"{\"version\":1,\"id\":1,\"op\":false}");
    d.reply(r.correlation, b"{\"error\":\"invalid op\"}")
        .unwrap();
    let mut d: Decoder = serde_json::from_slice(&serde_json::to_vec(&d).unwrap()).unwrap();
    d.delivered(r.correlation).unwrap();
    call(&mut d, 9, 2, 1, 2);
}
#[test]
fn same_time_order_reconnect_and_public_exhaustion() {
    let mut d = Decoder::default();
    let a = call(&mut d, 5, 1, 1, 1);
    complete(&mut d, &a);
    let b = call(&mut d, 5, 2, 1, 2);
    complete(&mut d, &b);
    close(&mut d, 5, 1);
    let c = call(&mut d, 5, 3, 2, 1);
    complete(&mut d, &c);
    assert_eq!(
        d.receive(5, b"SBX_RPC1 4 1 {\"version\":1,\"id\":3}"),
        Err(Error::StaleSession)
    );
    let exhausted = Session {
        id: NonZeroU32::new(1).unwrap(),
        public: NonZeroU32::MAX,
        pending: None,
        reply_prepared: false,
    };
    let mut d = Decoder {
        session: 1,
        sessions: vec![exhausted],
        ..Decoder::default()
    };
    assert_eq!(
        d.receive(0, b"SBX_RPC1 1 1 {\"version\":1,\"id\":1}"),
        Err(Error::PublicId)
    );
    let mut d = Decoder {
        session: 1,
        sessions: vec![exhausted],
        ..Decoder::default()
    };
    call(&mut d, 0, 1, 2, 1);
}
#[test]
fn sessions_interleave_each_sequential_and_bounded() {
    let mut d = Decoder::default();
    let a = call(&mut d, 1, 1, 1, 1);
    let b = call(&mut d, 1, 2, 2, 1);
    // Replies complete in any order across sessions.
    complete(&mut d, &b);
    let b2 = call(&mut d, 2, 3, 2, 2);
    assert_eq!(
        d.clone()
            .receive(2, b"SBX_RPC1 4 1 {\"version\":1,\"id\":2}"),
        Err(Error::Outstanding)
    );
    let mut d: Decoder = serde_json::from_slice(&serde_json::to_vec(&d).unwrap()).unwrap();
    complete(&mut d, &a);
    complete(&mut d, &b2);
    let mut d = Decoder::default();
    for session in 1..=MAX_SESSIONS as u32 {
        call(&mut d, 0, session, session, 1);
    }
    let extra = MAX_SESSIONS as u32 + 1;
    assert_eq!(
        d.receive(
            0,
            format!("SBX_RPC1 {extra} {extra} {{\"version\":1,\"id\":1}}").as_bytes()
        ),
        Err(Error::Sessions)
    );
    let mut d = Decoder::default();
    for session in 1..=MAX_SESSIONS as u32 {
        call(&mut d, 0, session, session, 1);
    }
    close(&mut d, 0, 3);
    call(&mut d, 0, extra, extra, 1);
    // A silent or unknown session cannot close.
    assert_eq!(d.close(0, b"SBX_CLOSE1 3"), Err(Error::StaleSession));
}
#[test]
fn all_reply_identity_fields_and_undelivered_gate() {
    for field in 0..3 {
        let mut d = Decoder::default();
        let r = call(&mut d, 0, 1, 1, 1);
        let mut key = r.correlation;
        match field {
            0 => key.session = NonZeroU32::new(2).unwrap(),
            1 => key.relay = NonZeroU32::new(2).unwrap(),
            _ => key.request = NonZeroU32::new(2).unwrap(),
        }
        assert_eq!(d.reply(key, b"{}"), Err(Error::StaleReply));
    }
    let mut d = Decoder::default();
    let a = call(&mut d, 0, 1, 1, 1);
    d.reply(a.correlation, b"{}").unwrap();
    assert_eq!(
        d.receive(0, b"SBX_RPC1 2 1 {\"version\":1,\"id\":2}"),
        Err(Error::Outstanding)
    );
}
#[test]
fn malformed_identity_envelope_exhaustion_and_time() {
    for payload in [
        "{}",
        "{\"version\":1,\"id\":0}",
        "{\"version\":2,\"id\":1}",
        "{\"version\":1,\"id\":1,\"id\":1}",
    ] {
        let mut d = Decoder::default();
        assert_eq!(
            d.receive(0, format!("SBX_RPC1 1 1 {payload}").as_bytes()),
            Err(Error::Identity)
        );
    }
    let mut d = Decoder {
        relay: u32::MAX,
        ..Decoder::default()
    };
    assert_eq!(
        d.receive(0, b"SBX_RPC1 1 1 {\"version\":1,\"id\":1}"),
        Err(Error::RelayId)
    );
    let mut d = Decoder::default();
    assert_eq!(d.receive(0, b"SBX_RPC1 1 1 {}\n"), Err(Error::Frame));
    let mut d = Decoder::default();
    let a = call(&mut d, 2, 1, 1, 1);
    complete(&mut d, &a);
    assert_eq!(
        d.receive(1, b"SBX_RPC1 2 1 {\"version\":1,\"id\":2}"),
        Err(Error::Time)
    );
}
#[test]
fn closed_session_preserves_accepted_work_and_discards_its_destination() {
    for prepared in [false, true] {
        let mut decoder = Decoder::default();
        let old = call(&mut decoder, 1, 1, 1, 1);
        if prepared {
            assert!(decoder.reply(old.correlation, b"{}").unwrap().is_some());
        }
        close(&mut decoder, 2, 1);
        let new = call(&mut decoder, 2, 2, 3, 1);
        // The caller's accepted old operation remains available; closing its
        // session neither rewrites nor cancels its payload.
        assert_eq!(old.payload, b"{\"version\":1,\"id\":1}");
        let mut decoder: Decoder =
            serde_json::from_slice(&serde_json::to_vec(&decoder).unwrap()).unwrap();
        assert_eq!(decoder.reply(old.correlation, b"{}"), Ok(None));
        assert_eq!(decoder.delivered(old.correlation), Ok(false));
        assert_eq!(
            decoder.resolve(super::super::SimulationControlId {
                session: old.correlation.session,
                request: old.correlation.request,
            }),
            Ok(None)
        );
        complete(&mut decoder, &new);
        let later = call(&mut decoder, 3, 3, 3, 2);
        complete(&mut decoder, &later);
    }
}

#[test]
fn replacement_validation_remains_fail_closed() {
    for (frame, expected) in [
        (
            b"SBX_RPC1 2 2 {\"version\":1,\"id\":2}".as_slice(),
            Error::PublicId,
        ),
        (
            b"SBX_RPC1 3 2 {\"version\":1,\"id\":1}".as_slice(),
            Error::RelayId,
        ),
        (b"SBX_RPC1 2 2 {}".as_slice(), Error::Identity),
    ] {
        let mut decoder = Decoder::default();
        call(&mut decoder, 1, 1, 1, 1);
        assert_eq!(decoder.receive(2, frame), Err(expected));
    }
    let mut decoder = Decoder::default();
    let old = call(&mut decoder, 1, 1, 1, 1);
    call(&mut decoder, 2, 2, 2, 1);
    assert_eq!(
        decoder.reply(old.correlation, b"invalid JSON"),
        Err(Error::Frame)
    );
}
