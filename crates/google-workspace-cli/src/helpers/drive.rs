// Copyright 2026 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use super::Helper;
use crate::auth;
use crate::error::GwsError;
use crate::executor;
use clap::{Arg, ArgMatches, Command};
use google_workspace::validate::encode_path_segment;
use serde_json::{json, Value};
use std::future::Future;
use std::path::Path;
use std::pin::Pin;

pub struct DriveHelper;

impl Helper for DriveHelper {
    fn inject_commands(
        &self,
        mut cmd: Command,
        _doc: &crate::discovery::RestDescription,
    ) -> Command {
        cmd = cmd.subcommand(
            Command::new("+upload")
                .about("[Helper] Upload a file with automatic metadata")
                .arg(
                    Arg::new("file")
                        .help("Path to file to upload")
                        .required(true)
                        .index(1),
                )
                .arg(
                    Arg::new("parent")
                        .long("parent")
                        .help("Parent folder ID")
                        .value_name("ID"),
                )
                .arg(
                    Arg::new("name")
                        .long("name")
                        .help("Target filename (defaults to source filename)")
                        .value_name("NAME"),
                )
                .after_help(
                    "\
EXAMPLES:
  gws drive +upload ./report.pdf
  gws drive +upload ./report.pdf --parent FOLDER_ID
  gws drive +upload ./data.csv --name 'Sales Data.csv'

TIPS:
  MIME type is detected automatically.
  Filename is inferred from the local path unless --name is given.",
                ),
        );
        cmd = cmd.subcommand(
            Command::new("+organize")
                .about("[Helper] Match and move Meet recordings into client folders")
                .arg(
                    Arg::new("dry-run")
                        .long("dry-run")
                        .help("Preview matches without moving files")
                        .action(clap::ArgAction::SetTrue),
                )
                .after_help(
                    "\
EXAMPLES:
  gws drive +organize --dry-run
  gws drive +organize

TIPS:
  Auto-discovers the 'Meet Recordings' folder by name.
  Matches recordings and transcripts to client subfolders.
  Never deletes files — only moves them.
  Unmatched files are reported but left in place.",
                ),
        );
        cmd
    }

    fn handle<'a>(
        &'a self,
        doc: &'a crate::discovery::RestDescription,
        matches: &'a ArgMatches,
        _sanitize_config: &'a crate::helpers::modelarmor::SanitizeConfig,
    ) -> Pin<Box<dyn Future<Output = Result<bool, GwsError>> + Send + 'a>> {
        Box::pin(async move {
            if let Some(matches) = matches.subcommand_matches("+organize") {
                handle_organize(matches).await?;
                return Ok(true);
            }
            if let Some(matches) = matches.subcommand_matches("+upload") {
                let file_path = matches.get_one::<String>("file").unwrap();
                let parent_id = matches.get_one::<String>("parent");
                let name_arg = matches.get_one::<String>("name");

                // Determine filename
                let filename = determine_filename(file_path, name_arg.map(|s| s.as_str()))?;

                // Find method: files.create
                let files_res = doc
                    .resources
                    .get("files")
                    .ok_or_else(|| GwsError::Discovery("Resource 'files' not found".to_string()))?;
                let create_method = files_res.methods.get("create").ok_or_else(|| {
                    GwsError::Discovery("Method 'files.create' not found".to_string())
                })?;

                // Build metadata
                let metadata = build_metadata(&filename, parent_id.map(|s| s.as_str()));

                let body_str = metadata.to_string();

                let scopes: Vec<&str> = create_method.scopes.iter().map(|s| s.as_str()).collect();
                let (token, auth_method) = match auth::get_token(&scopes).await {
                    Ok(t) => (Some(t), executor::AuthMethod::OAuth),
                    Err(_) if matches.get_flag("dry-run") => (None, executor::AuthMethod::None),
                    Err(e) => return Err(GwsError::Auth(format!("Drive auth failed: {e}"))),
                };

                executor::execute_method(
                    doc,
                    create_method,
                    None,
                    Some(&body_str),
                    token.as_deref(),
                    auth_method,
                    None,
                    Some(executor::UploadSource::File {
                        path: file_path,
                        content_type: None,
                    }),
                    matches.get_flag("dry-run"),
                    &executor::PaginationConfig::default(),
                    None,
                    &crate::helpers::modelarmor::SanitizeMode::Warn,
                    &crate::formatter::OutputFormat::default(),
                    false,
                )
                .await?;

                return Ok(true);
            }
            Ok(false)
        })
    }
}

