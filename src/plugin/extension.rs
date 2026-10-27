//! Declarative extension plans.
//!
//! An extension never draws and never mutates the store. It returns one of
//! these plans, and the host validates it before the UI sees it. A malformed
//! plan is rejected as a whole so a broken extension cannot hide or duplicate
//! mail in the list.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

/// The organizer's complete answer for one list revision.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OrganizerPlan {
    #[serde(default)]
    pub groups: Vec<OrganizerGroup>,
    #[serde(default)]
    pub row_overrides: Vec<RowOverride>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OrganizerGroup {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub subtitle: Option<String>,
    #[serde(default)]
    pub message_ids: Vec<String>,
    #[serde(default)]
    pub order: i64,
    #[serde(default)]
    pub collapsed: bool,
    #[serde(default)]
    pub badges: Vec<Badge>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RowOverride {
    pub message_id: String,
    #[serde(default)]
    pub badges: Vec<Badge>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Badge {
    pub text: String,
    #[serde(default)]
    pub tone: String,
}

impl OrganizerPlan {
    /// Every message must appear exactly once. Unknown ids are errors here:
    /// the host may drop and warn, but it must not pass a plan that could
    /// hide mail to the list.
    pub fn validate(&self, input_ids: &[String]) -> Result<(), String> {
        if self.groups.len() > 500 {
            return Err(format!("too many groups: {}", self.groups.len()));
        }
        let input: HashSet<&str> = input_ids.iter().map(String::as_str).collect();
        let mut seen = HashSet::new();
        let mut placements = 0usize;
        for group in &self.groups {
            if group.id.is_empty() || group.title.is_empty() {
                return Err("a group needs a non-empty id and title".to_string());
            }
            for message in &group.message_ids {
                placements += 1;
                if !input.contains(message.as_str()) {
                    return Err(format!("unknown message id {message}"));
                }
                if !seen.insert(message.as_str()) {
                    return Err(format!("message {message} appears twice"));
                }
            }
        }
        if placements > 5_000 {
            return Err(format!("too many message placements: {placements}"));
        }
        if seen.len() != input.len() {
            return Err(format!(
                "plan covers {} of {} messages",
                seen.len(),
                input.len()
            ));
        }
        for row in &self.row_overrides {
            if !input.contains(row.message_id.as_str()) {
                return Err(format!("unknown row override {}", row.message_id));
            }
        }
        Ok(())
    }
}

/// The display hook's answer. A missing head or content means "keep the
/// host's own rendering".
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DisplayPlan {
    #[serde(default)]
    pub head: Option<HeadPlan>,
    #[serde(default)]
    pub content: Option<ContentPlan>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HeadPlan {
    #[serde(default)]
    pub sections: Vec<HeadSection>,
    #[serde(default)]
    pub actions: Vec<HeadAction>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HeadSection {
    pub id: String,
    #[serde(default = "default_op")]
    pub op: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub rows: Vec<HeadRow>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HeadRow {
    Text {
        label: String,
        value: String,
        #[serde(default)]
        tone: String,
    },
    Badge {
        text: String,
        #[serde(default)]
        tone: String,
    },
    Link {
        label: String,
        href: String,
    },
    Button {
        label: String,
        #[serde(default)]
        icon: String,
        operation: String,
        #[serde(default)]
        params: serde_json::Value,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HeadAction {
    pub label: String,
    #[serde(default)]
    pub icon: String,
    pub operation: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContentPlan {
    pub mode: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub html: Option<String>,
    #[serde(default)]
    pub parts: Vec<ContentPart>,
    #[serde(default)]
    pub remote: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text {
        text: String,
    },
    Html {
        html: String,
        #[serde(default)]
        remote: Option<String>,
    },
    Notice {
        text: String,
        #[serde(default)]
        tone: String,
    },
}

impl DisplayPlan {
    pub fn validate(&self) -> Result<(), String> {
        if let Some(head) = &self.head {
            if head.sections.len() > 50 || head.actions.len() > 100 {
                return Err("display plan is too large".to_string());
            }
            for section in &head.sections {
                if section.id.is_empty() {
                    return Err("a head section needs an id".to_string());
                }
                if !matches!(
                    section.op.as_str(),
                    "replace" | "append" | "prepend" | "remove"
                ) {
                    return Err(format!("unknown section op {}", section.op));
                }
            }
            for action in &head.actions {
                if action.label.is_empty() || action.operation.is_empty() {
                    return Err("an action needs a label and an operation".to_string());
                }
            }
        }
        if let Some(content) = &self.content {
            if content.parts.len() > 500 {
                return Err("too many content parts".to_string());
            }
            if !matches!(content.mode.as_str(), "keep" | "text" | "html" | "parts") {
                return Err(format!("unknown content mode {}", content.mode));
            }
            if let Some(remote) = &content.remote {
                if !matches!(remote.as_str(), "host" | "block") {
                    return Err(format!("unknown remote policy {remote}"));
                }
            }
        }
        Ok(())
    }
}

fn default_op() -> String {
    "replace".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> Vec<String> {
        vec!["m1".to_string(), "m2".to_string()]
    }

    #[test]
    fn organizer_must_place_every_message_once() {
        let good: OrganizerPlan = serde_json::from_value(serde_json::json!({
            "groups": [{
                "id": "thread:t1",
                "title": "Thread",
                "message_ids": ["m1", "m2"],
                "order": 0
            }]
        }))
        .unwrap();
        assert!(good.validate(&ids()).is_ok());

        let missing: OrganizerPlan = serde_json::from_value(serde_json::json!({
            "groups": [{ "id": "thread:t1", "title": "Thread", "message_ids": ["m1"] }]
        }))
        .unwrap();
        assert!(missing.validate(&ids()).is_err());

        let duplicate: OrganizerPlan = serde_json::from_value(serde_json::json!({
            "groups": [{
                "id": "thread:t1",
                "title": "Thread",
                "message_ids": ["m1", "m1"]
            }]
        }))
        .unwrap();
        assert!(duplicate.validate(&ids()).is_err());
    }

    #[test]
    fn display_plan_is_bounded_and_typed() {
        let plan: DisplayPlan = serde_json::from_value(serde_json::json!({
            "head": {
                "sections": [{
                    "id": "security",
                    "op": "append",
                    "rows": [{ "type": "badge", "text": "External", "tone": "warning" }]
                }]
            },
            "content": { "mode": "keep" }
        }))
        .unwrap();
        assert!(plan.validate().is_ok());

        let bad: DisplayPlan = serde_json::from_value(serde_json::json!({
            "content": { "mode": "paint" }
        }))
        .unwrap();
        assert!(bad.validate().is_err());
    }
}
