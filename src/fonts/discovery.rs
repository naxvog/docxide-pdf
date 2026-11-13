use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, OnceLock, RwLock};
use crate::clock::Instant;
use std::{env, fs};

use memmap2::Mmap;
use ttf_parser::Face;

use super::cache::{CachedFace, CachedFile, FontCache, dir_mtime, load_cache, save_cache};

/// (lowercase family name, bold, italic) -> (file path, face index within TTC)
type FontLookup = HashMap<(String, bool, bool), (PathBuf, u32)>;

static FONT_INDEX: OnceLock<FontLookup> = OnceLock::new();
/// Lowercase families that only Mac Roman–named macOS system faces provide.
static MAC_ONLY_FAMILIES: OnceLock<HashSet<String>> = OnceLock::new();

/// Return all localized family names for a font face (deduplicated by lowercase),
/// and whether they came only from Mac Roman records: macOS system faces such as
/// Helvetica, Times, Courier, Optima and Apple's Symbol carry no Unicode family
/// name. Those rank below every other face (see `scan_font_dirs`).
fn font_family_names(face: &Face) -> (Vec<String>, bool) {
    let mut seen = HashSet::new();
    let mut names = Vec::new();
    let family = || {
        face.names()
            .into_iter()
            .filter(|n| n.name_id == ttf_parser::name_id::FAMILY)
    };
    for s in family()
        .filter(|n| n.is_unicode())
        .filter_map(|n| n.to_string())
    {
        if seen.insert(s.to_lowercase()) {
            names.push(s);
        }
    }
    if !names.is_empty() {
        return (names, false);
    }
    // ponytail: ASCII-only Mac Roman decode; every such family name seen is ASCII.
    for n in family().filter(|n| {
        n.platform_id == ttf_parser::PlatformId::Macintosh
            && n.encoding_id == 0
            && n.name.is_ascii()
    }) {
        let s = String::from_utf8_lossy(n.name).into_owned();
        if seen.insert(s.to_lowercase()) {
            names.push(s);
        }
    }
    (names, true)
}

/// Returns `(names, bold, italic, mac_roman_only, off_weight)` — one entry with
/// ALL localized family names.
fn read_font_style(data: &[u8], face_index: u32) -> Option<(Vec<String>, bool, bool, bool, bool)> {
    let face = Face::parse(data, face_index).ok()?;
    let (names, mac_roman_only) = font_family_names(&face);
    if names.is_empty() {
        return None;
    }
    // Thin, Light, Black… faces that share the family name of the regular
    // and bold ones (Apple's HelveticaNeue.ttc: only its Thin face carries a
    // Unicode family name, so it outranked the Mac-Roman-only Regular).
    let off_weight = !(350..=750).contains(&face.weight().to_number());
    Some((
        names,
        face.is_bold(),
        face.is_italic(),
        mac_roman_only,
        off_weight,
    ))
}

fn font_directories() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    // The scan is a LIFO stack and the first-scanned face wins, so later entries
    // here outrank earlier ones. Platform-specific system font directories first.
    #[cfg(target_os = "macos")]
    {
        dirs.extend([
            "/Library/Fonts".into(),
            "/System/Library/Fonts".into(),
            "/System/Library/Fonts/Supplemental".into(),
        ]);
        if let Ok(home) = env::var("HOME") {
            dirs.push(PathBuf::from(&home).join("Library/Fonts"));
        }
    }

    #[cfg(target_os = "linux")]
    {
        dirs.extend(["/usr/share/fonts".into(), "/usr/local/share/fonts".into()]);
        if let Ok(home) = env::var("HOME") {
            dirs.push(PathBuf::from(home).join(".local/share/fonts"));
        }
    }

    #[cfg(target_os = "windows")]
    {
        if let Ok(windir) = env::var("WINDIR") {
            dirs.push(PathBuf::from(windir).join("Fonts"));
        } else {
            dirs.push("C:\\Windows\\Fonts".into());
        }
    }

    // DOCXIDE_FONTS pushed last = HIGHEST priority. A directory supplied explicitly
    // pins the font build regardless of what the host has installed (#105); the
    // system directories still fill in the families and styles it lacks.
    if let Ok(val) = env::var("DOCXIDE_FONTS") {
        let sep = if cfg!(windows) { ';' } else { ':' };
        dirs.extend(custom_font_dirs(&val, sep));
    }

    dirs
}

