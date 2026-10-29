use super::*;
use alloc::string::ToString;
use ed25519_compact::{KeyPair, Seed};

fn reset() {
    policy::reset_for_test();
    av::reset_for_test();
    reset_for_test();
}

fn keypair(n: u8) -> KeyPair {
    KeyPair::from_seed(Seed::new([n; 32]))
}

fn trust(name: &str, kp: &KeyPair) {
    assert!(add_key(name, *kp.pk));
}

/// A `.sig` for `body`, signed by `kp` the way `hunter-feed-sign` does it.
fn sign(kp: &KeyPair, key: &str, feed: &str, serial: u64, body: &[u8]) -> String {
    let sum: Digest = Sha256::digest(body).into();
    let header = format!(
        "{}\nfeed {}\nserial {}\nkey {}\nsha256 {}\n",
        MAGIC,
        feed,
        serial,
        key,
        hex(&sum)
    );
    let sig = kp.sk.sign(header.as_bytes(), None);
    format!("{}ed25519 {}\n", header, hex(&sig[..]))
}

/// A feed line whose signature is the SHA-256 of `content`.
fn hash_line(content: &str, name: &str) -> String {
    let d: Digest = Sha256::digest(content.as_bytes()).into();
    format!("hash:{}:{}\n", hex(&d), name)
}

/// What the antivirus says about a file holding exactly `content`.
fn detects(content: &str) -> Option<String> {
    let bytes = content.as_bytes();
    av::scan(bytes.len(), &mut |off, buf| {
        buf.copy_from_slice(&bytes[off..off + buf.len()]);
        true
    })
    .unwrap()
}

/// Produced on a Linux host by `tools/hunterctl/hunter-feed-sign` (OpenSSL
/// 3.0), so the kernel's verifier is checked against an independent signer,
/// not only against itself.
#[test]
fn a_feed_signed_by_openssl_verifies() {
    let _g = crate::test_globals::lock();
    reset();
    let pk =
        integrity::parse_hex("e894b9801690dba2c365a9fa5f12e2b3a6fb788668ad34e48eb50265a8d41970")
            .unwrap();
    assert!(add_key("test-pub", pk));
    let body = b"# test feed\nbytes:0badc0de0badc0de:Test.Feed.OpenSSL\n";
    let sig = "hunter-feed-signature 1\n\
               feed Test.OpenSSL\n\
               serial 2026102918\n\
               key test-pub\n\
               sha256 dd3e608fd711b9208ba628f956f5f0d831236d7672eefea0f889512ec24ad096\n\
               ed25519 ced2469a3f2a230e8c54e87ca4baad393bee284842bc9912bc09e4bb59ce85319a6da1b5d218c8c8c3b5516c48446cc648866bdcf326ac19a6c68b9e2a7c6308\n";
    let r = install("Test.OpenSSL", body, Some(sig), Install::Update, 0).unwrap();
    assert_eq!(r.total, 1);
    assert!(render().contains("feed Test.OpenSSL serial=2026102918 key=test-pub signatures=1"));
    // One byte of the body changed, and nothing of it is believed.
    reset();
    assert!(add_key("test-pub", pk));
    let evil = b"# test feed\nbytes:0badc0de0badc0df:Test.Feed.OpenSSL\n";
    let e = install("Test.OpenSSL", evil, Some(sig), Install::Update, 0).unwrap_err();
    assert!(e.contains("SHA-256"), "{}", e);
    assert_eq!(av::stats().1, 0);
}

#[test]
fn a_forged_misnamed_or_unknown_signature_installs_nothing() {
    let _g = crate::test_globals::lock();
    reset();
    let good = keypair(1);
    let other = keypair(2);
    trust("pub", &good);
    let body = hash_line("evil", "Evil");

    let by_stranger = sign(&other, "pub", "Main", 1, body.as_bytes());
    let e = install(
        "Main",
        body.as_bytes(),
        Some(&by_stranger),
        Install::Update,
        0,
    )
    .unwrap_err();
    assert!(e.contains("does not verify"), "{}", e);

    let unknown_key = sign(&other, "stranger", "Main", 1, body.as_bytes());
    let e = install(
        "Main",
        body.as_bytes(),
        Some(&unknown_key),
        Install::Update,
        0,
    )
    .unwrap_err();
    assert!(e.contains("not trusted"), "{}", e);

    let for_another_feed = sign(&good, "pub", "Tiny", 1, body.as_bytes());
    let e = install(
        "Main",
        body.as_bytes(),
        Some(&for_another_feed),
        Install::Update,
        0,
    )
    .unwrap_err();
    assert!(e.contains("for feed 'Tiny'"), "{}", e);

    // A header line edited after signing (a newer serial, to beat the
    // replay check) breaks the signature.
    let bumped = sign(&good, "pub", "Main", 1, body.as_bytes()).replace("serial 1\n", "serial 9\n");
    let e = install("Main", body.as_bytes(), Some(&bumped), Install::Update, 0).unwrap_err();
    assert!(e.contains("does not verify"), "{}", e);

    assert_eq!(detects("evil"), None);
    assert!(render().contains("REJECTED"));
    assert!(!render().contains("\nfeed Main "));
}

