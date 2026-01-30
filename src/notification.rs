//! Native Windows toast notifications using WinRT
//!
//! Replaces the old PowerShell approach with native Windows APIs for:
//! - ~10ms instead of 200-500ms latency
//! - No PowerShell process spawn overhead
//! - Proper app identity support

#[cfg(windows)]
use windows::{
    core::HSTRING,
    Data::Xml::Dom::XmlDocument,
    UI::Notifications::{ToastNotification, ToastNotificationManager},
};
use tracing::debug;

/// Notification payload received from MQTT
#[derive(serde::Deserialize, Default, Debug)]
pub struct NotificationPayload {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub message: String,
}

impl NotificationPayload {
    /// Parse notification payload from JSON or plain text
    pub fn from_payload(payload: &str) -> Self {
        serde_json::from_str(payload).unwrap_or_else(|_| {
            Self {
                title: String::new(),
                message: payload.to_string(),
            }
        })
    }
}

/// Show a native Windows toast notification
#[cfg(windows)]
pub fn show_toast(payload: &str) -> anyhow::Result<()> {
    let notif = NotificationPayload::from_payload(payload);
    
    let title = if notif.title.is_empty() { "Home Assistant" } else { &notif.title };
    let message = if notif.message.is_empty() { payload } else { &notif.message };
    
    // Escape XML special characters
    let title = escape_xml(title);
    let message = escape_xml(message);
    
    // Build toast XML template
    let toast_xml = format!(
        r#"<toast>
            <visual>
                <binding template="ToastText02">
                    <text id="1">{}</text>
                    <text id="2">{}</text>
                </binding>
            </visual>
        </toast>"#,
        title, message
    );
    
    // Create XML document
    let xml_doc = XmlDocument::new()?;
    xml_doc.LoadXml(&HSTRING::from(&toast_xml))?;
    
    // Create toast notification
    let toast = ToastNotification::CreateToastNotification(&xml_doc)?;
    
    // Use PowerShell's AUMID as app identity (works without app registration)
    let app_id = HSTRING::from("{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\\WindowsPowerShell\\v1.0\\powershell.exe");
    let notifier = ToastNotificationManager::CreateToastNotifierWithId(&app_id)?;
    
    notifier.Show(&toast)?;
    
    debug!("Toast notification sent: {} - {}", title, message);
    Ok(())
}

/// Fallback for non-Windows platforms (no-op)
#[cfg(not(windows))]
pub fn show_toast(payload: &str) -> anyhow::Result<()> {
    let notif = NotificationPayload::from_payload(payload);
    let title = if notif.title.is_empty() { "Home Assistant" } else { &notif.title };
    let message = if notif.message.is_empty() { payload } else { &notif.message };
    
    // On Linux, could use notify-send or libnotify in the future
    tracing::info!("Notification (not shown on this platform): {} - {}", title, message);
    Ok(())
}

/// Escape XML special characters and strip control chars
fn escape_xml(s: &str) -> String {
    s.chars()
        .filter(|&c| c >= '\x20' || c == '\t' || c == '\n' || c == '\r')
        .map(|c| match c {
            '&' => "&amp;".to_string(),
            '<' => "&lt;".to_string(),
            '>' => "&gt;".to_string(),
            '\'' => "&apos;".to_string(),
            '"' => "&quot;".to_string(),
            _ => c.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_payload_parsing_json() {
        let json = r#"{"title": "Test", "message": "Hello world"}"#;
        let payload = NotificationPayload::from_payload(json);
        assert_eq!(payload.title, "Test");
        assert_eq!(payload.message, "Hello world");
    }
    
    #[test]
    fn test_payload_parsing_plain_text() {
        let text = "Just a plain message";
        let payload = NotificationPayload::from_payload(text);
        assert_eq!(payload.title, "");
        assert_eq!(payload.message, "Just a plain message");
    }
    
    #[test]
    fn test_xml_escaping() {
        assert_eq!(escape_xml("Hello & World"), "Hello &amp; World");
        assert_eq!(escape_xml("<script>"), "&lt;script&gt;");
    }
}