/// `DOCXIDE_FONTS` entries in stack order: the first directory listed is pushed
/// last, so the scan pops it first and it wins over the ones after it (like PATH).
fn custom_font_dirs(val: &str, sep: char) -> Vec<PathBuf> {
    val.rsplit(sep)
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(PathBuf::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_listed_custom_dir_is_popped_first() {
        let dirs = custom_font_dirs(" a : :b:c ", ':');
        assert_eq!(dirs, [PathBuf::from("c"), "b".into(), "a".into()]);
    }
}

fn font_ext(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

fn is_font_file(path: &Path) -> bool {
    matches!(font_ext(path).as_deref(), Some("ttf" | "otf" | "ttc"))
}

fn scan_font_dirs() -> FontLookup {
    let t0 = Instant::now();
    let mut index = FontLookup::new();
    // Mac-Roman-only faces fill in only the families nothing else provides, so
    // Apple's Symbol never shadows the vendored Microsoft Symbol (bullets).
    let mut mac_roman_index = FontLookup::new();
    // Off-weight faces fill in only the styles no regular-weight face provides.
    let mut off_weight_index = FontLookup::new();
    let dirs = font_directories();

    let no_cache = env::var("DOCXIDE_NO_FONT_CACHE").is_ok();

    let cache = if no_cache {
        FontCache::default()
    } else {
        load_cache()
    };
    let mut new_cache = FontCache::default();
    let mut files_scanned: u32 = 0;
    let mut dirs_cached: u32 = 0;
    let mut dirs_scanned: u32 = 0;
    let mut visited_dirs = HashSet::new();

    let mut stack: Vec<PathBuf> = dirs;
    while let Some(dir) = stack.pop() {
        if !visited_dirs.insert(dir.clone()) {
            continue;
        }

        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };

        let mut subdirs = Vec::new();
        let mut font_files = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            // readdir's file type spares a stat per font file; only symlinks need one.
            let is_dir = match entry.file_type() {
                Ok(t) if t.is_symlink() => path.is_dir(),
                Ok(t) => t.is_dir(),
                Err(_) => false,
            };
            if is_dir {
                subdirs.push(path);
            } else if is_font_file(&path) {
                font_files.push(path);
            }
        }
        stack.extend(subdirs);

        if font_files.is_empty() {
            continue;
        }

        let current_mtime = dir_mtime(&dir);

        if let Some(&cached_mtime) = cache.dir_mtimes.get(&dir)
            && cached_mtime == current_mtime
        {
            dirs_cached += 1;
            new_cache.dir_mtimes.insert(dir.clone(), current_mtime);
            for file_path in &font_files {
                if let Some(cached_file) = cache.files.get(file_path) {
                    for face in &cached_file.faces {
                        let tier = if face.off_weight {
                            &mut off_weight_index
                        } else if face.mac_roman_only {
                            &mut mac_roman_index
                        } else {
                            &mut index
                        };
                        tier.entry((face.family.to_lowercase(), face.bold, face.italic))
                            .or_insert((file_path.clone(), face.face_index));
                    }
                    new_cache
                        .files
                        .insert(file_path.clone(), cached_file.clone());
                }
            }
            continue;
        }

        // Directory changed — scan all font files in it
        dirs_scanned += 1;
        new_cache.dir_mtimes.insert(dir, current_mtime);
        for file_path in font_files {
            files_scanned += 1;
            let Ok(file) = fs::File::open(&file_path) else {
                continue;
            };
            let Ok(data) = (unsafe { Mmap::map(&file) }) else {
                continue;
            };
            let face_count = ttf_parser::fonts_in_collection(&data).unwrap_or(1);
            let mut faces = Vec::new();
            for face_idx in 0..face_count {
                if let Some((families, bold, italic, mac_roman_only, off_weight)) =
                    read_font_style(&data, face_idx)
                {
                    for family in families {
                        let tier = if off_weight {
                            &mut off_weight_index
                        } else if mac_roman_only {
                            &mut mac_roman_index
                        } else {
                            &mut index
                        };
                        tier.entry((family.to_lowercase(), bold, italic))
                            .or_insert((file_path.clone(), face_idx));
                        faces.push(CachedFace {
                            family,
                            bold,
                            italic,
                            face_index: face_idx,
                            mac_roman_only,
                            off_weight,
                        });
                    }
                }
            }
            new_cache.files.insert(file_path, CachedFile { faces });
        }
    }

    let mut mac_only = HashSet::new();
    for (key, value) in mac_roman_index {
        if let std::collections::hash_map::Entry::Vacant(slot) = index.entry(key) {
            mac_only.insert(slot.key().0.clone());
            slot.insert(value);
        }
    }
    for (key, value) in off_weight_index {
        index.entry(key).or_insert(value);
    }
    let _ = MAC_ONLY_FAMILIES.set(mac_only);

    if !no_cache {
        save_cache(&new_cache);
    }

    log::info!(
        "Font scan: {:.1}ms, {} dirs cached / {} scanned, {} files parsed → {} entries",
        t0.elapsed().as_secs_f64() * 1000.0,
        dirs_cached,
        dirs_scanned,
        files_scanned,
        index.len(),
    );

    index
}