#[test]
fn an_older_serial_is_a_replay_unless_the_operator_rolls_back() {
    let _g = crate::test_globals::lock();
    reset();
    let kp = keypair(3);
    trust("pub", &kp);
    let v1 = hash_line("old-bad", "Old");
    let v2 = format!(
        "{}{}",
        hash_line("old-bad", "Old"),
        hash_line("new-bad", "New")
    );
    let s1 = sign(&kp, "pub", "Main", 100, v1.as_bytes());
    let s2 = sign(&kp, "pub", "Main", 200, v2.as_bytes());

    install("Main", v1.as_bytes(), Some(&s1), Install::Update, 0).unwrap();
    let r = install("Main", v2.as_bytes(), Some(&s2), Install::Update, 0).unwrap();
    assert_eq!((r.total, r.added, r.dropped), (2, 1, 0));
    assert_eq!(detects("new-bad").as_deref(), Some("New"));

    // The same feed again is not an error, and changes nothing.
    assert!(
        install("Main", v2.as_bytes(), Some(&s2), Install::Update, 0)
            .unwrap()
            .unchanged
    );

    // An attacker replaying the genuine, older v1 to take "New" back out.
    let e = install("Main", v1.as_bytes(), Some(&s1), Install::Update, 0).unwrap_err();
    assert!(e.contains("older"), "{}", e);
    assert_eq!(detects("new-bad").as_deref(), Some("New"));

    // Two different feeds under one serial: one of them is not the publisher's.
    let other = hash_line("other", "Other");
    let s2b = sign(&kp, "pub", "Main", 200, other.as_bytes());
    assert!(install("Main", other.as_bytes(), Some(&s2b), Install::Update, 0).is_err());

    // The operator, on purpose: v2 blocks something it should not.
    let r = install("Main", v1.as_bytes(), Some(&s1), Install::Rollback, 0).unwrap();
    assert_eq!((r.total, r.added, r.dropped, r.kept), (1, 0, 1, 0));
    assert_eq!(detects("new-bad"), None, "rolled back live, no reboot");
    assert_eq!(detects("old-bad").as_deref(), Some("Old"));
    assert!(render().contains("serial=100"));
}

#[test]
fn a_signed_feed_is_never_replaced_by_an_unsigned_one() {
    let _g = crate::test_globals::lock();
    reset();
    let kp = keypair(4);
    trust("pub", &kp);
    let body = hash_line("bad", "Bad");
    install(
        "Main",
        body.as_bytes(),
        Some(&sign(&kp, "pub", "Main", 1, body.as_bytes())),
        Install::Update,
        0,
    )
    .unwrap();
    let stripped = hash_line("harmless", "Harmless");
    for how in [Install::Update, Install::Rollback] {
        let e = install("Main", stripped.as_bytes(), None, how, 0).unwrap_err();
        assert!(e.contains("unsigned"), "{}", e);
    }
    assert_eq!(detects("bad").as_deref(), Some("Bad"));
}

#[test]
fn unsigned_feeds_are_taken_until_signatures_are_required() {
    let _g = crate::test_globals::lock();
    reset();
    let body = hash_line("bazaar-sample", "MalwareBazaar.Recent");
    install(
        "MalwareBazaar.Recent",
        body.as_bytes(),
        None,
        Install::Update,
        0,
    )
    .unwrap();
    assert!(render().contains("feed MalwareBazaar.Recent serial=- key=-"));

    set_require_signed(true);
    let next = hash_line("bazaar-sample-2", "MalwareBazaar.Recent");
    let e = install(
        "MalwareBazaar.Recent",
        next.as_bytes(),
        None,
        Install::Update,
        0,
    )
    .unwrap_err();
    assert!(e.contains("require-signed-feeds"), "{}", e);

    policy::seal_tighten_only();
    set_require_signed(false);
    assert!(require_signed(), "off is a relaxation, refused once sealed");
}

