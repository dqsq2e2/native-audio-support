use std::ffi::{CStr, CString};
use std::os::raw::c_int;
use std::path::{Path, PathBuf};
use std::process::Command;
use serde_json::{json, Value};
use std::sync::{Mutex, RwLock};
use regex::Regex;
use encoding_rs::GBK;
use lofty::probe::Probe;
use lofty::prelude::*;
use lofty::picture::MimeType;
use lofty::config::{WriteOptions, ParseOptions, ParsingMode};

// Global state to manage FFmpeg path or other resources
// Since this is a dylib, we can use static mutable state with synchronization
lazy_static::lazy_static! {
    static ref FFMPEG_PATH: Mutex<Option<String>> = Mutex::new(None);
    static ref PLUGIN_DIR: RwLock<Option<PathBuf>> = RwLock::new(None);
}

// We need lazy_static dependency, let's add it to Cargo.toml or use std::sync::OnceLock (Rust 1.70+)
// Assuming Rust 1.70+ is available based on project config (1.70 in Cargo.toml of backend)

// --- FFI Interface ---

#[no_mangle]
pub unsafe extern "C" fn plugin_invoke(
    method: *const u8,
    params: *const u8,
    result_ptr: *mut *mut u8,
) -> c_int {
    let method_str = match CStr::from_ptr(method as *const std::os::raw::c_char).to_str() {
        Ok(s) => s,
        Err(_) => return -1,
    };

    let params_str = match CStr::from_ptr(params as *const std::os::raw::c_char).to_str() {
        Ok(s) => s,
        Err(_) => return -1,
    };

    // Parse params immediately to handle initialize
    let params_json: Value = match serde_json::from_str(params_str) {
        Ok(v) => v,
        Err(_) => return -1,
    };

    let result = match method_str {
        "initialize" => initialize(params_json),
        "detect" => detect(params_json),
        "extract_metadata" => extract_metadata(params_json),
        "write_metadata" => write_metadata(params_json),
        "get_stream_url" => get_stream_url(params_json),
        "configure" => configure(params_json),
        "get_decryption_plan" => get_decryption_plan(params_json),
        "get_metadata_read_size" => get_metadata_read_size(params_json),
        "garbage_collect" => Ok(json!({})),
        _ => Err(format!("Unknown method: {}", method_str)),
    };

    match result {
        Ok(val) => {
            let json = serde_json::to_string(&val).unwrap_or_default();
            let c_string = match CString::new(json) {
                Ok(s) => s,
                Err(_) => return -1,
            };
            *result_ptr = c_string.into_raw() as *mut u8;
            0 // Success
        }
        Err(e) => {
            let error_json = json!({ "error": e }).to_string();
             let c_string = match CString::new(error_json) {
                Ok(s) => s,
                Err(_) => return -1,
            };
            *result_ptr = c_string.into_raw() as *mut u8;
            -1 // Failure
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn plugin_free(ptr: *mut u8) {
    if !ptr.is_null() {
        let _ = CString::from_raw(ptr as *mut std::os::raw::c_char);
    }
}

// --- Implementation ---

fn initialize(params: Value) -> Result<Value, String> {
    if let Some(plugin_path_str) = params.get("plugin_path").and_then(|v| v.as_str()) {
        let path = PathBuf::from(plugin_path_str);
        if let Ok(mut lock) = PLUGIN_DIR.write() {
            *lock = Some(path);
        }
    }
    Ok(json!({ "status": "initialized" }))
}

fn get_ffmpeg_path() -> String {
    let ffmpeg = FFMPEG_PATH.lock().unwrap();
    if let Some(path) = &*ffmpeg {
        return path.clone();
    }
    
    // Check PLUGIN_DIR first
    let exe_ext = std::env::consts::EXE_EXTENSION;
    if let Ok(lock) = PLUGIN_DIR.read() {
        if let Some(plugin_path) = lock.as_ref() {
            // Check if ffmpeg is inside this plugin directory (unlikely but possible)
            let mut bin_path = plugin_path.join("ffmpeg");
            if !exe_ext.is_empty() { bin_path.set_extension(exe_ext); }
            if bin_path.exists() { return bin_path.to_string_lossy().to_string(); }

            // Check sibling ffmpeg-utils
            if let Some(plugins_dir) = plugin_path.parent() {
                if let Ok(entries) = std::fs::read_dir(plugins_dir) {
                    for entry in entries.filter_map(|e| e.ok()) {
                        let path = entry.path();
                        if path.is_dir() {
                            let dir_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                            if dir_name.starts_with("FFmpeg Provider") || dir_name.starts_with("ffmpeg-utils") {
                                let mut bin_path = path.join("ffmpeg");
                                if !exe_ext.is_empty() { bin_path.set_extension(exe_ext); }
                                if bin_path.exists() { return bin_path.to_string_lossy().to_string(); }

                                let mut bin_sub_path = path.join("bin").join("ffmpeg");
                                if !exe_ext.is_empty() { bin_sub_path.set_extension(exe_ext); }
                                if bin_sub_path.exists() { return bin_sub_path.to_string_lossy().to_string(); }
                            }
                        }
                    }
                }
            }
        }
    }
    
    let mut search_paths = Vec::new();

    // 1. Check relative to Executable (Production usually)
    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(root) = current_exe.parent() {
            search_paths.push(root.to_path_buf());
        }
    }

    // 2. Check relative to CWD (Development usually)
    if let Ok(cwd) = std::env::current_dir() {
        search_paths.push(cwd);
    }

    let exe_ext = std::env::consts::EXE_EXTENSION;

    for root in search_paths {
        // Try to find "plugins" directory
        let possible_plugin_dirs = vec![
            root.join("plugins"),
            root.join("backend").join("plugins"),
            root.join("ting-reader").join("backend").join("plugins"),
            // Case: running from target/debug/deps, so plugins is up 3 levels then plugins
            root.join("..").join("..").join("plugins"), 
        ];

        for plugins_dir in possible_plugin_dirs {
            if plugins_dir.exists() {
                // Look for any folder starting with "FFmpeg Provider" or "ffmpeg-utils"
                if let Ok(entries) = std::fs::read_dir(&plugins_dir) {
                    for entry in entries.filter_map(|e| e.ok()) {
                        let path = entry.path();
                        if path.is_dir() {
                            let dir_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                            if dir_name.starts_with("FFmpeg Provider") || dir_name.starts_with("ffmpeg-utils") {
                                // Found candidate directory, check for binary
                                let mut bin_path = path.join("ffmpeg");
                                if !exe_ext.is_empty() { bin_path.set_extension(exe_ext); }
                                if bin_path.exists() { return bin_path.to_string_lossy().to_string(); }

                                let mut bin_sub_path = path.join("bin").join("ffmpeg");
                                if !exe_ext.is_empty() { bin_sub_path.set_extension(exe_ext); }
                                if bin_sub_path.exists() { return bin_sub_path.to_string_lossy().to_string(); }
                            }
                        }
                    }
                }
            }
        }
    }
    
    // Check ./ffmpeg.exe (CWD root fallback)
    let mut local_path = PathBuf::from("ffmpeg");
    if !exe_ext.is_empty() { local_path.set_extension(exe_ext); }
    if local_path.exists() {
        return local_path.to_string_lossy().to_string();
    }

    // Default to system PATH
    "".to_string()
}

fn get_ffprobe_path() -> String {
    // Check PLUGIN_DIR first
    let exe_ext = std::env::consts::EXE_EXTENSION;
    if let Ok(lock) = PLUGIN_DIR.read() {
        if let Some(plugin_path) = lock.as_ref() {
            // Check if ffprobe is inside this plugin directory
            let mut bin_path = plugin_path.join("ffprobe");
            if !exe_ext.is_empty() { bin_path.set_extension(exe_ext); }
            if bin_path.exists() { return bin_path.to_string_lossy().to_string(); }

            // Check sibling ffmpeg-utils
            if let Some(plugins_dir) = plugin_path.parent() {
                if let Ok(entries) = std::fs::read_dir(plugins_dir) {
                    for entry in entries.filter_map(|e| e.ok()) {
                        let path = entry.path();
                        if path.is_dir() {
                            let dir_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                            if dir_name.starts_with("FFmpeg Provider") || dir_name.starts_with("ffmpeg-utils") {
                                let mut bin_path = path.join("ffprobe");
                                if !exe_ext.is_empty() { bin_path.set_extension(exe_ext); }
                                if bin_path.exists() { return bin_path.to_string_lossy().to_string(); }

                                let mut bin_sub_path = path.join("bin").join("ffprobe");
                                if !exe_ext.is_empty() { bin_sub_path.set_extension(exe_ext); }
                                if bin_sub_path.exists() { return bin_sub_path.to_string_lossy().to_string(); }
                            }
                        }
                    }
                }
            }
        }
    }

    let mut search_paths = Vec::new();

    // 1. Check relative to Executable
    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(root) = current_exe.parent() {
            search_paths.push(root.to_path_buf());
        }
    }

    // 2. Check relative to CWD
    if let Ok(cwd) = std::env::current_dir() {
        search_paths.push(cwd);
    }

    let exe_ext = std::env::consts::EXE_EXTENSION;

    for root in search_paths {
        // Try to find "plugins" directory
        let possible_plugin_dirs = vec![
            root.join("plugins"),
            root.join("backend").join("plugins"),
            root.join("ting-reader").join("backend").join("plugins"),
            root.join("..").join("..").join("plugins"), 
        ];

        for plugins_dir in possible_plugin_dirs {
            if plugins_dir.exists() {
                if let Ok(entries) = std::fs::read_dir(&plugins_dir) {
                    for entry in entries.filter_map(|e| e.ok()) {
                        let path = entry.path();
                        if path.is_dir() {
                            let dir_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                            if dir_name.starts_with("FFmpeg Provider") || dir_name.starts_with("ffmpeg-utils") {
                                let mut bin_path = path.join("ffprobe");
                                if !exe_ext.is_empty() { bin_path.set_extension(exe_ext); }
                                if bin_path.exists() { return bin_path.to_string_lossy().to_string(); }

                                let mut bin_sub_path = path.join("bin").join("ffprobe");
                                if !exe_ext.is_empty() { bin_sub_path.set_extension(exe_ext); }
                                if bin_sub_path.exists() { return bin_sub_path.to_string_lossy().to_string(); }
                            }
                        }
                    }
                }
            }
        }
    }
    
    // Check ./ffprobe.exe (CWD root)
    let mut local_path = PathBuf::from("ffprobe");
    if !exe_ext.is_empty() { local_path.set_extension(exe_ext); }
    if local_path.exists() {
        return local_path.to_string_lossy().to_string();
    }

    "".to_string()
}

fn configure(params: Value) -> Result<Value, String> {
    // Allow host to configure ffmpeg path
    if let Some(path) = params.get("ffmpeg_path").and_then(|v| v.as_str()) {
        // Store it globally?
        // For now just acknowledge
        let mut ffmpeg = FFMPEG_PATH.lock().unwrap();
        *ffmpeg = Some(path.to_string());
    }
    Ok(json!({ "status": "configured" }))
}

fn detect(params: Value) -> Result<Value, String> {
    let path_str = params["file_path"].as_str().ok_or("Missing file_path")?;
    let path = Path::new(path_str);
    
    // Check extension
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    let is_supported = match ext.as_str() {
        "m4a" | "mp4" | "wma" | "flac" | "ape" | "wav" | "ogg" | "opus" | "aac" => true,
        _ => false
    };
    
    Ok(json!({ "is_supported": is_supported }))
}

fn fix_encoding(s: &str) -> String {
    // Check if the string contains only Latin-1 characters
    let bytes: Vec<u8> = s.chars()
        .map(|c| c as u32)
        .filter(|&c| c <= 255)
        .map(|c| c as u8)
        .collect();
    
    // If length differs, it contains non-Latin-1 chars, so likely UTF-8
    if bytes.len() != s.chars().count() {
        return s.to_string();
    }
    
    // Try to decode as GBK
    // If it decodes without errors, use it
    let (cow, _, had_errors) = GBK.decode(&bytes);
    if !had_errors {
        return cow.into_owned();
    }
    
    s.to_string()
}

fn extract_metadata(params: Value) -> Result<Value, String> {
    let path_str = params["file_path"].as_str().ok_or("Missing file_path")?;
    let path = Path::new(path_str);
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    let extract_cover = params.get("extract_cover").and_then(|v| v.as_bool()).unwrap_or(true);
    
    // 1. Try to read metadata using Lofty first
    // Lofty is faster and allows us to handle encoding manually if needed
    let mut metadata = json!({
        "format": ext,
        "duration": 0.0
    });
    
    let meta_obj = metadata.as_object_mut().unwrap();
    
    // Attempt to read via Lofty with Relaxed mode
    let parse_options = ParseOptions::new().parsing_mode(ParsingMode::Relaxed);
    
    if let Ok(tagged_file) = Probe::open(path)
         .and_then(|p| {
              p.options(parse_options).read()
         }) {
        // Duration
        let duration = tagged_file.properties().duration();
        let duration_sec = duration.as_secs_f64();
        meta_obj.insert("duration".to_string(), json!(duration_sec));
        
        // Tags
        if let Some(tag) = tagged_file.primary_tag() {
            if let Some(title) = tag.title() {
                let s: &str = &title;
                meta_obj.insert("title".to_string(), json!(fix_encoding(s)));
            }
            if let Some(artist) = tag.artist() {
                let s: &str = &artist;
                meta_obj.insert("artist".to_string(), json!(fix_encoding(s)));
            }
            if let Some(album) = tag.album() {
                let s: &str = &album;
                meta_obj.insert("album".to_string(), json!(fix_encoding(s)));
            }
            // Removed year extraction due to compatibility issues
            if let Some(genre) = tag.genre() {
                let s: &str = &genre;
                meta_obj.insert("genre".to_string(), json!(fix_encoding(s)));
            }
            if let Some(comment) = tag.comment() {
                let s: &str = &comment;
                meta_obj.insert("comment".to_string(), json!(fix_encoding(s)));
                meta_obj.insert("description".to_string(), json!(fix_encoding(s)));
            }
        }
    }
    
    // If we have basic metadata from Lofty, we might still want to use ffprobe for cover art extraction
    // Or if Lofty failed completely.
    
    // If duration is still 0 or missing title, fallback to ffprobe
    // But for now, let's assume Lofty works for metadata.
    
    // However, for cover art, Lofty can give us the Picture, but we need to write it to disk.
    // If Lofty found a picture, we can save it.
    let mut cover_extracted_by_lofty = false;
    
    // We try to reuse the already opened tagged file if possible, but the code structure here
    // makes it easier to just reopen.
    if extract_cover {
        let parse_options = ParseOptions::new().parsing_mode(ParsingMode::Relaxed);
        if let Ok(tagged_file) = Probe::open(path)
             .and_then(|p| {
                  p.options(parse_options).read()
             }) {
             if let Some(tag) = tagged_file.primary_tag() {
                 let pictures = tag.pictures();
                 if !pictures.is_empty() {
                     let pic = &pictures[0];
                     let mime = pic.mime_type();
                     let data = pic.data();
                     
                     let ext = match mime {
                         Some(MimeType::Png) => "png",
                         Some(MimeType::Jpeg) => "jpg",
                         Some(MimeType::Tiff) => "tiff",
                         Some(MimeType::Bmp) => "bmp",
                         Some(MimeType::Gif) => "gif",
                         _ => "jpg"
                     };
    
                     if let Some(parent) = path.parent() {
                         let cover_filename = format!("cover.{}", ext);
                         let cover_path = parent.join(&cover_filename);
                         if !cover_path.exists() {
                             if let Ok(_) = std::fs::write(&cover_path, data) {
                                 meta_obj.insert("cover_url".to_string(), json!(cover_path.to_string_lossy()));
                                 cover_extracted_by_lofty = true;
                             }
                         } else {
                              meta_obj.insert("cover_url".to_string(), json!(cover_path.to_string_lossy()));
                              cover_extracted_by_lofty = true;
                         }
                     }
                 }
             }
        }
    }

    // Even if Lofty worked, we might want to run the description cleaning logic later.
    // But if Lofty worked, we don't need ffprobe unless we are missing data.
    
    let has_title = meta_obj.contains_key("title");
    
    if !has_title || !cover_extracted_by_lofty {
        // Fallback to ffprobe or use it for cover extraction if Lofty missed it
        let ffprobe = get_ffprobe_path();
        
        if !ffprobe.is_empty() {
            let ffmpeg = get_ffmpeg_path();
            
            // ... (Existing ffprobe logic) ...
            // We will merge ffprobe results into meta_obj if keys are missing
            
            let output = Command::new(&ffprobe)
            .arg("-v")
            .arg("quiet")
            .arg("-print_format")
            .arg("json")
            .arg("-show_format")
            .arg("-show_streams") 
            .arg(path_str)
            .output();
            
        if let Ok(out) = output {
            if out.status.success() {
                if let Ok(json_out) = serde_json::from_slice::<Value>(&out.stdout) {
                    if let Some(format) = json_out.get("format") {
                        // Duration fallback
                        if meta_obj.get("duration").and_then(|d| d.as_f64()).unwrap_or(0.0) == 0.0 {
                            let d = format.get("duration")
                                .and_then(|v| v.as_str())
                                .and_then(|s| s.parse::<f64>().ok())
                                .unwrap_or(0.0);
                             meta_obj.insert("duration".to_string(), json!(d));
                        }
                        
                        // Metadata fallback
                        if let Some(tags) = format.get("tags").and_then(|t| t.as_object()) {
                            for (k, v) in tags {
                                let k_lower = k.to_lowercase();
                                let raw_str = v.as_str().unwrap_or("").to_string();
                                let v_str = fix_encoding(&raw_str);
                                
                                // Only insert if missing
                                match k_lower.as_str() {
                                    "title" | "nam" | "name" => { if !meta_obj.contains_key("title") { meta_obj.insert("title".to_string(), json!(v_str)); } },
                                    "artist" | "art" => { if !meta_obj.contains_key("artist") { meta_obj.insert("artist".to_string(), json!(v_str)); } },
                                    "album" | "alb" => { if !meta_obj.contains_key("album") { meta_obj.insert("album".to_string(), json!(v_str)); } },
                                    // ... handle others as needed
                                    "description" | "desc" | "synopsis" => {
                                        // Always try to get description if current is empty
                                         if !meta_obj.contains_key("description") {
                                              meta_obj.insert("description".to_string(), json!(v_str));
                                         }
                                    },
                                    _ => {}
                                }
                            }
                        }
                    }
                    
                    // Cover extraction using ffmpeg if Lofty failed
                    if extract_cover && !cover_extracted_by_lofty {
                        // ... (Use existing cover extraction logic, but simplified)
                        // Copy paste the cover logic logic from previous code
                        let mut has_cover = false;
                        let mut cover_codec = "jpg".to_string();

                        if let Some(streams) = json_out.get("streams").and_then(|v| v.as_array()) {
                            for stream in streams {
                                let mut is_this_cover = false;
                                if let Some(disposition) = stream.get("disposition") {
                                    if let Some(val) = disposition.get("attached_pic") {
                                        if val.as_i64() == Some(1) || val.as_u64() == Some(1) || val.as_str() == Some("1") {
                                            is_this_cover = true;
                                        }
                                    }
                                }
                                if !is_this_cover {
                                     if let Some(codec_type) = stream.get("codec_type").and_then(|v| v.as_str()) {
                                         if codec_type == "video" {
                                             if let Some(codec_name) = stream.get("codec_name").and_then(|v| v.as_str()) {
                                                 if codec_name == "mjpeg" || codec_name == "png" {
                                                     is_this_cover = true;
                                                 }
                                             }
                                         }
                                     }
                                }
                                if is_this_cover {
                                    has_cover = true;
                                    if let Some(codec) = stream.get("codec_name").and_then(|v| v.as_str()) {
                                        if codec == "png" { cover_codec = "png".to_string(); }
                                        else if codec == "webp" { cover_codec = "webp".to_string(); }
                                        else { cover_codec = "jpg".to_string(); }
                                    }
                                    break;
                                }
                            }
                        }
                        
                        if has_cover {
                            if let Some(parent) = path.parent() {
                                let cover_filename = format!("cover.{}", cover_codec);
                                let cover_path = parent.join(&cover_filename);
                                if !cover_path.exists() {
                                    let _ = Command::new(&ffmpeg)
                                        .arg("-loglevel").arg("error").arg("-y")
                                        .arg("-i").arg(path_str)
                                        .arg("-map").arg("0:v:0")
                                        .arg("-c").arg("copy")
                                        .arg("-f").arg(if cover_codec == "jpg" { "mjpeg" } else { "image2" }) 
                                        .arg(&cover_path)
                                        .status();
                                }
                                if cover_path.exists() {
                                    meta_obj.insert("cover_url".to_string(), json!(cover_path.to_string_lossy()));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    }

    // Description cleaning and author/narrator extraction from description text
    if let Some(desc_val) = meta_obj.get("description").and_then(|v| v.as_str()) {
        // ... (existing logic)
        let raw = desc_val.to_string();
        if !raw.trim().is_empty() {
            let mut t = raw.replace("<br>", "\n").replace("<br/>", "\n").replace("<br />", "\n");
            let re_p_open = Regex::new("(?is)<p[^>]*>").unwrap();
            t = re_p_open.replace_all(&t, "\n").to_string();
            let re_p_close = Regex::new("(?is)</p>").unwrap();
            t = re_p_close.replace_all(&t, "\n").to_string();
            let re_tags = Regex::new("(?is)<[^>]+>").unwrap();
            t = re_tags.replace_all(&t, "").to_string();
            t = t.replace("&nbsp;", " ").replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">");
            let re_ws = Regex::new(r"[ \t\u{00A0}]+").unwrap();
            t = re_ws.replace_all(&t, " ").to_string();
            let re_blank = Regex::new(r"\n{2,}").unwrap();
            t = re_blank.replace_all(&t, "\n").to_string();
            let t = t.trim().to_string();
            if !t.is_empty() {
                meta_obj.insert("description".to_string(), json!(t.clone()));
                
                let author_re = Regex::new(r"(?im)(?:作者|原著|作家)\s*[：:]\s*([^\n]+)").unwrap();
                let narrator_re = Regex::new(r"(?im)(?:主播|演播|播讲|朗读)\s*[：:]\s*([^\n]+)").unwrap();
                
                let clean_extracted = |val: &str| -> String {
                     let mut s = val.trim().to_string();
                     if let Some(idx) = s.find(|c| c == '，' || c == ',' || c == '。' || c == '；' || c == ';') {
                         s = s[..idx].trim().to_string();
                     }
                     s
                };

                let author_from_desc = author_re.captures(&t).and_then(|c| c.get(1)).map(|m| clean_extracted(m.as_str()));
                let narrator_from_desc = narrator_re.captures(&t).and_then(|c| c.get(1)).map(|m| clean_extracted(m.as_str()));
                
                let artist_val = meta_obj.get("artist").and_then(|v| v.as_str()).unwrap_or("").to_string();

                if let Some(a) = author_from_desc.clone() {
                    if !a.is_empty() {
                        meta_obj.insert("album_artist".to_string(), json!(a.clone()));
                        if !artist_val.is_empty() && artist_val == a {
                             meta_obj.insert("artist".to_string(), json!(a));
                        }
                    }
                }
                
                if let Some(n) = narrator_from_desc.clone() {
                    if !n.is_empty() {
                        meta_obj.insert("narrator".to_string(), json!(n.clone()));
                        if artist_val.trim().is_empty() || artist_val.trim() == n {
                            meta_obj.insert("artist".to_string(), json!(n));
                        }
                    }
                }
            }
        }
    }
    
    Ok(metadata)
}

fn get_stream_url(params: Value) -> Result<Value, String> {
    // This plugin acts as a transcoder.
    // It should return a command that the host can execute to get the stream.
    // OR, if the host expects a URL, we might need to start a local server?
    // Usually Native Plugins for "Format" might just return the stream command if the host supports it.
    
    // However, TingReader's architecture for plugins:
    // If it's a "Format" plugin, it might be called to "get_media_source".
    
    // Let's assume the host asks "how do I play this?".
    // If we want to support transcoding, we might return a special protocol URL or a command.
    
    // If we want to support "streaming" m4a as mp3 (transcoding), we typically do this via a piped command.
    // But the current `audio_streamer.rs` in backend uses `symphonia` or `File`.
    
    // Wait, the user said "support streaming playback (mp3 stream)".
    // This implies the backend will ask the plugin for a stream.
    
    // If the backend calls `get_stream_command`, we can return:
    // ffmpeg -i input.m4a -f mp3 -
    
    let path_str = params["file_path"].as_str().ok_or("Missing file_path")?;
    let transcode = params["transcode"].as_str().unwrap_or("mp3");
    let seek = params["seek"].as_str().filter(|s| !s.trim().is_empty());
    let ffmpeg = get_ffmpeg_path();
    if ffmpeg.is_empty() {
        return Err("FFmpeg not found in plugin directory".to_string());
    }
    
    // We construct a command that outputs MP3 data to stdout
    let mut command = vec![
        ffmpeg,
        "-loglevel".to_string(),
        "error".to_string(),
    ];

    if let Some(seek_time) = seek {
        command.push("-ss".to_string());
        command.push(seek_time.to_string());
    }

    command.push("-i".to_string());
    command.push(path_str.to_string());

    match transcode {
        "mp3" => {
            command.extend([
                "-vn".to_string(),
                "-map".to_string(),
                "0:a:0".to_string(),
                "-acodec".to_string(),
                "libmp3lame".to_string(),
                "-b:a".to_string(),
                "128k".to_string(),
                "-ac".to_string(),
                "2".to_string(),
                "-ar".to_string(),
                "44100".to_string(),
                "-f".to_string(),
                "mp3".to_string(),
                "-".to_string(),
            ]);
        }
        "wav" => {
            command.extend([
                "-vn".to_string(),
                "-map".to_string(),
                "0:a:0".to_string(),
                "-f".to_string(),
                "wav".to_string(),
                "-".to_string(),
            ]);
        }
        _ => return Err("Unsupported transcode format".to_string()),
    }
    
    Ok(json!({
        "stream_type": "pipe",
        "command": command,
        "content_type": if transcode == "wav" { "audio/wav" } else { "audio/mpeg" }
    }))
}

fn get_decryption_plan(_params: Value) -> Result<Value, String> {
    // Return a plain plan to allow direct streaming
    Ok(json!({
        "segments": [
            {
                "type": "plain",
                "offset": 0,
                "length": -1 // Read until end
            }
        ],
        "total_size": null // Use actual file size
    }))
}

fn get_metadata_read_size(_params: Value) -> Result<Value, String> {
    Ok(json!({
        "size": 1024 * 1024 // 1MB
    }))
}

fn write_metadata(params: Value) -> Result<Value, String> {
    let path_str = params["file_path"].as_str().ok_or("Missing file_path")?;
    let path = std::path::Path::new(path_str);
    
    // Check if file exists
    if !path.exists() {
        eprintln!("[native-audio-support] File not found: {}", path_str);
        return Err(format!("File not found: {}", path_str));
    }

    eprintln!("[native-audio-support] Writing metadata to: {}", path_str);
    eprintln!("[native-audio-support] Params: {}", params);

    // Open file with Lofty
    // Use Relaxed parsing mode to handle malformed tags (e.g. invalid BOM)
    let parsing_options = ParseOptions::new().parsing_mode(ParsingMode::Relaxed);
    
    let mut tagged_file = match Probe::open(path) {
        Ok(p) => {
             match p.options(parsing_options).read() {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("[native-audio-support] Failed to read file tags with Relaxed mode: {}", e);
                    
                    // Fallback: Try reading without tags if parsing failed completely
                    eprintln!("[native-audio-support] Attempting to read without tags...");
                    let p_fallback = Probe::open(path).map_err(|e| format!("Failed to reopen file: {}", e))?;
                    match p_fallback.options(ParseOptions::new().read_tags(false).parsing_mode(ParsingMode::Relaxed)).read() {
                        Ok(t) => {
                            eprintln!("[native-audio-support] Successfully read file structure (tags ignored/cleared)");
                            t
                        },
                        Err(e2) => return Err(format!("Failed to read file (even without tags): {}", e2)),
                    }
                }
             }
        },
        Err(e) => {
            eprintln!("[native-audio-support] Failed to open file: {}", e);
            return Err(format!("Failed to open file: {}", e));
        }
    };

    // Get primary tag or create one
    let tag_type = match path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase().as_str() {
        "wav" | "aiff" => lofty::tag::TagType::Id3v2,
        "flac" | "ogg" | "opus" => lofty::tag::TagType::VorbisComments,
        "m4a" | "mp4" | "m4b" => lofty::tag::TagType::Mp4Ilst,
        "ape" => lofty::tag::TagType::Ape,
        _ => lofty::tag::TagType::Id3v2,
    };

    // Ensure we have a tag of the correct type
    if tagged_file.tag(tag_type).is_none() {
        eprintln!("[native-audio-support] Creating new tag of type: {:?}", tag_type);
        tagged_file.insert_tag(lofty::tag::Tag::new(tag_type));
    }

    let tag = tagged_file.tag_mut(tag_type).ok_or("Failed to get tag")?;

    // Update Text Fields
    // NOTE: To fix potential UTF-16 BOM issues when converting ID3v2.4 (UTF-8) to ID3v2.3 (UTF-16),
    // we explicitly remove the old frames before setting new ones. This forces Lofty to re-encode the text.
    if let Some(title) = params.get("title").and_then(|v| v.as_str()) {
        eprintln!("[native-audio-support] Setting title: {}", title);
        tag.remove_title();
        tag.set_title(title.to_string());
    }
    if let Some(artist) = params.get("artist").and_then(|v| v.as_str()) {
        eprintln!("[native-audio-support] Setting artist: {}", artist);
        tag.remove_artist();
        tag.set_artist(artist.to_string());
    }
    if let Some(album) = params.get("album").and_then(|v| v.as_str()) {
        eprintln!("[native-audio-support] Setting album: {}", album);
        tag.remove_album();
        tag.set_album(album.to_string());
    }
    if let Some(genre) = params.get("genre").and_then(|v| v.as_str()) {
        eprintln!("[native-audio-support] Setting genre: {}", genre);
        tag.remove_genre();
        tag.set_genre(genre.to_string());
    }
    // Handle description/comment
    if let Some(description) = params.get("description").and_then(|v| v.as_str()) {
        eprintln!("[native-audio-support] Setting comment/description");
        tag.remove_comment();
        tag.set_comment(description.to_string());
    }

    // Update Cover Art
    if let Some(cover_path_str) = params.get("cover_path").and_then(|v| v.as_str()) {
        eprintln!("[native-audio-support] Processing cover: {}", cover_path_str);
        let cover_path = std::path::Path::new(cover_path_str);
        
        if cover_path.exists() {
             use std::io::Read;
             let mut file = std::fs::File::open(cover_path).map_err(|e| format!("Failed to open cover file: {}", e))?;
             let mut data = Vec::new();
             file.read_to_end(&mut data).map_err(|e| format!("Failed to read cover file: {}", e))?;
             
             let ext = cover_path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
             let is_m4a = matches!(path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase().as_str(), "m4a" | "mp4" | "m4b");
             
             // Check if conversion is needed (WebP -> JPEG, or for M4A compliance)
             let mut final_data = data;
             let mut final_mime = match ext.as_str() {
                 "png" => lofty::picture::MimeType::Png,
                 "jpg" | "jpeg" => lofty::picture::MimeType::Jpeg,
                 "gif" => lofty::picture::MimeType::Gif,
                 "bmp" => lofty::picture::MimeType::Bmp,
                 "tiff" => lofty::picture::MimeType::Tiff,
                 _ => lofty::picture::MimeType::Unknown(format!("image/{}", ext)),
             };

             // If M4A and not JPEG/PNG, or if it is WebP, convert to JPEG
             if (is_m4a && !matches!(final_mime, lofty::picture::MimeType::Jpeg | lofty::picture::MimeType::Png)) || ext == "webp" {
                 eprintln!("[native-audio-support] Converting image to JPEG for compatibility");
                 match image::load_from_memory(&final_data) {
                     Ok(img) => {
                         let mut jpeg_data = Vec::new();
                         let mut cursor = std::io::Cursor::new(&mut jpeg_data);
                         match img.write_to(&mut cursor, image::ImageOutputFormat::Jpeg(90)) {
                             Ok(_) => {
                                 final_data = jpeg_data;
                                 final_mime = lofty::picture::MimeType::Jpeg;
                                 eprintln!("[native-audio-support] Conversion successful, size: {} bytes", final_data.len());
                             },
                             Err(e) => eprintln!("[native-audio-support] Failed to encode JPEG: {}", e),
                         }
                     },
                     Err(e) => eprintln!("[native-audio-support] Failed to decode image for conversion: {}", e),
                 }
             }

             // Create Picture using lofty's from_reader to ensure valid metadata
             let mut cursor = std::io::Cursor::new(&final_data);
             match lofty::picture::Picture::from_reader(&mut cursor) {
                 Ok(mut pic) => {
                     pic.set_pic_type(lofty::picture::PictureType::CoverFront);
                     
                     // Force remove all existing pictures to ensure replacement
                     // This handles cases where M4A/MP3 might have multiple covers or different types
                     let _ = tag.remove_picture_type(lofty::picture::PictureType::CoverFront);
                     let _ = tag.remove_picture_type(lofty::picture::PictureType::Other);
                     // For good measure, try to clear pictures if possible, but the API might not expose clear() directly on Tag trait easily.
                     // Instead, we can just push. Lofty usually appends.
                     // If we want to replace, we must remove.
                     // Let's remove ANY picture.
                     for pt in [
                         lofty::picture::PictureType::Other,
                         lofty::picture::PictureType::Icon,
                         lofty::picture::PictureType::OtherIcon,
                         lofty::picture::PictureType::CoverFront,
                         lofty::picture::PictureType::CoverBack,
                         lofty::picture::PictureType::Leaflet,
                         lofty::picture::PictureType::Media,
                         lofty::picture::PictureType::LeadArtist,
                         lofty::picture::PictureType::Artist,
                         lofty::picture::PictureType::Conductor,
                         lofty::picture::PictureType::Band,
                         lofty::picture::PictureType::Composer,
                         lofty::picture::PictureType::Lyricist,
                         lofty::picture::PictureType::RecordingLocation,
                         lofty::picture::PictureType::DuringRecording,
                         lofty::picture::PictureType::DuringPerformance,
                        lofty::picture::PictureType::ScreenCapture,
                        lofty::picture::PictureType::Illustration,
                         lofty::picture::PictureType::BandLogo,
                         lofty::picture::PictureType::PublisherLogo,
                     ] {
                         tag.remove_picture_type(pt);
                     }

                     tag.push_picture(pic);
                     eprintln!("[native-audio-support] Cover set successfully");
                 }
                 Err(e) => {
                     eprintln!("[native-audio-support] Lofty rejected the image data: {}", e);
                     // Fallback to unchecked if from_reader fails (sometimes happens with valid jpegs)
                     let pic = lofty::picture::Picture::unchecked(final_data)
                         .pic_type(lofty::picture::PictureType::CoverFront)
                         .mime_type(final_mime)
                         .build();
                     tag.remove_picture_type(lofty::picture::PictureType::CoverFront);
                     tag.push_picture(pic);
                     eprintln!("[native-audio-support] Cover set using unchecked fallback");
                 }
             }
        } else {
            eprintln!("[native-audio-support] Cover file does not exist: {}", cover_path_str);
        }
    }

    // Save changes
    let options = WriteOptions::default();
    
    match tagged_file.save_to_path(path, options) {
        Ok(_) => {
            eprintln!("[native-audio-support] Metadata saved successfully");
            Ok(json!({ "status": "success" }))
        },
        Err(e) => {
            eprintln!("[native-audio-support] Failed to save tags: {}", e);
            Err(format!("Failed to save tags: {}", e))
        },
    }
}