// ---------------------------------------------------------------------------
// In-memory fonts and font requests (for hosts without a font directory, such
// as a browser): the host registers font bytes, and every lookup that finds
// nothing is recorded so the host can supply exactly the faces a document asks
// for and convert again.

#[derive(Default)]
struct MemoryFonts {
    index: FontLookup,
    data: HashMap<PathBuf, Arc<[u8]>>,
    /// Index keys a Thin/Light/Black… face holds: a regular-weight face of the
    /// same family and style replaces it (as in the directory scan).
    off_weight_keys: HashSet<(String, bool, bool)>,
}

static MEMORY_FONTS: LazyLock<RwLock<MemoryFonts>> = LazyLock::new(Default::default);

/// One font lookup that missed: the family names tried, in the order the
/// resolver tried them, and the style asked for. A face for the first name
/// the host can supply is the one the resolver would have used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FontRequest {
    pub names: Vec<String>,
    pub bold: bool,
    pub italic: bool,
}

static CURRENT_REQUEST: Mutex<Option<FontRequest>> = Mutex::new(None);
static FONT_REQUESTS: Mutex<Vec<FontRequest>> = Mutex::new(Vec::new());

/// Register a font file (TrueType/OpenType, or each face of a collection) for
/// every later conversion. Registered faces win over the system's. Returns the
/// number of faces registered.
pub fn register_font(data: Vec<u8>) -> usize {
    let data: Arc<[u8]> = data.into();
    let mut memory = MEMORY_FONTS.write().unwrap_or_else(|e| e.into_inner());
    let path = PathBuf::from(format!("memory:{}", memory.data.len()));
    let mut faces = 0;
    for face_idx in 0..ttf_parser::fonts_in_collection(&data).unwrap_or(1) {
        let Some((families, bold, italic, _, off_weight)) = read_font_style(&data, face_idx) else {
            continue;
        };
        for family in families {
            let key = (family.to_lowercase(), bold, italic);
            let replace = !memory.index.contains_key(&key)
                || (!off_weight && memory.off_weight_keys.contains(&key));
            if replace {
                memory.index.insert(key.clone(), (path.clone(), face_idx));
                if off_weight {
                    memory.off_weight_keys.insert(key);
                } else {
                    memory.off_weight_keys.remove(&key);
                }
            }
        }
        faces += 1;
    }
    if faces > 0 {
        memory.data.insert(path, data);
    }
    faces
}

/// Forget every registered font.
pub fn clear_fonts() {
    *MEMORY_FONTS.write().unwrap_or_else(|e| e.into_inner()) = MemoryFonts::default();
}

/// The lookups that missed since the last call (each once), in order.
pub fn take_font_requests() -> Vec<FontRequest> {
    std::mem::take(&mut *FONT_REQUESTS.lock().unwrap_or_else(|e| e.into_inner()))
}

/// Group the misses of one font resolution (a run's name and its fallbacks).
pub(crate) fn begin_font_request(bold: bool, italic: bool) {
    *CURRENT_REQUEST.lock().unwrap_or_else(|e| e.into_inner()) =
        Some(FontRequest { names: Vec::new(), bold, italic });
}

pub(crate) fn end_font_request() {
    if let Some(request) = CURRENT_REQUEST.lock().unwrap_or_else(|e| e.into_inner()).take() {
        record_request(request);
    }
}

fn record_request(request: FontRequest) {
    if request.names.is_empty() {
        return;
    }
    let mut requests = FONT_REQUESTS.lock().unwrap_or_else(|e| e.into_inner());
    if !requests.contains(&request) {
        requests.push(request);
    }
}