fn determine_filename(file_path: &str, name_arg: Option<&str>) -> Result<String, GwsError> {
    if let Some(n) = name_arg {
        Ok(n.to_string())
    } else {
        Path::new(file_path)
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_string())
            .ok_or_else(|| GwsError::Validation("Invalid file path".to_string()))
    }
}

fn build_metadata(filename: &str, parent_id: Option<&str>) -> Value {
    let mut metadata = json!({
        "name": filename
    });

    if let Some(parent) = parent_id {
        metadata["parents"] = json!([parent]);
    }

    metadata
}

// ---------------------------------------------------------------------------
// +organize helpers
// ---------------------------------------------------------------------------

const MEET_RECORDINGS_FOLDER_NAME: &str = "Meet Recordings";
const DRIVE_FILES_URL: &str = "https://www.googleapis.com/drive/v3/files";

/// Separators Meet uses between the two parties in a recording filename.
const NAME_SEPARATORS: &[&str] = &[" and ", " & ", " <> "];

/// Aliases for the Drive account owner. Recordings are always between the
/// owner and a client, but the owner can appear on *either* side of the
/// separator ("Josh & BJ" vs "BJ & Johnny"), so the owner is skipped when
/// picking out the client.
const OWNER_NAME_ALIASES: &[&str] = &["bj", "bj hillabush", "hillabush"];

/// Minimum length a client name must have before the loose "client name
/// appears inside the folder name" rule applies. Very short names ("Al",
/// "Jo") are too generic to match safely.
const MIN_CONTAINMENT_LEN: usize = 3;

/// Lowercase a name and split it into whitespace-separated words, dropping
/// *all* punctuation within each word so "O'Reilly," and "OReilly" normalize
/// identically, and Meet's stylized spellings ("mus-tafa-") collapse onto the
/// plain folder name ("Mustafa").
///
/// Punctuation is removed rather than treated as a word boundary, so word
/// count is preserved and `contains_word_run`'s whole-word protection still
/// holds.
fn name_words(name: &str) -> Vec<String> {
    name.to_lowercase()
        .split_whitespace()
        .map(|w| w.chars().filter(|c| c.is_alphanumeric()).collect::<String>())
        .filter(|w| !w.is_empty())
        .collect()
}

/// Normalize a name for owner comparison, so "B.J." collapses to "bj".
fn normalize_owner_key(name: &str) -> String {
    name_words(name).join(" ")
}

fn is_owner_name(name: &str) -> bool {
    let key = normalize_owner_key(name);
    OWNER_NAME_ALIASES.contains(&key.as_str())
}

/// Split a names portion on the first separator found, returning both sides.
fn split_parties(names_part: &str) -> Option<(&str, &str)> {
    NAME_SEPARATORS.iter().find_map(|sep| {
        names_part.find(sep).map(|pos| {
            (
                names_part[..pos].trim(),
                names_part[pos + sep.len()..].trim(),
            )
        })
    })
}

