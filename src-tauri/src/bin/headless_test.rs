//! Headless test binary — simulates the full player flow without GUI/audio output.
//! For VPS debugging only. DO NOT COMMIT (hardcoded tokens).

use chrono::Datelike;
use std::io::BufReader;
use std::path::PathBuf;

const API_BASE: &str = "https://apimillsonic.fo.com.uy/api/v1";
const DEVICE_ID: &str = "1ccce50a-4e8c-4871-8ff0-98353d90f705";
const DEVICE_TOKEN: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJkZXZpY2VJZCI6IjFjY2NlNTBhLTRlOGMtNDg3MS04ZmYwLTk4MzUzZDkwZjcwNSIsInpvbmVJZCI6IjUyMjQzNThlLTcwNDktNGQ0MS05YTE1LWUzNTM4ZDFkYWFiZiIsInRlbmFudElkIjoiODgxMTRmMzctNWYyYi00NDUzLTk3NDQtMmI5ZDE0NmRiZGMxIiwidHlwZSI6ImRldmljZSIsImlhdCI6MTc3MjYzNTI5NCwiZXhwIjoxODA0MTcxMjk0fQ.z2c5wueXdveRpLhZtIWWdxFA_PMd0iYFefWwkwJoGyw";

#[tokio::main]
async fn main() {
    println!("=== MILLSONIC HEADLESS TEST ===");
    println!("Device: {}", DEVICE_ID);
    println!();

    // Step 1: Sync API
    println!("--- STEP 1: Sync API ---");
    let client = reqwest::Client::new();
    let url = format!("{}/devices/{}/sync?deviceToken={}", API_BASE, DEVICE_ID, DEVICE_TOKEN);
    println!("GET {}", url);

    let sync_resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            println!("FATAL: Request failed: {}", e);
            return;
        }
    };
    let status = sync_resp.status();
    println!("HTTP Status: {}", status);

    let sync_data: serde_json::Value = match sync_resp.json().await {
        Ok(v) => v,
        Err(e) => {
            println!("FATAL: Failed to parse JSON: {}", e);
            return;
        }
    };

    if sync_data.get("statusCode").is_some() {
        println!("API ERROR: {}", sync_data);
        return;
    }

    let tz_str = sync_data.get("timezone").and_then(|v| v.as_str()).unwrap_or("America/Montevideo");
    println!("Timezone: {}", tz_str);

    if let Some(device) = sync_data.get("device") {
        println!("Device volume: {:?}", device.get("volume"));
        println!("Device name: {:?}", device.get("name").and_then(|v| v.as_str()));
    }

    // Step 2: Parse schedule
    println!("\n--- STEP 2: Parse Schedule ---");
    let tz: chrono_tz::Tz = tz_str.parse().unwrap_or(chrono_tz::America::Montevideo);
    let now = chrono::Utc::now().with_timezone(&tz);
    let day_of_week = now.weekday().num_days_from_sunday();
    let current_time = now.format("%H:%M").to_string();
    println!("Local time: {} (day_of_week={})", now.format("%Y-%m-%d %H:%M:%S"), day_of_week);

    let schedule = sync_data.get("schedule").cloned().unwrap_or(serde_json::json!([]));
    let slots = schedule.as_array().cloned().unwrap_or_default();
    println!("Total schedule slots: {}", slots.len());

    let mut current_tracks: Vec<serde_json::Value> = Vec::new();
    for slot in &slots {
        let slot_day = slot.get("dayOfWeek").and_then(|d| d.as_u64()).unwrap_or(99) as u32;
        let start = slot.get("startTime").and_then(|s| s.as_str()).unwrap_or("00:00");
        let end = slot.get("endTime").and_then(|s| s.as_str()).unwrap_or("23:59");

        let matches = slot_day == day_of_week && current_time.as_str() >= start && current_time.as_str() < end;
        if matches {
            println!(">>> MATCHED slot: day={} {}-{}", slot_day, start, end);
            if let Some(playlist) = slot.get("playlist") {
                let name = playlist.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                if let Some(tracks) = playlist.get("tracks").and_then(|t| t.as_array()) {
                    println!("Playlist '{}' has {} tracks", name, tracks.len());
                    current_tracks = tracks.clone();
                }
            }
            break;
        }
    }

    if current_tracks.is_empty() {
        println!("No tracks for current time slot. Dumping all slots:");
        for (i, slot) in slots.iter().enumerate() {
            println!("  Slot {}: day={} {}-{} playlist={:?}",
                i,
                slot.get("dayOfWeek").and_then(|d| d.as_u64()).unwrap_or(99),
                slot.get("startTime").and_then(|s| s.as_str()).unwrap_or("?"),
                slot.get("endTime").and_then(|s| s.as_str()).unwrap_or("?"),
                slot.get("playlist").and_then(|p| p.get("name")).and_then(|n| n.as_str()),
            );
        }
        println!("Nothing to test. Exiting.");
        return;
    }

    // Step 3: Download first 2 tracks
    println!("\n--- STEP 3: Download Tracks ---");
    let cache_dir = PathBuf::from("/tmp/millsonic-headless-test");
    let _ = std::fs::create_dir_all(&cache_dir);

    let tracks_to_test: Vec<_> = current_tracks.iter().take(2).collect();
    let mut downloaded_files: Vec<(String, PathBuf)> = Vec::new();

    for (i, track) in tracks_to_test.iter().enumerate() {
        let track_id = track.get("id").and_then(|v| v.as_str()).unwrap_or("unknown");
        let title = track.get("title").and_then(|v| v.as_str()).unwrap_or("Unknown");
        let artist = track.get("artist").and_then(|v| v.as_str()).unwrap_or("Unknown");
        let duration = track.get("duration").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let stream_url = track.get("streamUrl").and_then(|v| v.as_str()).unwrap_or("");

        println!("\nTrack {} — '{}' by '{}' (duration={:.1}s)", i + 1, title, artist, duration);
        println!("  streamUrl: {}", if stream_url.is_empty() { "(empty)" } else { &stream_url[..stream_url.len().min(120)] });

        if stream_url.is_empty() {
            println!("  SKIP: no streamUrl");
            continue;
        }

        let file_path = cache_dir.join(format!("{}.mp3", track_id));

        // Download
        let resp = match client.get(stream_url).send().await {
            Ok(r) => r,
            Err(e) => {
                println!("  DOWNLOAD FAILED: {}", e);
                continue;
            }
        };

        let dl_status = resp.status();
        let content_length = resp.content_length();
        println!("  HTTP {}, Content-Length: {:?}", dl_status, content_length);

        if !dl_status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            println!("  ERROR BODY: {}", &body[..body.len().min(300)]);
            continue;
        }

        let bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => {
                println!("  FAILED reading body: {}", e);
                continue;
            }
        };

        println!("  Downloaded: {} bytes", bytes.len());
        if let Some(cl) = content_length {
            if bytes.len() as u64 != cl {
                println!("  ⚠ SIZE MISMATCH: got {} expected {}", bytes.len(), cl);
            } else {
                println!("  ✓ Size matches Content-Length");
            }
        }

        std::fs::write(&file_path, &bytes).unwrap();
        downloaded_files.push((title.to_string(), file_path));
    }

    // Step 4 & 5: Validate files and decode with rodio
    println!("\n--- STEP 4 & 5: Validate & Decode ---");
    for (title, path) in &downloaded_files {
        println!("\nFile: {} ({})", title, path.display());
        let data = std::fs::read(path).unwrap();
        println!("  Size: {} bytes", data.len());

        // Check header
        if data.len() < 4 {
            println!("  ✗ File too small!");
            continue;
        }

        let header_hex = format!("{:02X} {:02X} {:02X} {:02X}", data[0], data[1], data[2], data[3]);
        println!("  First 4 bytes: {}", header_hex);

        if data[0] == 0x49 && data[1] == 0x44 && data[2] == 0x33 {
            println!("  ✓ ID3 tag detected (ID3v2)");
        } else if data[0] == 0xFF && (data[1] & 0xE0) >= 0xE0 {
            println!("  ✓ MP3 sync word detected");
        } else if data[0] == 0x4F && data[1] == 0x67 && data[2] == 0x67 {
            println!("  ℹ OGG container detected");
        } else if data[0] == 0x66 && data[1] == 0x4C && data[2] == 0x61 {
            println!("  ℹ FLAC detected");
        } else if &data[..4] == b"RIFF" {
            println!("  ℹ WAV/RIFF detected");
        } else {
            println!("  ⚠ Unknown format! First 16 bytes:");
            let hex: Vec<String> = data.iter().take(16).map(|b| format!("{:02X}", b)).collect();
            println!("    {}", hex.join(" "));
            let ascii: String = data.iter().take(16).map(|b| if b.is_ascii_graphic() || *b == b' ' { *b as char } else { '.' }).collect();
            println!("    {}", ascii);
        }

        // Try rodio decode
        println!("  Attempting rodio::Decoder::new()...");
        let file = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(e) => {
                println!("  ✗ Can't open file: {}", e);
                continue;
            }
        };
        let reader = BufReader::new(file);
        match rodio::Decoder::new(reader) {
            Ok(decoder) => {
                use rodio::Source;
                println!("  ✓ Decoder created successfully!");
                println!("    Channels: {:?}", decoder.channels());
                println!("    Sample rate: {:?}", decoder.sample_rate());
                println!("    Total duration: {:?}", decoder.total_duration());
                // Try reading a few samples
                let samples: Vec<i16> = decoder.take(4800).collect();
                println!("    Read {} samples (first ~100ms)", samples.len());
                if samples.is_empty() {
                    println!("    ⚠ Decoder produced 0 samples!");
                } else {
                    let max = samples.iter().map(|s| s.abs() as u32).max().unwrap_or(0);
                    let avg = samples.iter().map(|s| s.abs() as u64).sum::<u64>() / samples.len() as u64;
                    println!("    Max amplitude: {}, Avg: {}", max, avg);
                }
            }
            Err(e) => {
                println!("  ✗ Decoder FAILED: {:?}", e);
            }
        }
    }

    // Step 6: Test audio output (expected to fail on VPS)
    println!("\n--- STEP 6: Audio Output Test ---");
    match rodio::OutputStream::try_default() {
        Ok((_stream, handle)) => {
            println!("✓ OutputStream created (unexpected on VPS!)");
            let _ = handle; // avoid unused warning
        }
        Err(e) => {
            println!("✗ OutputStream failed (expected on VPS): {:?}", e);
            println!("  This is NORMAL — the player needs ALSA/PulseAudio.");
        }
    }

    // Cleanup
    println!("\n--- CLEANUP ---");
    let _ = std::fs::remove_dir_all(&cache_dir);
    println!("Removed {}", cache_dir.display());

    println!("\n=== TEST COMPLETE ===");
}