fn note_miss(name: &str, bold: bool, italic: bool) {
    let mut current = CURRENT_REQUEST.lock().unwrap_or_else(|e| e.into_inner());
    match current.as_mut() {
        Some(request) => {
            if !request.names.iter().any(|n| n.eq_ignore_ascii_case(name)) {
                request.names.push(name.to_owned());
            }
        }
        None => {
            drop(current);
            record_request(FontRequest { names: vec![name.to_owned()], bold, italic });
        }
    }
}

/// The bytes of a font file found by `find_font_file`: a registered font or a
/// file on disk.
pub(crate) fn read_font(path: &Path) -> Option<Vec<u8>> {
    if let Some(data) = MEMORY_FONTS.read().unwrap_or_else(|e| e.into_inner()).data.get(path) {
        return Some(data.to_vec());
    }
    fs::read(path).ok()
}

fn memory_lookup(key: &(String, bool, bool)) -> Option<(PathBuf, u32)> {
    MEMORY_FONTS.read().unwrap_or_else(|e| e.into_inner()).index.get(key).cloned()
}

static SYSTEM_FONTS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
static NO_SYSTEM_FONTS: LazyLock<FontLookup> = LazyLock::new(FontLookup::new);

/// Whether lookups also see the system's font directories (the default).
/// Off, only registered fonts resolve, as on a host without a file system.
pub fn use_system_fonts(enabled: bool) {
    SYSTEM_FONTS.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

fn get_font_index() -> &'static FontLookup {
    if !SYSTEM_FONTS.load(std::sync::atomic::Ordering::Relaxed) {
        return &NO_SYSTEM_FONTS;
    }
    FONT_INDEX.get_or_init(scan_font_dirs)
}

/// True if only a macOS system face provides this family (Helvetica, Optima…),
/// which Windows Word, including Mac Word's online PDF export, never has.
pub(super) fn is_mac_only_family(name: &str) -> bool {
    get_font_index();
    MAC_ONLY_FAMILIES
        .get()
        .is_some_and(|s| s.contains(&name.to_lowercase()))
}

/// Parse one face of a font file (memory-mapped, so only the tables touched are read).
pub(super) fn probe_face<T>(path: &Path, face_index: u32, f: impl FnOnce(&Face) -> T) -> Option<T> {
    let memory = MEMORY_FONTS.read().unwrap_or_else(|e| e.into_inner()).data.get(path).cloned();
    if let Some(data) = memory {
        return Face::parse(&data, face_index).ok().map(|face| f(&face));
    }
    let file = fs::File::open(path).ok()?;
    let data = unsafe { Mmap::map(&file) }.ok()?;
    Face::parse(&data, face_index).ok().map(|face| f(&face))
}

/// Look up a font file by family name and style using the OS/2 table metadata index.
/// Falls back to the regular variant if the requested bold/italic is not available.
/// Returns `(path, face_index, face_bold, face_italic)`.
pub(super) fn find_font_file(
    font_name: &str,
    bold: bool,
    italic: bool,
) -> Option<(PathBuf, u32, bool, bool)> {
    let index = get_font_index();
    let key = font_name.to_lowercase();
    let lookup = |b: bool, i: bool| {
        let key = (key.clone(), b, i);
        memory_lookup(&key).or_else(|| index.get(&key).cloned())
    };
    if let Some((path, face_index)) = lookup(bold, italic) {
        return Some((path, face_index, bold, italic));
    }
    // A plain regular request that misses falls through to a generic family
    // fallback rather than borrowing this family's bold/italic face, which would
    // render unexpectedly heavy/slanted.
    if !bold && !italic {
        note_miss(font_name, bold, italic);
        return None;
    }
    // A styled variant was requested but the exact (bold, italic) face is absent.
    // Relax the style axes and accept any same-family face: keeping the family's
    // real glyphs beats dropping to a different family, and synthetic bold/oblique
    // covers the style gap (we report exact_match=false). Some families ship only a
    // single styled face — e.g. Vivaldi is an italic-flagged script with no upright
    // or bold variant, so "Vivaldi bold" only resolves once we try (regular,italic).
    for (b, i) in [(bold, !italic), (!bold, italic), (!bold, !italic)] {
        if let Some((path, face_index)) = lookup(b, i) {
            return Some((path, face_index, b, i));
        }
    }
    note_miss(font_name, bold, italic);
    None
}