#[test]
fn a_replacement_drops_only_what_nothing_else_holds() {
    let _g = crate::test_globals::lock();
    reset();
    // "shared" is in both feeds, "mine" is also local; "only-a" is A's alone.
    add_local(
        av::parse_signature(&hash_line("mine", "Local"))
            .unwrap()
            .unwrap(),
    );
    let a1 = format!(
        "{}{}{}",
        hash_line("shared", "A"),
        hash_line("mine", "A"),
        hash_line("only-a", "A")
    );
    install("A", a1.as_bytes(), None, Install::Update, 0).unwrap();
    install(
        "B",
        hash_line("shared", "B").as_bytes(),
        None,
        Install::Update,
        0,
    )
    .unwrap();

    let a2 = hash_line("fresh", "A");
    let r = install("A", a2.as_bytes(), None, Install::Update, 0).unwrap();
    assert_eq!((r.added, r.dropped, r.kept), (1, 3, 2));
    assert!(detects("shared").is_some(), "feed B still holds it");
    assert!(detects("mine").is_some(), "the operator's own signature");
    assert_eq!(detects("only-a"), None);
    assert!(detects("fresh").is_some());
}

#[test]
fn once_sealed_no_signature_leaves() {
    let _g = crate::test_globals::lock();
    reset();
    install(
        "A",
        hash_line("v1", "A").as_bytes(),
        None,
        Install::Update,
        0,
    )
    .unwrap();
    policy::seal_tighten_only();
    let r = install(
        "A",
        hash_line("v2", "A").as_bytes(),
        None,
        Install::Update,
        0,
    )
    .unwrap();
    assert_eq!((r.added, r.dropped, r.kept), (1, 1, 1));
    assert!(detects("v1").is_some(), "kept until reboot");
    assert!(detects("v2").is_some());
    // And who is trusted is frozen too.
    assert!(!add_key("late", *keypair(5).pk));
}

#[test]
fn one_bad_line_rejects_the_whole_feed() {
    let _g = crate::test_globals::lock();
    reset();
    let body = format!("{}bytes:zz:Broken\n", hash_line("good", "Good"));
    let e = install("A", body.as_bytes(), None, Install::Update, 0).unwrap_err();
    assert!(e.starts_with("line 2:"), "{}", e);
    assert_eq!(detects("good"), None);
    assert!(install("A", b"# only comments\n\n", None, Install::Update, 0).is_err());
    assert!(install("../etc", b"x", None, Install::Update, 0).is_err());
}

#[test]
fn a_key_name_keeps_its_first_key() {
    let _g = crate::test_globals::lock();
    reset();
    trust("pub", &keypair(6));
    assert!(add_key("pub", *keypair(6).pk), "the same key again is fine");
    assert!(!add_key("pub", *keypair(7).pk));
    assert!(render().contains(&format!("key pub {}", hex(&*keypair(6).pk))));
}

#[test]
fn the_signature_file_is_read_one_way_only() {
    let kp = keypair(8);
    let good = sign(&kp, "pub", "Main", 7, b"x");
    let parsed = parse_feed_signature(&good).unwrap();
    assert_eq!(
        (parsed.feed.as_str(), parsed.serial, parsed.key.as_str()),
        ("Main", 7, "pub")
    );
    assert_eq!(parsed.signed_len, good.rfind("ed25519 ").unwrap());
    for bad in [
        good.replace("hunter-feed-signature 1", "hunter-feed-signature 2"),
        good.replace("serial 7", "serial +7"),
        good.replace("serial 7", "serial 7 "),
        good.replace("feed Main", "feed ../Main"),
        good.replacen("feed Main\nserial 7\n", "serial 7\nfeed Main\n", 1),
        good.trim_end().to_string(),
        format!("{}trust me\n", good),
        good.replace("\n", "\r\n"),
    ] {
        assert!(parse_feed_signature(&bad).is_err(), "accepted:\n{}", bad);
    }
    // Blank lines after the end are harmless (an editor's trailing newline).
    assert!(parse_feed_signature(&format!("{}\n", good)).is_ok());
}

#[test]
fn feed_names_are_plain_file_names() {
    for ok in ["MalwareBazaar.Recent", "eclipse-main", "a_b.1"] {
        assert!(valid_name(ok), "{}", ok);
    }
    for bad in ["", ".hidden", "x.sig", "a/b", "..", "a b", "ñ"] {
        assert!(!valid_name(bad), "{}", bad);
    }
}