/// True if `needle` appears in `haystack` as a contiguous run of *whole* words.
///
/// This is what makes containment word-boundary aware: ["josh"] does not
/// appear in ["nitin", "joshi"], but ["rohan"] does appear in
/// ["rohan", "recordings"].
fn contains_word_run(haystack: &[String], needle: &[String]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Extract the client name from a Meet recording filename.
///
/// Filenames follow patterns like:
///   "Nicole & B.J. - 2026/04/03 16:00 PDT - Notes by Gemini"
///   "Ayo Ejidiran and BJ Hillabush - 2026/04/03 14:28 PDT - Recording"
///   "Maha <> B.J. - 2026/04/03 12:01 PDT - Notes by Gemini"
///   "BJ & Johnny - 2026/04/03 09:00 PDT - Recording"   (owner listed first)
///
/// Returns the client portion (e.g. "Nicole", "Ayo Ejidiran", "Maha", "Johnny").
fn extract_client_name(filename: &str) -> Option<String> {
    // Split on " - " to isolate the names portion (before the date)
    let names_part = filename.split(" - ").next()?;

    let (left, right) = split_parties(names_part)?;

    // Take whichever party isn't the account owner.
    let client = if !left.is_empty() && !is_owner_name(left) {
        left
    } else if !right.is_empty() && !is_owner_name(right) {
        // The owner led the title, so the client is on the right. Trim any
        // further parties ("BJ and Johnny & Sam" → "Johnny").
        split_parties(right).map(|(l, _)| l).unwrap_or(right)
    } else {
        return None;
    };

    if client.is_empty() {
        None
    } else {
        Some(client.to_string())
    }
}

/// Find the best matching folder for a client name.
///
/// `folders` is a slice of (folder_name, folder_id) pairs.
/// Returns the folder_id of the best match, or None.
fn find_matching_folder<'a>(client_name: &str, folders: &'a [(String, String)]) -> Option<&'a str> {
    let client_words = name_words(client_name);
    if client_words.is_empty() {
        return None;
    }
    let folder_words: Vec<Vec<String>> = folders.iter().map(|(name, _)| name_words(name)).collect();

    // 1. Exact full-name match (case-insensitive): "Matthew O'Reilly" == "Matthew O'Reilly"
    for (i, (_, folder_id)) in folders.iter().enumerate() {
        if folder_words[i] == client_words {
            return Some(folder_id);
        }
    }

    // 2. Full client name contained in folder name (handles "Recordings" suffix)
    //    e.g., "Rohan" in "Rohan recordings".
    //    Matched on whole-word runs, not raw substrings — a raw `contains`
    //    matched "Josh" against "Nitin Joshi" and filed two calls under the
    //    wrong client.
    let client_len: usize = client_words.iter().map(|w| w.chars().count()).sum();
    if client_len >= MIN_CONTAINMENT_LEN {
        for (i, (_, folder_id)) in folders.iter().enumerate() {
            if contains_word_run(&folder_words[i], &client_words) {
                return Some(folder_id);
            }
        }
    }

    // 3. First name match: "Nicole" in "Nicole Inouye", "Frankie" in "Frankie Recordings"
    //    Checked before last-name to avoid false positives like "Frankie Johnson" → "Kwame Johnson".
    let first_name = &client_words[0];
    // Skip very short names (<=2 chars) to avoid false matches
    if first_name.chars().count() > 2 {
        for (i, (_, folder_id)) in folders.iter().enumerate() {
            // Exact first-name match or starts-with (e.g., "Ayo" matches "Ayodeji")
            if let Some(folder_first) = folder_words[i].first() {
                if folder_first == first_name || folder_first.starts_with(first_name.as_str()) {
                    return Some(folder_id);
                }
            }
        }
    }

    // 4. Last name match: "Ejidiran" in "Ayodeji Ejidiran"
    if client_words.len() >= 2 {
        let last_name = client_words.last().unwrap();
        for (i, (_, folder_id)) in folders.iter().enumerate() {
            if folder_words[i].iter().any(|w| w == last_name) {
                return Some(folder_id);
            }
        }
    }

    None
}

async fn get_json(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    query: &[(&str, &str)],
) -> Result<Value, GwsError> {
    let resp = client
        .get(url)
        .query(query)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| GwsError::Other(anyhow::anyhow!("HTTP request failed: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(GwsError::Api {
            code: status.as_u16(),
            message: body,
            reason: "organize_request_failed".to_string(),
            enable_url: None,
        });
    }

    resp.json::<Value>()
        .await
        .map_err(|e| GwsError::Other(anyhow::anyhow!("JSON parse failed: {e}")))
}

/// List all files matching a query, paginating automatically.
async fn list_all_files(
    client: &reqwest::Client,
    token: &str,
    query: &str,
    fields: &str,
) -> Result<Vec<Value>, GwsError> {
    let mut all_files = Vec::new();
    let mut page_token: Option<String> = None;

    loop {
        let mut params: Vec<(&str, &str)> = vec![
            ("q", query),
            ("pageSize", "100"),
            ("fields", fields),
        ];
        let pt;
        if let Some(ref tok) = page_token {
            pt = tok.clone();
            params.push(("pageToken", &pt));
        }

        let resp = get_json(client, DRIVE_FILES_URL, token, &params).await?;

        if let Some(files) = resp["files"].as_array() {
            all_files.extend(files.iter().cloned());
        }

        match resp["nextPageToken"].as_str() {
            Some(tok) => page_token = Some(tok.to_string()),
            None => break,
        }
    }

    Ok(all_files)
}

