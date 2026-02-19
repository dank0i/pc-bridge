//! Text VDF parser - minimal zero-copy implementation
//!
//! Parses libraryfolders.vdf and appmanifest_*.acf files.

/// Fast path: extract specific keys from appmanifest without full parse
#[inline]
pub fn extract_appmanifest_fields(content: &str) -> Option<(u32, String, String)> {
    let mut appid = None;
    let mut name = None;
    let mut installdir = None;

    for line in content.lines() {
        let line = line.trim();
        if line.starts_with("\"appid\"") {
            appid = extract_quoted_value(line);
        } else if line.starts_with("\"name\"") {
            name = extract_quoted_string(line);
        } else if line.starts_with("\"installdir\"") {
            installdir = extract_quoted_string(line);
        }

        // Early exit if we have all fields
        if appid.is_some() && name.is_some() && installdir.is_some() {
            break;
        }
    }

    Some((appid?.parse().ok()?, name?, installdir?))
}

/// Extract library paths from libraryfolders.vdf
#[cfg(test)]
pub fn extract_library_paths(content: &str) -> Vec<String> {
    extract_library_info(content)
        .into_iter()
        .map(|(path, _)| path)
        .collect()
}

/// Extract library paths AND app_ids from libraryfolders.vdf
///
/// Returns: Vec<(library_path, Vec<app_id>)>
///
/// libraryfolders.vdf format:
/// ```
/// "libraryfolders"
/// {
///     "0"
///     {
///         "path"    "C:\\Program Files (x86)\\Steam"
///         "apps"
///         {
///             "228980"    "123456789"  // app_id -> install size
///             "730"       "23456789"
///         }
///     }
/// }
/// ```
pub fn extract_library_info(content: &str) -> Vec<(String, Vec<u32>)> {
    use tracing::debug;

    let mut libraries = Vec::new();
    let mut current_path: Option<String> = None;
    let mut current_apps: Vec<u32> = Vec::new();
    let mut in_library_block = false;
    let mut in_apps_block = false;
    let mut brace_depth: i32 = 0;

    debug!("Parsing libraryfolders.vdf ({} bytes)", content.len());

    for line in content.lines() {
        let line = line.trim();

        if line == "{" {
            brace_depth += 1;
            continue;
        } else if line == "}" {
            brace_depth -= 1;

            if in_apps_block && brace_depth == 2 {
                // End of apps block
                debug!("  End apps block, got {} app_ids", current_apps.len());
                in_apps_block = false;
            } else if in_library_block && brace_depth == 1 {
                // End of library block - save it
                if let Some(path) = current_path.take() {
                    debug!(
                        "  Library block end: path={}, apps={}",
                        path,
                        current_apps.len()
                    );
                    libraries.push((path, std::mem::take(&mut current_apps)));
                }
                in_library_block = false;
            }
            continue;
        }

        // Check for numbered library blocks ("0", "1", etc.)
        if brace_depth == 1 && line.starts_with('"') && line.ends_with('"') {
            let key = &line[1..line.len() - 1];
            if key.chars().all(|c| c.is_ascii_digit()) {
                debug!("  Found library block: {}", key);
                in_library_block = true;
            }
        }

        if in_library_block && brace_depth == 2 {
            if line.starts_with("\"path\"") {
                current_path = extract_quoted_string(line);
                debug!("    path: {:?}", current_path);
            } else if line.starts_with("\"apps\"") {
                in_apps_block = true;
            }
        }

        // Inside apps block - extract app_ids (the KEY, not the value)
        if in_apps_block && brace_depth == 3 {
            // Lines look like: "730"  "62017550958" (app_id, size_on_disk)
            // We want the first quoted string (the key/app_id)
            if let Some(app_id_str) = extract_first_quoted(line) {
                if let Ok(app_id) = app_id_str.parse::<u32>() {
                    current_apps.push(app_id);
                }
            }
        }
    }

    debug!("Parsed {} library folders", libraries.len());
    libraries
}

/// Extract the first quoted string from a line (the key)
#[inline]
fn extract_first_quoted(line: &str) -> Option<&str> {
    let start = line.find('"')? + 1;
    let end = start + line[start..].find('"')?;
    Some(&line[start..end])
}

