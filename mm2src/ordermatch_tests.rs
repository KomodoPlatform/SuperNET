use super::*;

#[test]
fn test_match_maker_order_and_taker_request() {
    let maker = MakerOrder {
        base: "BASE".into(),
        rel: "REL".into(),
        created_at: now_ms(),
        max_base_vol: 10.into(),
        min_base_vol: 0.into(),
        price: 1.into(),
        matches: HashMap::new(),
    };

    let request = TakerRequest {
        base: "BASE".into(),
        rel: "REL".into(),
        uuid: Uuid::new_v4(),
        method: "request".into(),
        dest_pub_key: H256Json::default(),
        sender_pubkey: H256Json::default(),
        base_amount: 10.into(),
        rel_amount: 20.into(),
        action: TakerAction::Buy,
    };

    let actual = match_order_and_request(&maker, &request);
    let expected = OrderMatchResult::Matched((10.into(), 10.into()));
    assert_eq!(expected, actual);

    let maker = MakerOrder {
        base: "BASE".into(),
        rel: "REL".into(),
        created_at: now_ms(),
        max_base_vol: 10.into(),
        min_base_vol: 0.into(),
        price: "0.5".parse().unwrap(),
        matches: HashMap::new(),
    };

    let request = TakerRequest {
        base: "BASE".into(),
        rel: "REL".into(),
        uuid: Uuid::new_v4(),
        method: "request".into(),
        dest_pub_key: H256Json::default(),
        sender_pubkey: H256Json::default(),
        base_amount: 10.into(),
        rel_amount: 20.into(),
        action: TakerAction::Buy,
    };

    let actual = match_order_and_request(&maker, &request);
    let expected = OrderMatchResult::Matched((10.into(), 5.into()));
    assert_eq!(expected, actual);

    let maker = MakerOrder {
        base: "BASE".into(),
        rel: "REL".into(),
        created_at: now_ms(),
        max_base_vol: 10.into(),
        min_base_vol: 0.into(),
        price: "0.5".parse().unwrap(),
        matches: HashMap::new(),
    };

    let request = TakerRequest {
        base: "BASE".into(),
        rel: "REL".into(),
        uuid: Uuid::new_v4(),
        method: "request".into(),
        dest_pub_key: H256Json::default(),
        sender_pubkey: H256Json::default(),
        base_amount: 10.into(),
        rel_amount: 2.into(),
        action: TakerAction::Buy,
    };

    let actual = match_order_and_request(&maker, &request);
    let expected = OrderMatchResult::NotMatched;
    assert_eq!(expected, actual);

    let maker = MakerOrder {
        base: "BASE".into(),
        rel: "REL".into(),
        created_at: now_ms(),
        max_base_vol: 10.into(),
        min_base_vol: 0.into(),
        price: "0.5".parse().unwrap(),
        matches: HashMap::new(),
    };

    let request = TakerRequest {
        base: "REL".into(),
        rel: "BASE".into(),
        uuid: Uuid::new_v4(),
        method: "request".into(),
        dest_pub_key: H256Json::default(),
        sender_pubkey: H256Json::default(),
        base_amount: 5.into(),
        rel_amount: 10.into(),
        action: TakerAction::Sell,
    };

    let actual = match_order_and_request(&maker, &request);
    let expected = OrderMatchResult::Matched((10.into(), 5.into()));
    assert_eq!(expected, actual);

    let maker = MakerOrder {
        base: "BASE".into(),
        rel: "REL".into(),
        created_at: now_ms(),
        max_base_vol: 20.into(),
        min_base_vol: 0.into(),
        price: "0.5".parse().unwrap(),
        matches: HashMap::new(),
    };

    let request = TakerRequest {
        base: "REL".into(),
        rel: "BASE".into(),
        uuid: Uuid::new_v4(),
        method: "request".into(),
        dest_pub_key: H256Json::default(),
        sender_pubkey: H256Json::default(),
        base_amount: 10.into(),
        rel_amount: 10.into(),
        action: TakerAction::Sell,
    };

    let actual = match_order_and_request(&maker, &request);
    let expected = OrderMatchResult::Matched((20.into(), 10.into()));
    assert_eq!(expected, actual);
}

#[test]
fn test_maker_order_available_amount() {
    let mut maker = MakerOrder {
        base: "BASE".into(),
        rel: "REL".into(),
        created_at: now_ms(),
        max_base_vol: 10.into(),
        min_base_vol: 0.into(),
        price: 1.into(),
        matches: HashMap::new(),
    };
    maker.matches.insert(Uuid::new_v4(), MakerMatch {
        request: TakerRequest {
            uuid: Uuid::new_v4(),
            base: "BASE".into(),
            rel: "REL".into(),
            base_amount: 5.into(),
            rel_amount: 5.into(),
            sender_pubkey: H256Json::default(),
            dest_pub_key: H256Json::default(),
            method: "request".into(),
            action: TakerAction::Buy,
        },
        reserved: MakerReserved {
            method: "reserved".into(),
            base: "BASE".into(),
            rel: "REL".into(),
            base_amount: 5.into(),
            rel_amount: 5.into(),
            sender_pubkey: H256Json::default(),
            dest_pub_key: H256Json::default(),
            maker_order_uuid: Uuid::new_v4(),
            taker_order_uuid: Uuid::new_v4(),
        },
        connect: None,
        connected: None,
        last_updated: now_ms(),
    });
    maker.matches.insert(Uuid::new_v4(), MakerMatch {
        request: TakerRequest {
            uuid: Uuid::new_v4(),
            base: "BASE".into(),
            rel: "REL".into(),
            base_amount: 1.into(),
            rel_amount: 1.into(),
            sender_pubkey: H256Json::default(),
            dest_pub_key: H256Json::default(),
            method: "request".into(),
            action: TakerAction::Buy,
        },
        reserved: MakerReserved {
            method: "reserved".into(),
            base: "BASE".into(),
            rel: "REL".into(),
            base_amount: 1.into(),
            rel_amount: 1.into(),
            sender_pubkey: H256Json::default(),
            dest_pub_key: H256Json::default(),
            maker_order_uuid: Uuid::new_v4(),
            taker_order_uuid: Uuid::new_v4(),
        },
        connect: None,
        connected: None,
        last_updated: now_ms(),
    });

    let expected: BigDecimal = 4.into();
    let actual = maker.available_amount();
    assert_eq!(expected, actual);
}