async fn handle_organize(matches: &ArgMatches) -> Result<(), GwsError> {
    let dry_run = matches.get_flag("dry-run");

    // Auth
    let scopes = vec!["https://www.googleapis.com/auth/drive"];
    let token = auth::get_token(&scopes)
        .await
        .map_err(|e| GwsError::Auth(format!("Drive auth failed: {e}")))?;

    let client = google_workspace::client::shared_client()?;

    // Step 1: Find "Meet Recordings" folder
    let folder_query = format!(
        "name='{}' and mimeType='application/vnd.google-apps.folder' and trashed=false",
        MEET_RECORDINGS_FOLDER_NAME
    );
    let folder_results = list_all_files(
        &client,
        &token,
        &folder_query,
        "files(id,name)",
    )
    .await?;

    let meet_folder_id = folder_results
        .first()
        .and_then(|f| f["id"].as_str())
        .ok_or_else(|| {
            GwsError::Other(anyhow::anyhow!(
                "Could not find a '{}' folder in your Drive",
                MEET_RECORDINGS_FOLDER_NAME
            ))
        })?
        .to_string();

    eprintln!("Found '{}' folder: {}", MEET_RECORDINGS_FOLDER_NAME, meet_folder_id);

    // Step 2: List client subfolders
    let subfolder_query = format!(
        "'{}' in parents and mimeType='application/vnd.google-apps.folder' and trashed=false",
        meet_folder_id
    );
    let subfolder_results = list_all_files(
        &client,
        &token,
        &subfolder_query,
        "files(id,name),nextPageToken",
    )
    .await?;

    let folders: Vec<(String, String)> = subfolder_results
        .iter()
        .filter_map(|f| {
            let name = f["name"].as_str()?.to_string();
            let id = f["id"].as_str()?.to_string();
            Some((name, id))
        })
        .collect();

    eprintln!("Found {} client folders", folders.len());

    // Step 3: List loose (non-folder) files
    let files_query = format!(
        "'{}' in parents and mimeType!='application/vnd.google-apps.folder' and trashed=false",
        meet_folder_id
    );
    let loose_files = list_all_files(
        &client,
        &token,
        &files_query,
        "files(id,name,mimeType,parents),nextPageToken",
    )
    .await?;

    eprintln!("Found {} loose files to organize\n", loose_files.len());

    if loose_files.is_empty() {
        println!("{}", json!({"message": "No loose files to organize", "status": "ok"}));
        return Ok(());
    }

    // Step 4: Match files to folders
    let mut moves: Vec<(String, String, String, String)> = Vec::new(); // (file_id, file_name, folder_id, folder_name)
    let mut unmatched: Vec<String> = Vec::new();

    for file in &loose_files {
        let file_name = file["name"].as_str().unwrap_or("");
        let file_id = file["id"].as_str().unwrap_or("");

        if let Some(client_name) = extract_client_name(file_name) {
            if let Some(folder_id) = find_matching_folder(&client_name, &folders) {
                let folder_name = folders
                    .iter()
                    .find(|(_, id)| id == folder_id)
                    .map(|(name, _)| name.as_str())
                    .unwrap_or("?");
                moves.push((
                    file_id.to_string(),
                    file_name.to_string(),
                    folder_id.to_string(),
                    folder_name.to_string(),
                ));
            } else {
                unmatched.push(file_name.to_string());
            }
        } else {
            unmatched.push(file_name.to_string());
        }
    }

    // Step 5: Print summary
    let summary = json!({
        "matched": moves.len(),
        "unmatched": unmatched.len(),
        "dry_run": dry_run,
        "moves": moves.iter().map(|(_, name, _, folder)| {
            json!({"file": name, "destination": folder})
        }).collect::<Vec<_>>(),
        "unmatched_files": unmatched,
    });
    println!("{}", serde_json::to_string_pretty(&summary)
        .unwrap_or_else(|_| summary.to_string()));

    if dry_run {
        eprintln!("\nDry run — no files were moved.");
        return Ok(());
    }

    // Step 6: Execute moves
    let mut moved = 0u32;
    let mut errors = 0u32;

    for (file_id, file_name, target_folder_id, target_folder_name) in &moves {
        let encoded_id = encode_path_segment(file_id);
        let url = format!("{}/{}",DRIVE_FILES_URL, encoded_id);

        let resp = match client
            .patch(&url)
            .query(&[
                ("addParents", target_folder_id.as_str()),
                ("removeParents", meet_folder_id.as_str()),
                ("fields", "id,name,parents"),
            ])
            .bearer_auth(&token)
            .send()
            .await
        {
            Ok(resp) => resp,
            // A transient network failure on one file must not abandon the
            // rest of the batch — moves are independent, and the run is
            // unattended. Record it and carry on; the file stays put and is
            // retried on the next run.
            Err(e) => {
                errors += 1;
                eprintln!("  Error moving {}: request failed: {}", file_name, e);
                continue;
            }
        };

        if resp.status().is_success() {
            moved += 1;
            eprintln!("  Moved: {} -> {}", file_name, target_folder_name);
        } else {
            errors += 1;
            let body = resp.text().await.unwrap_or_default();
            eprintln!("  Error moving {}: {}", file_name, body);
        }
    }

    let result = json!({
        "status": "done",
        "moved": moved,
        "errors": errors,
        "unmatched": unmatched.len(),
    });
    println!("{}", serde_json::to_string_pretty(&result)
        .unwrap_or_else(|_| result.to_string()));

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_determine_filename_explicit() {
        assert_eq!(
            determine_filename("path/to/file.txt", Some("custom.txt")).unwrap(),
            "custom.txt"
        );
    }

    #[test]
    fn test_determine_filename_from_path() {
        assert_eq!(
            determine_filename("path/to/file.txt", None).unwrap(),
            "file.txt"
        );
    }

    #[test]
    fn test_determine_filename_invalid_path() {
        assert!(determine_filename("", None).is_err());
        assert!(determine_filename("/", None).is_err()); // Root has no filename component usually
    }

    #[test]
    fn test_build_metadata_no_parent() {
        let meta = build_metadata("file.txt", None);
        assert_eq!(meta["name"], "file.txt");
        assert!(meta.get("parents").is_none());
    }

    #[test]
    fn test_build_metadata_with_parent() {
        let meta = build_metadata("file.txt", Some("folder123"));
        assert_eq!(meta["name"], "file.txt");
        assert_eq!(meta["parents"][0], "folder123");
    }

    // --- extract_client_name ---

    #[test]
    fn test_extract_name_and_separator() {
        assert_eq!(
            extract_client_name("Ayo Ejidiran and BJ Hillabush - 2026/04/03 14:28 PDT - Recording"),
            Some("Ayo Ejidiran".to_string())
        );
    }

    #[test]
    fn test_extract_name_ampersand_separator() {
        assert_eq!(
            extract_client_name("Nicole & B.J. - 2026/04/03 16:00 PDT - Notes by Gemini"),
            Some("Nicole".to_string())
        );
    }

    #[test]
    fn test_extract_name_angle_separator() {
        assert_eq!(
            extract_client_name("Maha <> B.J. - 2026/04/03 12:01 PDT - Notes by Gemini"),
            Some("Maha".to_string())
        );
    }

    #[test]
    fn test_extract_name_ampersand_no_dot() {
        assert_eq!(
            extract_client_name("Brian & BJ - 2026/04/03 10:29 PDT - Recording"),
            Some("Brian".to_string())
        );
    }

    #[test]
    fn test_extract_name_transcript_suffix() {
        assert_eq!(
            extract_client_name("Georgina and BJ Hillabush - 2026/03/31 14:56 PDT - Transcript"),
            Some("Georgina".to_string())
        );
    }

    #[test]
    fn test_extract_name_no_separator_returns_none() {
        assert_eq!(extract_client_name("Some random file.mp4"), None);
    }

    #[test]
    fn test_extract_name_owner_listed_first() {
        assert_eq!(
            extract_client_name("BJ & Johnny - 2026/08/20 15:00 PDT - Recording"),
            Some("Johnny".to_string())
        );
    }

    #[test]
    fn test_extract_name_owner_listed_first_with_dots() {
        assert_eq!(
            extract_client_name("B.J. and Nitin Joshi - 2026/08/20 15:00 PDT - Transcript"),
            Some("Nitin Joshi".to_string())
        );
    }

    #[test]
    fn test_extract_name_owner_full_name_first() {
        assert_eq!(
            extract_client_name("BJ Hillabush <> Maha - 2026/04/03 12:01 PDT - Notes by Gemini"),
            Some("Maha".to_string())
        );
    }

    #[test]
    fn test_extract_name_owner_first_three_parties() {
        assert_eq!(
            extract_client_name("BJ and Johnny & Sam - 2026/08/20 15:00 PDT - Recording"),
            Some("Johnny".to_string())
        );
    }

    #[test]
    fn test_extract_name_both_parties_owner_returns_none() {
        assert_eq!(
            extract_client_name("BJ & B.J. - 2026/08/20 15:00 PDT - Recording"),
            None
        );
    }

    #[test]
    fn test_extract_name_client_named_first_still_wins() {
        // The owner-skip must not change existing behaviour when the client leads.
        assert_eq!(
            extract_client_name("Josh & BJ - 2026/08/20 15:00 PDT - Recording"),
            Some("Josh".to_string())
        );
    }

    #[test]
    fn test_is_owner_name_variants() {
        assert!(is_owner_name("BJ"));
        assert!(is_owner_name("bj"));
        assert!(is_owner_name("B.J."));
        assert!(is_owner_name("BJ Hillabush"));
        assert!(is_owner_name("B.J. Hillabush"));
        assert!(!is_owner_name("Josh"));
        assert!(!is_owner_name("BJorn"));
    }

    // --- find_matching_folder ---

    fn test_folders() -> Vec<(String, String)> {
        vec![
            ("Nicole Inouye".to_string(), "folder_nicole".to_string()),
            ("Ayodeji Ejidiran".to_string(), "folder_ayo".to_string()),
            ("Matthew O'Reilly".to_string(), "folder_matt".to_string()),
            ("Brian Bardi".to_string(), "folder_brian".to_string()),
            ("Maha Al Khater".to_string(), "folder_maha".to_string()),
            ("Kerri Clifford".to_string(), "folder_kerri".to_string()),
            ("Rohan recordings".to_string(), "folder_rohan".to_string()),
            ("Viktoriia Uskova".to_string(), "folder_vik".to_string()),
            ("Craig Slater".to_string(), "folder_craig".to_string()),
            ("Nitin Joshi".to_string(), "folder_nitin".to_string()),
            ("Wil Van Auken".to_string(), "folder_wil".to_string()),
            ("Mustafa".to_string(), "folder_mustafa".to_string()),
            ("Karin Cross-Smith".to_string(), "folder_karin".to_string()),
        ]
    }

    #[test]
    fn test_match_exact_full_name() {
        let folders = test_folders();
        assert_eq!(
            find_matching_folder("Matthew O'Reilly", &folders),
            Some("folder_matt")
        );
    }

    #[test]
    fn test_match_first_name() {
        let folders = test_folders();
        assert_eq!(
            find_matching_folder("Nicole", &folders),
            Some("folder_nicole")
        );
    }

    #[test]
    fn test_match_last_name() {
        let folders = test_folders();
        assert_eq!(
            find_matching_folder("Ayo Ejidiran", &folders),
            Some("folder_ayo")
        );
    }

    #[test]
    fn test_match_first_name_starts_with() {
        let folders = test_folders();
        // "Ayo" should match "Ayodeji Ejidiran" via starts-with on first name
        assert_eq!(
            find_matching_folder("Ayo", &folders),
            Some("folder_ayo")
        );
    }

    #[test]
    fn test_match_folder_with_recordings_suffix() {
        let folders = test_folders();
        assert_eq!(
            find_matching_folder("Rohan", &folders),
            Some("folder_rohan")
        );
    }

    #[test]
    fn test_match_maha() {
        let folders = test_folders();
        assert_eq!(
            find_matching_folder("Maha", &folders),
            Some("folder_maha")
        );
    }

    #[test]
    fn test_no_match_returns_none() {
        let folders = test_folders();
        assert_eq!(find_matching_folder("Unknown Person", &folders), None);
    }

    #[test]
    fn test_match_case_insensitive() {
        let folders = test_folders();
        assert_eq!(
            find_matching_folder("craig slater", &folders),
            Some("folder_craig")
        );
    }

    // --- regression: substring containment false positives ---

    #[test]
    fn test_short_first_name_does_not_substring_match_longer_surname() {
        // Regression: "josh" is a substring of "nitin joshi", which filed two
        // 2026-08-20 recordings under the wrong client folder.
        let folders = test_folders();
        assert_eq!(find_matching_folder("Josh", &folders), None);
    }

    #[test]
    fn test_josh_recording_filename_end_to_end_is_unmatched() {
        let folders = test_folders();
        let client =
            extract_client_name("Josh & BJ - 2026/08/20 15:00 PDT - Recording").unwrap();
        assert_eq!(client, "Josh");
        assert_eq!(find_matching_folder(&client, &folders), None);
    }

    #[test]
    fn test_containment_still_matches_whole_word() {
        // The word-boundary rule must not break the "Recordings" suffix case.
        let folders = test_folders();
        assert_eq!(find_matching_folder("Rohan", &folders), Some("folder_rohan"));
    }

    #[test]
    fn test_full_name_variant_resolves_via_last_name() {
        // "Wilhem Van Auken" -> "Wil Van Auken" (first names differ, surname matches)
        let folders = test_folders();
        assert_eq!(
            find_matching_folder("Wilhem Van Auken", &folders),
            Some("folder_wil")
        );
    }

    #[test]
    fn test_multi_word_containment_matches_word_run() {
        let folders = test_folders();
        assert_eq!(
            find_matching_folder("Van Auken", &folders),
            Some("folder_wil")
        );
    }

    #[test]
    fn test_short_name_skips_containment_rule() {
        // "Al" is a whole word inside "Maha Al Khater" but is too short to
        // match on safely.
        let folders = test_folders();
        assert_eq!(find_matching_folder("Al", &folders), None);
    }

    #[test]
    fn test_nitin_joshi_still_matches_its_own_folder() {
        let folders = test_folders();
        assert_eq!(
            find_matching_folder("Nitin Joshi", &folders),
            Some("folder_nitin")
        );
    }

    #[test]
    fn test_match_interior_punctuation_in_client_name() {
        // Meet wrote this client as "mus-tafa-"; the folder is "Mustafa".
        // Interior punctuation must not block the match.
        let folders = test_folders();
        assert_eq!(
            find_matching_folder("mus-tafa-", &folders),
            Some("folder_mustafa")
        );
    }

    #[test]
    fn test_match_interior_punctuation_end_to_end() {
        let client =
            extract_client_name("mus-tafa- and bj - 2026/07/17 09:30 PDT - Recording").unwrap();
        assert_eq!(client, "mus-tafa-");
        let folders = test_folders();
        assert_eq!(
            find_matching_folder(&client, &folders),
            Some("folder_mustafa")
        );
    }

    #[test]
    fn test_match_hyphenated_folder_name() {
        // Punctuation is stripped on the folder side too, so a plainly-spelled
        // client name still reaches its hyphenated folder.
        let folders = test_folders();
        assert_eq!(
            find_matching_folder("Karin CrossSmith", &folders),
            Some("folder_karin")
        );
    }

    #[test]
    fn test_punctuation_stripping_does_not_break_word_boundaries() {
        // Guards the original false positive: stripping punctuation must not
        // merge words, or ["josh"] would start matching "Nitin Joshi".
        let folders = test_folders();
        assert_eq!(find_matching_folder("Josh", &folders), None);
        assert_eq!(name_words("mus-tafa-"), vec!["mustafa".to_string()]);
        assert_eq!(
            name_words("Karin Cross-Smith"),
            vec!["karin".to_string(), "crosssmith".to_string()]
        );
    }

    #[test]
    fn test_empty_client_name_returns_none() {
        let folders = test_folders();
        assert_eq!(find_matching_folder("   ", &folders), None);
        assert_eq!(find_matching_folder("...", &folders), None);
    }

    // --- word helpers ---

    #[test]
    fn test_contains_word_run() {
        let haystack: Vec<String> = vec!["nitin".into(), "joshi".into()];
        assert!(!contains_word_run(&haystack, &["josh".to_string()]));
        assert!(contains_word_run(&haystack, &["joshi".to_string()]));

        let rohan: Vec<String> = vec!["rohan".into(), "recordings".into()];
        assert!(contains_word_run(&rohan, &["rohan".to_string()]));

        // needle longer than haystack
        assert!(!contains_word_run(
            &rohan,
            &["rohan".to_string(), "recordings".to_string(), "x".to_string()]
        ));
        assert!(!contains_word_run(&rohan, &[]));
    }

    #[test]
    fn test_name_words_strips_punctuation() {
        // Punctuation is dropped everywhere in a word, not just at its edges,
        // so both sides of a comparison normalize the same way regardless of
        // how Meet or the folder spelled the name.
        assert_eq!(name_words("Matthew O'Reilly"), vec!["matthew", "oreilly"]);
        assert_eq!(name_words("  Rohan,  "), vec!["rohan"]);
        assert!(name_words("--").is_empty());
        // Word count is preserved — punctuation is not a word boundary.
        assert_eq!(name_words("Cross-Smith").len(), 1);
    }
}
