//! The slim browser build: formats and paste kinds a build leaves out say so, and the page's
//! DOCX bridge (`doc.exportDocx`, `doc.loadDocx`) round-trips. Kept out of `tests.rs` so the
//! two files change independently.
use super::*;

/// A known format this build leaves out is reported unavailable, not opened as plain text;
/// plain text and unknown text extensions still open.
#[test]
fn formats_left_out_of_the_build_are_unavailable() {
    for name in ["page.html", "page.htm", "archive.mht", "paper.tex", "letter.rtf", "report.odt"] {
        let bytes: &[u8] = if name == "letter.rtf" { b"{\\rtf1 Hi}" } else { b"<html><body>Hi</body></html>" };
        let r = crate::io::open_bytes(name, bytes);
        if wordcraft_formats::UNAVAILABLE_EXTENSIONS.contains(&name.rsplit('.').next().unwrap_or("")) {
            assert!(r.unwrap_err().contains("not included in this build"), "{name}");
        }
    }
    assert!(crate::io::open_bytes("notes.txt", b"Hello").is_ok());
    assert!(crate::io::open_bytes("notes.log", b"Hello").is_ok());
}

#[test]
fn doc_docx_base64_commands_roundtrip_and_reject_invalid() {
    let mut src = Session::new(wordcraft_doc::Document::from_text("DOCX bridge roundtrip test"));
    let exported = src.run("doc.exportDocx", &json!({})).unwrap();
    let b64 = exported["docxBase64"].as_str().unwrap();
    assert!(!b64.is_empty());
    assert!(exported["bytes"].as_u64().unwrap() > 0);

    let mut dst = s();
    let loaded = dst.run("doc.loadDocx", &json!({"docxBase64": b64})).unwrap();
    assert_eq!(loaded["ok"], true);
    assert!(text(&dst).contains("DOCX bridge roundtrip test"));

    assert!(dst.run("doc.loadDocx", &json!({})).is_err());
    assert!(dst.run("doc.loadDocx", &json!({"docxBase64": "%%%invalid%%%"})).is_err());
    let junk = crate::cmd::insert::base64_encode(b"not a valid zip or docx");
    assert!(dst.run("doc.loadDocx", &json!({"docxBase64": junk})).is_err());

    let mut used = s();
    run(&mut used, "text.insert", json!({"text": "Unsaved work"}));
    let original = used.document_id();
    run(&mut used, "doc.loadDocx", json!({"docxBase64": b64}));
    assert_ne!(used.document_id(), original, "a used document stays open beside the loaded one");
    assert_eq!(used.documents().len(), 2);
    assert!(text(&used).contains("DOCX bridge roundtrip test"));
    used.activate(original).unwrap();
    assert_eq!(text(&used), "Unsaved work");
    assert!(used.dirty);

    // Password-protected documents stay encrypted on `doc.exportDocx` and reopen with `password`.
    let mut encrypted = Session::new(wordcraft_doc::Document::from_text("Secret DOCX bridge"));
    encrypted.run("file.encrypt", &json!({"password": "pw"})).unwrap();
    let enc_b64 = encrypted.run("doc.exportDocx", &json!({})).unwrap()["docxBase64"].as_str().unwrap().to_string();
    let mut reopened = s();
    assert!(reopened.run("doc.loadDocx", &json!({"docxBase64": enc_b64})).is_err());
    reopened.run("doc.loadDocx", &json!({"docxBase64": enc_b64, "password": "pw"})).unwrap();
    assert!(text(&reopened).contains("Secret DOCX bridge"));
    assert_eq!(reopened.password.as_ref().map(crate::io::Password::as_str), Some("pw"));
}

#[test]
fn paste_special_format_availability_matches_build_support() {
    let mut s = s();
    let r = run(&mut s, "edit.pasteSpecial", json!({"html": "<b>HTML</b>", "rtf": r"{\rtf1 RTF}", "text": "plain"}));
    let ids: Vec<&str> = r["formats"].as_array().unwrap().iter().filter_map(|f| f["id"].as_str()).collect();
    assert!(ids.contains(&"text"));
    assert_eq!(ids.contains(&"html"), cfg!(feature = "extra-formats"), "{ids:?}");
    assert_eq!(ids.contains(&"rtf"), cfg!(feature = "extra-formats"), "{ids:?}");

    #[cfg(not(feature = "extra-formats"))]
    for params in [json!({"as": "html", "html": "<b>HTML</b>"}), json!({"as": "rtf", "rtf": r"{\rtf1 RTF}"})] {
        let e = s.run("edit.pasteSpecial", &params).unwrap_err();
        assert!(matches!(&e, crate::CmdError::Failed(m) if m == "HTML/RTF paste is not included in this build"), "{e}");
    }
}
