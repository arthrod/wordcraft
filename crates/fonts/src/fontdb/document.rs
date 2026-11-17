//! Fonts one document embeds, for as long as that document is shown: added so they can be
//! withdrawn when another document takes its place, put back if that one fails to open, and
//! kept by content so a document opened again (or another embedding the same file) gets the
//! same faces instead of new ones. Used by the view-only core, which shows one document at a
//! time; the editor's documents keep [`FontDb::add_document_fonts`].

use std::collections::hash_map::RandomState;
use std::collections::{HashMap, HashSet};
use std::hash::BuildHasher;
use std::sync::{Arc, PoisonError};

use super::names::{MAX_FAMILY_NAME, MAX_FILE_FACES};
use super::{FontBytes, FontDb, FontFace, RunFallbackCache, enumerate_faces, make_face};

/// The faces one document added to the font list ([`FontDb::add_document_fonts_scoped`]).
#[derive(Clone, Default)]
pub struct DocumentFaces {
    pub(super) faces: Vec<Arc<FontFace>>,
    new_bytes: usize,
}

impl DocumentFaces {
    /// How many faces the document added.
    pub fn len(&self) -> usize {
        self.faces.len()
    }

    /// Whether it added none.
    pub fn is_empty(&self) -> bool {
        self.faces.is_empty()
    }

    /// The bytes of font files the database kept for the first time when these faces were added
    /// (a file it already kept, or one that added no face, counts nothing).
    pub fn new_bytes(&self) -> usize {
        self.new_bytes
    }
}

/// The font files documents embedded, kept for the life of the database so that a document
/// opened again reuses its faces ([`FontDb::add_document_fonts_scoped`]). Found by the name a
/// file was given, its length and a hash of its bytes; the bytes are compared only with a file
/// that matches all three. The hash has per-process random keys, so two kept files under one
/// key take a 64-bit collision no document can aim at: a file is hashed once and compared with
/// at most the one file it is.
///
/// It also counts, per face, the documents holding it ([`FontDb::release_document_fonts`]).
#[derive(Default)]
pub(super) struct DocumentFiles {
    hasher: RandomState,
    files: HashMap<FileKey, Vec<KeptFile>>,
    held: HashMap<u32, usize>,
}

/// What a kept font file is found by ([`DocumentFiles`]).
#[derive(PartialEq, Eq, Hash)]
struct FileKey {
    /// The name it was given to go by ([`given_name`]).
    name: Option<String>,
    len: usize,
    hash: u64,
}

/// A font file one document embedded, kept with the faces made for it so that a document
/// embedding it again gets the same faces ([`FontDb::add_document_fonts_scoped`]).
struct KeptFile {
    data: Arc<Vec<u8>>,
    faces: Vec<Arc<FontFace>>,
}

impl DocumentFiles {
    fn key(&self, data: &[u8], name: Option<String>) -> FileKey {
        FileKey { name, len: data.len(), hash: self.hasher.hash_one(data) }
    }

    /// The kept file `data` is, found under `key` (its own).
    fn find(&self, key: &FileKey, data: &[u8]) -> Option<&KeptFile> {
        self.files.get(key)?.iter().find(|k| k.data.as_slice() == data)
    }
}

/// The name a font file is given to go by, as [`FontDb::add_document_fonts`] takes it: trimmed,
/// at most [`MAX_FAMILY_NAME`] characters, none when empty.
fn given_name(name: &str) -> Option<String> {
    Some(name.trim().chars().take(MAX_FAMILY_NAME).collect::<String>()).filter(|n| !n.is_empty())
}

/// The faces of the font file `data`, going by `name` where they don't already answer to it
/// (as [`FontDb::add_document_fonts`] makes them).
fn file_faces(data: &Arc<Vec<u8>>, name: Option<&str>) -> Vec<Arc<FontFace>> {
    let mut faces = Vec::new();
    for f in enumerate_faces(data, MAX_FILE_FACES) {
        if let Some(mut face) = make_face(FontBytes::Owned(data.clone()), f.index, f.family, f.style, f.legacy, f.coords) {
            face.local_names = f.local;
            if let Some(n) = name.filter(|n| !face.named(n) && !face.local_names.iter().any(|l| l.name.eq_ignore_ascii_case(n))) {
                face.family = n.to_string();
                face.legacy_family = n.to_string();
            }
            faces.push(Arc::new(face));
        }
    }
    faces
}

/// Puts a document's withdrawn faces back when dropped ([`FontDb::installing_over`]), so they
/// come back on every way out, unwinding included.
struct Reinstate<'a> {
    db: &'a FontDb,
    faces: &'a DocumentFaces,
}

impl Drop for Reinstate<'_> {
    fn drop(&mut self) {
        self.db.reinstate_document_fonts(self.faces);
    }
}

/// A face's family and style, as the font list tells faces apart.
fn key(f: &FontFace) -> (String, String) {
    (f.family.to_ascii_lowercase(), f.style.to_ascii_lowercase())
}