/// Extract the second quoted string from a line (the value)
#[inline]
fn extract_quoted_value(line: &str) -> Option<&str> {
    let mut parts = line.split('"');
    parts.next()?; // skip before first quote
    parts.next()?; // skip key
    parts.next()?; // skip between quotes
    parts.next() // value
}

#[inline]
fn extract_quoted_string(line: &str) -> Option<String> {
    extract_quoted_value(line).map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_appmanifest() {
        let content = r#"
"AppState"
{
    "appid"        "730"
    "name"         "Counter-Strike 2"
    "installdir"   "Counter-Strike Global Offensive"
    "StateFlags"   "4"
}
"#;
        let (id, name, dir) = extract_appmanifest_fields(content).unwrap();
        assert_eq!(id, 730);
        assert_eq!(name, "Counter-Strike 2");
        assert_eq!(dir, "Counter-Strike Global Offensive");
    }

    #[test]
    fn test_extract_appmanifest_unordered_fields() {
        // Fields can appear in any order
        let content = r#"
"AppState"
{
    "StateFlags"   "4"
    "installdir"   "Cyberpunk 2077"
    "name"         "Cyberpunk 2077"
    "appid"        "1091500"
}
"#;
        let (id, name, dir) = extract_appmanifest_fields(content).unwrap();
        assert_eq!(id, 1091500);
        assert_eq!(name, "Cyberpunk 2077");
        assert_eq!(dir, "Cyberpunk 2077");
    }

    #[test]
    fn test_extract_appmanifest_missing_field() {
        let content = r#"
"AppState"
{
    "appid"        "730"
    "name"         "Counter-Strike 2"
}
"#;
        assert!(extract_appmanifest_fields(content).is_none());
    }

    #[test]
    fn test_extract_appmanifest_invalid_appid() {
        let content = r#"
"AppState"
{
    "appid"        "not_a_number"
    "name"         "Test"
    "installdir"   "Test"
}
"#;
        assert!(extract_appmanifest_fields(content).is_none());
    }

    #[test]
    fn test_extract_library_paths() {
        let content = r#"
"libraryfolders"
{
    "0"
    {
        "path"        "C:\\Program Files (x86)\\Steam"
        "label"       ""
        "apps"
        {
            "228980"    "62389745"
            "730"       "31233856512"
        }
    }
    "1"
    {
        "path"        "D:\\SteamLibrary"
        "label"       ""
        "apps"
        {
            "1091500"    "73246892134"
        }
    }
}
"#;
        let paths = extract_library_paths(content);
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0], r"C:\\Program Files (x86)\\Steam");
        assert_eq!(paths[1], r"D:\\SteamLibrary");
    }

    #[test]
    fn test_extract_library_info_with_apps() {
        let content = r#"
"libraryfolders"
{
    "0"
    {
        "path"        "C:\\Steam"
        "apps"
        {
            "730"       "123"
            "1091500"   "456"
        }
    }
}
"#;
        let info = extract_library_info(content);
        assert_eq!(info.len(), 1);
        assert_eq!(info[0].0, r"C:\\Steam");
        assert_eq!(info[0].1, vec![730, 1091500]);
    }

    #[test]
    fn test_extract_first_quoted() {
        assert_eq!(extract_first_quoted("\"key\"  \"value\""), Some("key"));
        assert_eq!(extract_first_quoted("\"730\"  \"123456\""), Some("730"));
        assert_eq!(extract_first_quoted("no quotes"), None);
        assert_eq!(extract_first_quoted("\"only_one"), None);
    }

    #[test]
    fn test_extract_quoted_value() {
        assert_eq!(extract_quoted_value("\"key\"  \"value\""), Some("value"));
        assert_eq!(
            extract_quoted_value("\"appid\"        \"730\""),
            Some("730")
        );
        assert_eq!(extract_quoted_value("\"key\""), None);
    }

    #[test]
    fn test_empty_content() {
        assert!(extract_appmanifest_fields("").is_none());
        assert!(extract_library_paths("").is_empty());
        assert!(extract_library_info("").is_empty());
    }
}
