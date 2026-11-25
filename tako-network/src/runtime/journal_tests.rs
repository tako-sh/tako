#[test]
fn journal_verification_never_creates_a_journal() {
    let fixture = Fixture::new();
    assert!(verify_journal(&fixture.journal()).is_err());
    std::fs::write(fixture.journal(), b"").unwrap();
    assert!(verify_journal(&fixture.journal()).is_err());
    assert_eq!(std::fs::metadata(fixture.journal()).unwrap().len(), 0);
    std::fs::write(fixture.journal(), b"not a journal").unwrap();
    assert!(verify_journal(&fixture.journal()).is_err());
    std::fs::remove_file(fixture.journal()).unwrap();
}
