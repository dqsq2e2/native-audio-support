use std::ffi::{CStr, CString};
use std::os::raw::c_int;
use std::path::{Path, PathBuf};
use std::process::Command;
use serde_json::{json, Value};
use std::sync::Mutex;
use regex::Regex;
use encoding_rs::GBK;
use lofty::probe::Probe;
use lofty::prelude::*;
use lofty::picture::MimeType;
use std::borrow::Cow;

// Global state to manage FFmpeg path or other resources
// Since this is a dylib, we can use static mutable state with synchronization
lazy_static::lazy_static! {
    static ref FFMPEG_PATH: Mutex<Option<String>> = Mutex::new(None);
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

    let params_json: Value = match serde_json::from_str(params_str) {
        Ok(v) => v,
        Err(_) => return -1,
    };

    let result = match method_str {
        "detect" => detect(params_json),
        "extract_metadata" => extract_metadata(params_json),
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

fn get_ffmpeg_path() -> String {
    let ffmpeg = FFMPEG_PATH.lock().unwrap();
    if let Some(path) = &*ffmpeg {
        return path.clone();
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
    "ffmpeg".to_string()
}

fn get_ffprobe_path() -> String {
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

    "ffprobe".to_string()
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
    
    // 1. Try to read metadata using Lofty first
    // Lofty is faster and allows us to handle encoding manually if needed
    let mut metadata = json!({
        "format": ext,
        "duration": 0.0
    });
    
    let meta_obj = metadata.as_object_mut().unwrap();
    
    // Attempt to read via Lofty
    if let Ok(tagged_file) = Probe::open(path).and_then(|p| p.read()) {
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
    
    if let Ok(tagged_file) = Probe::open(path).and_then(|p| p.read()) {
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

    // Even if Lofty worked, we might want to run the description cleaning logic later.
    // But if Lofty worked, we don't need ffprobe unless we are missing data.
    
    let has_title = meta_obj.contains_key("title");
    
    if !has_title || !cover_extracted_by_lofty {
        // Fallback to ffprobe or use it for cover extraction if Lofty missed it
        let ffprobe = get_ffprobe_path();
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
                    if !cover_extracted_by_lofty {
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

    // Description cleaning and author/narrator extraction from description text
    if let Some(desc_val) = meta_obj.get("description").and_then(|v| v.as_str()) {
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
    let ffmpeg = get_ffmpeg_path();
    
    // We construct a command that outputs MP3 data to stdout
    let command = vec![
        ffmpeg,
        "-i".to_string(),
        path_str.to_string(),
        "-f".to_string(),
        "mp3".to_string(),
        "-".to_string()
    ];
    
    Ok(json!({
        "stream_type": "pipe",
        "command": command,
        "content_type": "audio/mpeg"
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