impl FontDb {
    /// [`FontDb::add_document_fonts`], answering with the faces the document added, so they can
    /// be withdrawn when another document takes its place ([`FontDb::withdraw_document_fonts`]):
    /// a font one document embeds then never answers for a later one that only names the family,
    /// and the later one's own subset of a family is not skipped as already present.
    ///
    /// A file the database already keeps from an earlier document (the same bytes under the same
    /// name) brings back the faces made for it then: opening a document again, or another that
    /// embeds the same font, makes no new faces and keeps no new bytes. A file whose faces were
    /// all already present is not kept and costs nothing. A new file is kept while its bytes fit
    /// what is left of `room`, in the order given; one past it is left out (not even read).
    /// [`DocumentFaces::new_bytes`] counts the bytes this call newly kept, which stay for the
    /// life of the process: `room` is the caller's bound on them.
    pub fn add_document_fonts_scoped(&self, files: Vec<(Vec<u8>, String)>, room: usize) -> DocumentFaces {
        let mut kept = self.document_files.lock().unwrap_or_else(PoisonError::into_inner);
        let mut known = Vec::new();
        let mut fresh = Vec::new();
        for (data, name) in files {
            let key = kept.key(&data, given_name(&name));
            match kept.find(&key, &data) {
                Some(k) => known.extend(k.faces.iter().cloned()),
                None if data.len() > room => {}
                None => {
                    let data = Arc::new(data);
                    let faces = file_faces(&data, key.name.as_deref());
                    fresh.push((key, KeptFile { data, faces }));
                }
            }
        }
        let mut list = self.faces.write().unwrap_or_else(PoisonError::into_inner);
        let mut present: HashSet<_> = list.iter().map(|g| key(g)).collect();
        // A kept face in the list already is another open document's of this file: this one
        // holds it too.
        let listed: HashSet<u32> = list.iter().map(|g| g.id).collect();
        let (mut shared, mut added) = (Vec::new(), Vec::new());
        for f in known {
            if listed.contains(&f.id) {
                shared.push(f);
            } else if present.insert(key(&f)) {
                added.push(f);
            }
        }
        let (mut room, mut new_bytes) = (room, 0usize);
        for (file_key, file) in fresh {
            let mut own = HashSet::new();
            let from_file: Vec<_> = file.faces.iter().filter(|f| !present.contains(&key(f)) && own.insert(key(f))).cloned().collect();
            // A file adding no face keeps nothing and costs nothing; one adding faces is kept
            // while it fits.
            let Some(left) = room.checked_sub(file.data.len()).filter(|_| !from_file.is_empty()) else {
                continue;
            };
            room = left;
            new_bytes = new_bytes.saturating_add(file.data.len());
            present.extend(own);
            added.extend(from_file);
            kept.files.entry(file_key).or_default().push(file);
        }
        list.extend(added.iter().cloned());
        drop(list);
        let changed = !added.is_empty();
        added.extend(shared);
        for f in &added {
            let held = kept.held.entry(f.id).or_insert(0);
            *held = held.saturating_add(1);
        }
        drop(kept);
        if changed {
            self.forget_choices();
        }
        DocumentFaces { faces: added, new_bytes }
    }

    /// The document that added `faces` ([`FontDb::add_document_fonts_scoped`]) is closed: each
    /// face no other open document holds leaves the font list, as in
    /// [`FontDb::withdraw_document_fonts`]. Call it once per addition; a face it is called for
    /// more often than added leaves the list (again), nothing worse.
    pub fn release_document_fonts(&self, faces: &DocumentFaces) {
        let mut kept = self.document_files.lock().unwrap_or_else(PoisonError::into_inner);
        let mut free = HashSet::new();
        for f in &faces.faces {
            match kept.held.get_mut(&f.id) {
                Some(held) if *held > 1 => *held -= 1,
                _ => {
                    kept.held.remove(&f.id);
                    free.insert(f.id);
                }
            }
        }
        let mut list = self.faces.write().unwrap_or_else(PoisonError::into_inner);
        let before = list.len();
        list.retain(|f| !free.contains(&f.id));
        let removed = list.len() != before;
        drop(list);
        drop(kept);
        if removed {
            self.forget_choices();
        }
    }

    /// Take a document's faces out of the font list: lookups no longer find them, and fallback
    /// choices made while they were there are forgotten. Faces not in the list are skipped.
    /// Layouts made with them keep drawing with them.
    pub fn withdraw_document_fonts(&self, faces: &DocumentFaces) {
        let ids: HashSet<u32> = faces.faces.iter().map(|f| f.id).collect();
        let mut list = self.faces.write().unwrap_or_else(PoisonError::into_inner);
        let before = list.len();
        list.retain(|f| !ids.contains(&f.id));
        let removed = list.len() != before;
        drop(list);
        if removed {
            self.forget_choices();
        }
    }

    /// Put a document's withdrawn faces back (the document after it failed to open), each
    /// unless the list already has that family and style. The same faces, so their ids and
    /// cached outlines are still good.
    pub fn reinstate_document_fonts(&self, faces: &DocumentFaces) {
        let mut list = self.faces.write().unwrap_or_else(PoisonError::into_inner);
        let mut present: HashSet<_> = list.iter().map(|g| key(g)).collect();
        let before = list.len();
        for f in &faces.faces {
            if present.insert(key(f)) {
                list.push(f.clone());
            }
        }
        let added = list.len() != before;
        drop(list);
        if added {
            self.forget_choices();
        }
    }

    /// Run `install` (a host's font, a user font, a pack) with the document's faces out of the
    /// list, then put them back where it left their family and style free: an installed face
    /// takes the place of the document's own instead of being skipped as already present, so
    /// the family stays when the document goes, and the document's other faces still leave with
    /// it. They are put back however `install` ends, an error or an unwind. The steps are not
    /// one atomic change: a lookup on another thread in between sees the document's faces gone
    /// (the view-only core runs on one thread).
    pub fn installing_over<R>(&self, faces: &DocumentFaces, install: impl FnOnce() -> R) -> R {
        self.withdraw_document_fonts(faces);
        let _back = Reinstate { db: self, faces };
        install()
    }

    /// Faces left the list or came back: every fallback choice may be another now. The run cache
    /// is kept by face count, which a withdrawal and a later addition can bring back to the same
    /// number, so it is emptied too.
    fn forget_choices(&self) {
        *self.run_fallbacks.write().unwrap_or_else(PoisonError::into_inner) = RunFallbackCache::default();
        self.fonts_changed();
    }
}

#[cfg(test)]
mod tests;
