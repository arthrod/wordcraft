//! Locale tags resolve to an offered language before its catalog loads (the web picker).
//! Kept out of `tests.rs` so the two files change independently.

#[test]
fn offered_code_from_tag_matches_locale_variants_before_catalog_load() {
    let _lock = super::RUNTIME_CATALOG_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(super::offered_code_from_tag("fr-FR"), Some("fr"));
    assert_eq!(super::offered_code_from_tag("zh-TW"), Some("zh-hant"));
    assert_eq!(super::offered_code_from_tag("zh-Hans-HK"), Some("zh-hans"), "an explicit script beats the region's usual one");
    assert_eq!(super::offered_code_from_tag("zh-Hant-CN"), Some("zh-hant"));
    assert_eq!(super::offered_code_from_tag("zh_CN.UTF-8"), Some("zh-hans"));
    assert_eq!(super::offered_code_from_tag("sr-Latn-RS"), Some("sr-latn"));
    assert_eq!(super::offered_code_from_tag("sr-RS"), Some("sr"));
    assert_eq!(super::offered_code_from_tag("sr_RS.UTF-8@latin"), Some("sr-latn"), "the POSIX @latin modifier asks for the Latin script");
    assert_eq!(super::offered_code_from_tag("sr_RS@latin"), Some("sr-latn"));
    assert_eq!(super::offered_code_from_tag("sr_RS.UTF-8"), Some("sr"));
    assert_eq!(super::offered_code_from_tag("pt-PT"), None);
    assert_eq!(super::offered_code_from_tag("pt-BR"), Some("pt-br"));
    assert_eq!(super::target_locale_code("FR"), "fr");
    assert_eq!(super::target_locale_code("sr_RS.UTF-8@latin"), "sr-latn");
    assert_eq!(super::target_locale_code("auto"), "en");
}
